//! 完整内容身份只使用发布档案中的相对路径、类型、长度、执行要求和 SHA-256。
//! inode 与变更时间仅作本机复查缓存失效依据，不进入跨卷比对的版本摘要。
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::catalog::Artifact;
use super::integrity::{hash, invalid};
use sha2::{Digest, Sha256};

struct Entry<'a> {
    dir: bool,
    size: u64,
    executable: bool,
    sha: &'a str,
}

fn entries(pkg: Artifact) -> BTreeMap<&'static str, Entry<'static>> {
    pkg.manifest
        .lines()
        .skip(1)
        .map(|line| {
            let mut fields = line.splitn(5, '\t');
            let dir = fields.next() == Some("d");
            let size = fields.next().unwrap().parse().unwrap();
            let executable = fields.next() == Some("x");
            let sha = fields.next().unwrap();
            (
                fields.next().unwrap(),
                Entry {
                    dir,
                    size,
                    executable,
                    sha,
                },
            )
        })
        .collect()
}

pub(super) fn conflict(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, message.into())
}

/// 先枚举整棵树再报告缺损，避免“缺少 A、另含未知 B”被误认为允许替换。
fn inventory(
    root: &Path,
    expected: &BTreeMap<&str, Entry>,
) -> io::Result<BTreeMap<String, fs::Metadata>> {
    if !fs::symlink_metadata(root)?.is_dir() {
        return Err(conflict(format!(
            "版本位置被其他类型文件占用：{}",
            root.display()
        )));
    }
    let mut actual = BTreeMap::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(dir) = directories.pop() {
        for item in fs::read_dir(dir)? {
            let path = item?.path();
            let rel = path.strip_prefix(root).unwrap().to_str().unwrap_or("");
            let Some(entry) = expected.get(rel) else {
                return Err(conflict(format!("版本目录含未知文件：{}", path.display())));
            };
            let meta = fs::symlink_metadata(&path)?;
            if (entry.dir && !meta.is_dir()) || (!entry.dir && !meta.is_file()) {
                return Err(conflict(format!("版本条目类型冲突：{}", path.display())));
            }
            if entry.dir {
                directories.push(path.clone());
            }
            actual.insert(rel.to_owned(), meta);
        }
    }
    Ok(actual)
}

pub(super) fn repairable(root: &Path, pkg: Artifact) -> io::Result<()> {
    inventory(root, &entries(pkg)).map(|_| ())
}

/// 只有清单内的空目录可作为未来版本的骨架，未知空子目录同样构成冲突。
pub(super) fn empty_skeleton(root: &Path, pkg: Artifact) -> io::Result<bool> {
    Ok(inventory(root, &entries(pkg))?.values().all(|m| m.is_dir()))
}

fn stamp(root: &Path, pkg: Artifact) -> io::Result<(String, bool)> {
    let expected = entries(pkg);
    let actual = inventory(root, &expected)?;
    let mut hash = Sha256::new();
    hash.update(pkg.manifest.as_bytes());
    let mut precise = true;
    for (path, row) in &expected {
        let meta = actual
            .get(*path)
            .ok_or_else(|| invalid(format!("版本缺少文件：{}", root.join(path).display())))?;
        if !row.dir && (meta.len() != row.size || row.executable && meta.mode() & 0o111 == 0) {
            return Err(invalid(format!(
                "文件长度不符或不可执行：{}",
                root.join(path).display()
            )));
        }
        // HFS+/部分网络卷的 ctime 只有整秒，不能用它排除同秒内的等长改写。
        precise &= row.dir || meta.ctime_nsec() != 0;
        for n in [
            meta.dev(),
            meta.ino(),
            meta.mode() as u64,
            meta.len(),
            meta.ctime() as u64,
            meta.ctime_nsec() as u64,
            meta.mtime() as u64,
            meta.mtime_nsec() as u64,
        ] {
            hash.update(n.to_le_bytes());
        }
    }
    Ok((base16ct::lower::encode_string(&hash.finalize()), precise))
}

/// SHA-256 顺序来自发布清单。读前读后比对变更标识，防止核验中途修改后写入过时缓存。
pub(super) fn content_digest(pkg: Artifact) -> String {
    base16ct::lower::encode_string(&Sha256::digest(pkg.manifest.split_once('\n').unwrap().1))
}

pub(super) fn full(
    root: &Path,
    pkg: Artifact,
    check: impl Fn() -> io::Result<()>,
) -> io::Result<(String, (String, bool))> {
    let before = stamp(root, pkg)?;
    super::paths::validate_payload(root, pkg)?;
    for (path, row) in entries(pkg).into_iter().filter(|(_, r)| !r.dir) {
        let actual = hash::<Sha256>(File::open(root.join(path))?.take(row.size + 1), &check)?;
        if actual != row.sha {
            return Err(invalid(format!(
                "SHA-256 与发布清单不符：{}",
                root.join(path).display()
            )));
        }
    }
    if before != stamp(root, pkg)? {
        return Err(invalid("核验期间版本内容发生变化，请重新核验"));
    }
    Ok((content_digest(pkg), before))
}

pub(super) fn check(
    home: &Path,
    root: &Path,
    pkg: Artifact,
    force: bool,
    cancel: impl Fn() -> io::Result<()>,
) -> io::Result<&'static str> {
    // 按实际版本位置分开缓存，候选根核验不会覆盖当前根的核验记录。
    let key = base16ct::lower::encode_string(&Sha256::digest(root.as_os_str().as_encoded_bytes()));
    let dir = home.join("cache/resource-verification");
    let cache = dir.join(key);
    let (signature, precise) = stamp(root, pkg)?;
    if !force
        && precise
        && let Ok(message) = super::paths::small_text(&cache)
        && let Some((cached, result)) = message.split_once('\n')
        && signature == cached
    {
        if result == "verified" {
            super::paths::validate_payload(root, pkg)?;
            return Ok("quick");
        }
        if let Some(issue) = result.strip_prefix("damaged\n") {
            return Err(invalid(issue));
        }
    }
    let checked = full(root, pkg, cancel);
    if !precise {
        return checked.map(|_| "verified");
    }
    let output = match &checked {
        Ok((_, (stamp, _))) => format!("{stamp}\nverified"),
        Err(e) if e.kind() == io::ErrorKind::InvalidData && stamp(root, pkg)?.0 == signature => {
            format!("{signature}\ndamaged\n{e}")
        }
        Err(_) => return checked.map(|_| "verified"),
    };
    // 缓存不影响接入只读资源；只写工作区私有目录。使用原子发布避免并发读到半条记录。
    if super::paths::directory(home, &dir, true).is_ok()
        && let Ok(id) = crate::instances::nonce()
    {
        let temp = dir.join(id);
        if let Ok(mut f) = File::create_new(&temp) {
            use std::io::Write;
            if f.write_all(output.as_bytes()).is_ok() {
                let _ = fs::rename(&temp, &cache);
            }
            let _ = fs::remove_file(temp);
        }
    }
    checked.map(|_| "verified")
}

/// 预放目录须只有官方版本或通向它的目录骨架。设置中选取的外部根只检查涉及的版本。
pub fn preview_seed(home: &Path) -> io::Result<Vec<String>> {
    let packages = super::RuntimePaths::managed_packages(
        &windroid_locations::StorageLocations::defaults(home),
    );
    let mut containers = BTreeSet::new();
    for pkg in &packages {
        for parent in pkg
            .path
            .ancestors()
            .skip(1)
            .take_while(|p| p.starts_with(home))
        {
            containers.insert(parent);
        }
    }
    let mut versions = Vec::new();
    let mut todo = vec![home.to_owned()];
    while let Some(dir) = todo.pop() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if !fs::symlink_metadata(&path)?.is_dir() {
                return Err(conflict(format!("预放目录含未知项目：{}", path.display())));
            }
            if let Some(pkg) = packages.iter().find(|pkg| pkg.path == path) {
                if !empty_skeleton(&path, pkg.artifact)? {
                    pkg.verify_at(&path)?;
                    versions.push(pkg.label());
                }
            } else if containers.contains(path.as_path()) {
                todo.push(path);
            } else {
                return Err(conflict(format!("预放目录含未知项目：{}", path.display())));
            }
        }
    }
    versions.sort();
    Ok(versions)
}

/// 重建入口：指定 WINDROID_RELEASE_ARCHIVES（七个官方原始文件）和 WINDROID_MANIFEST_OUTPUT，
/// 执行 cargo test -p runtime --release rebuild_release_manifests -- --ignored。
#[test]
#[ignore = "needs all pinned official release archives"]
fn rebuild_release_manifests() -> io::Result<()> {
    use std::path::PathBuf;
    let variable = |key| {
        std::env::var_os(key)
            .map(PathBuf::from)
            .ok_or_else(|| invalid(format!("请设置 {key}")))
    };
    let source = variable("WINDROID_RELEASE_ARCHIVES")?;
    let output = variable("WINDROID_MANIFEST_OUTPUT")?;
    fs::create_dir_all(&output)?;
    for pkg in super::CATALOG {
        let path = source.join(pkg.url.rsplit('/').next().unwrap());
        super::integrity::check_digest(&path, pkg, || Ok(()))?;
        let mut rows = BTreeMap::new();
        if let Some(prefix) = pkg.prefix {
            let mut archive = zip::ZipArchive::new(File::open(path)?)?;
            for index in 0..archive.len() {
                let mut file = archive.by_index(index)?;
                let rel = super::archive::relative(file.name_raw(), prefix)?;
                if rel.as_os_str().is_empty() {
                    continue;
                }
                for parent in rel
                    .ancestors()
                    .skip(1)
                    .filter(|p| !p.as_os_str().is_empty())
                {
                    rows.insert(parent.to_owned(), "d\t0\t-\t-".to_owned());
                }
                let row = if file.is_dir() {
                    "d\t0\t-\t-".to_owned()
                } else {
                    let executable = if file.unix_mode().unwrap_or(0) & 0o111 != 0 {
                        "x"
                    } else {
                        "-"
                    };
                    format!(
                        "f\t{}\t{executable}\t{}",
                        file.size(),
                        hash::<Sha256>(&mut file, || Ok(()))?
                    )
                };
                rows.insert(rel, row);
            }
        } else {
            rows.insert(
                PathBuf::from("scrcpy-server.jar"),
                format!("f\t{}\t-\t{}", pkg.bytes, pkg.digest.hex()),
            );
        }
        let mut text = format!("# {}\n", pkg.digest.hex());
        // 按相对路径的 UTF-8 字节排序，不使用 Path 的分段比较顺序。
        let rows: BTreeMap<_, _> = rows
            .into_iter()
            .map(|(p, row)| (p.into_os_string().into_string().unwrap(), row))
            .collect();
        for (path, row) in rows {
            text.push_str(&format!("{row}\t{path}\n"));
        }
        fs::write(output.join(format!("{}.tsv", pkg.id)), text)?;
    }
    Ok(())
}
