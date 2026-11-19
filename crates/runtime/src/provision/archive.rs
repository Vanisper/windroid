use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::catalog::Artifact;
use super::integrity::invalid;
use super::paths::{RECEIPT, directory, receipt, small_text, validate_payload};

// 固定包的目录均为 ASCII。逐段检查原始名字，避免解包器静默「修正」路径。
fn relative(raw: &[u8], prefix: &str) -> io::Result<PathBuf> {
    let name = std::str::from_utf8(raw).map_err(|_| invalid("无效 ZIP 文件名"))?;
    let name = name.strip_suffix('/').unwrap_or(name);
    if name.len() > 1024 {
        return Err(invalid("ZIP 路径过长"));
    }
    let mut parts = name.split('/');
    if parts.next() != Some(prefix) {
        return Err(invalid("ZIP 顶层目录与组件记录不符"));
    }
    let mut relative = PathBuf::new();
    for part in parts {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.eq_ignore_ascii_case(RECEIPT)
            || !part.bytes().all(|c| c.is_ascii_graphic() || c == b' ')
            || part.contains(['\\', ':'])
        {
            return Err(invalid("ZIP 中含不安全的文件名"));
        }
        relative.push(part);
    }
    Ok(relative)
}

fn unpack(
    file: &File,
    scratch: &Path,
    pkg: Artifact,
    check: &impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    let mut archive = zip::ZipArchive::new(file)?;
    if archive.len() > 4096 {
        return Err(invalid("ZIP 条目数超限"));
    }
    if archive.has_duplicate_names() || archive.has_overlapping_entries()? {
        return Err(invalid("ZIP 条目重复或重叠"));
    }
    let prefix = pkg.prefix.expect("archive component");
    let mut expanded: u64 = 0;
    let mut buf = vec![0; 256 * 1024];
    for index in 0..archive.len() {
        check()?;
        let mut entry = archive.by_index(index)?;
        let rel = relative(entry.name_raw(), prefix)?;
        let dir = entry.is_dir();
        let mode = entry.unix_mode().unwrap_or(0);
        let file_type = mode & 0o170000;
        if (file_type != 0 && file_type != if dir { 0o040000 } else { 0o100000 })
            || (dir && entry.size() != 0)
            || (!dir && rel.as_os_str().is_empty())
        {
            return Err(invalid("ZIP 条目类型无效，不能安装链接或设备文件"));
        }
        expanded = expanded
            .checked_add(entry.size())
            .filter(|s| *s <= pkg.unpacked)
            .ok_or_else(|| invalid("ZIP 展开体积超过发布记录"))?;
        let target = scratch.join(rel);
        if dir {
            fs::create_dir_all(target)?;
            continue;
        }
        fs::create_dir_all(target.parent().unwrap())?;
        // staging 每次重建且不接受链接。create_new 也拦截大小写折叠后的重复文件。
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let mut left = entry.size();
        loop {
            check()?;
            let count = entry.read(&mut buf)?;
            if count == 0 {
                break;
            }
            left = left
                .checked_sub(count as u64)
                .ok_or_else(|| invalid("ZIP 条目超出声明大小"))?;
            file.write_all(&buf[..count])?;
        }
        if left != 0 {
            return Err(invalid("ZIP 条目不完整"));
        }
        file.set_permissions(fs::Permissions::from_mode(if mode & 0o111 != 0 {
            0o755
        } else {
            0o644
        }))?;
    }
    if expanded != pkg.unpacked {
        return Err(invalid("ZIP 展开体积与发布记录不符"));
    }
    Ok(())
}

/// 只清理已核对所有权的安装暂存区内的工作文件。
fn remove_owned(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 解压与改名都在目标卷完成。独立工作区复用同一组件目录时也要串行发布。
fn workspace(parent: &Path, pkg: Artifact) -> io::Result<(PathBuf, File)> {
    let stage = parent.join(format!(".windroid-install-{}", pkg.id));
    let owner = stage.join("owner");
    const MARKER: &str = "windroid-package-install-v1\n";
    if let Err(e) = fs::create_dir(&stage)
        && (e.kind() != io::ErrorKind::AlreadyExists || !fs::symlink_metadata(&stage)?.is_dir())
    {
        return Err(e);
    }
    // 支持上次恰好在创建目录和写标记之间退出；只认领空目录。
    let empty = fs::read_dir(&stage)?.next().is_none();
    let mut lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(empty)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&owner)?;
    lock.try_lock()
        .map_err(|e| io::Error::other(format!("组件正在其他工作区安装：{e}")))?;
    let marker = small_text(&owner)?;
    if marker.is_empty()
        && fs::read_dir(&stage)?.all(|e| e.is_ok_and(|f| f.path() == owner))
        && lock.metadata()?.nlink() == 1
    {
        lock.write_all(MARKER.as_bytes())?;
    } else if marker != MARKER {
        return Err(invalid(format!(
            "安装暂存位置已被其他文件占用：{}",
            stage.display()
        )));
    }
    Ok((stage, lock))
}

fn publish_file(stage: &Path, scratch: &Path, dest: &Path) -> io::Result<()> {
    let identity = |meta: &fs::Metadata| format!("{}:{}\n", meta.dev(), meta.ino());
    let published = stage.join("published");
    match fs::symlink_metadata(dest) {
        Ok(meta)
            if meta.is_file() && small_text(&published).is_ok_and(|id| id == identity(&meta)) =>
        {
            fs::rename(scratch, dest)?;
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "目标已有非受管文件，请移走该文件或另选存储位置：{}",
                    dest.display()
                ),
            ));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            crate::storage::rename_exclusive(scratch, dest)?;
        }
        Err(e) => return Err(e),
    }
    let temp = stage.join("published.new");
    remove_owned(&temp)?;
    File::create_new(&temp)?.write_all(identity(&fs::metadata(dest)?).as_bytes())?;
    fs::rename(temp, published)
}

pub(super) fn install(
    root: &Path,
    mut input: &File,
    dest: &Path,
    pkg: Artifact,
    check: impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    input.rewind()?;
    let parent = dest.parent().ok_or_else(|| invalid("组件目标缺少父目录"))?;
    directory(root, parent, true)?;
    let (stage, _lock) = workspace(parent, pkg)?;
    // 同一目标可能已由另一工作区在下载期间安装。保留已在使用的完整文件。
    if (pkg.prefix.is_none() || small_text(&dest.join(RECEIPT)).is_ok_and(|r| r == receipt(pkg)))
        && validate_payload(dest, pkg).is_ok()
    {
        return Ok(());
    }
    let scratch = stage.join("unpack");
    let backup = stage.join("previous");
    // 上次在两次 rename 间退出。先恢复旧版本，再尝试发布本次产物。
    if fs::symlink_metadata(&backup).is_ok() && !dest.try_exists()? {
        fs::rename(&backup, dest)?;
    }
    remove_owned(&scratch)?;
    let result = (|| {
        if pkg.prefix.is_none() {
            check()?;
            io::copy(
                &mut input,
                &mut OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&scratch)?,
            )?;
            validate_payload(&scratch, pkg)?;
            return publish_file(&stage, &scratch, dest);
        }
        fs::create_dir(&scratch)?;
        unpack(input, &scratch, pkg, &check)?;
        validate_payload(&scratch, pkg)?;
        fs::write(scratch.join(RECEIPT), receipt(pkg))?;
        check()?;
        remove_owned(&backup)?;
        let had_old = match fs::symlink_metadata(dest) {
            Ok(m) if m.is_dir() => {
                if !small_text(&dest.join(RECEIPT))
                    .is_ok_and(|r| r.starts_with(&format!("windroid-v1 {} ", pkg.id)))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("目标已有非受管目录，请另选存储位置：{}", dest.display()),
                    ));
                }
                fs::rename(dest, &backup)?;
                true
            }
            Ok(_) => return Err(invalid("组件目标应为私有目录")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        if let Err(e) = crate::storage::rename_exclusive(&scratch, dest) {
            if had_old && !dest.try_exists()? {
                crate::storage::rename_exclusive(&backup, dest)?;
            }
            return Err(e);
        }
        remove_owned(&backup)
    })();
    if result.is_err() {
        remove_owned(&scratch)?;
    }
    result
}
