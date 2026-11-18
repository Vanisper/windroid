use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::catalog::Artifact;
use super::integrity::invalid;
use super::paths::{RECEIPT, directory, receipt, validate_payload};

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
    path: &Path,
    scratch: &Path,
    pkg: Artifact,
    check: &impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(path)?)?;
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

/// 只清理私有 staging 下从固定组件 ID 构造的保留位置，拒绝跟随链接。
fn remove_owned(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub(super) fn install(
    home: &Path,
    stage: &Path,
    part: &Path,
    dest: &Path,
    pkg: Artifact,
    check: impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    let scratch = stage.join(format!("{}.unpack", pkg.id));
    let backup = stage.join(format!("{}.previous", pkg.id));
    let parent = dest.parent().ok_or_else(|| invalid("组件目标缺少父目录"))?;
    directory(home, parent, true)?;
    if pkg.prefix.is_none() {
        check()?;
        // JAR 是单文件，rename 直接原子替换。调用方已完成长度与 SHA-256 校验。
        return fs::rename(part, dest);
    }
    remove_owned(&scratch)?;
    fs::create_dir(&scratch)?;
    let result = (|| {
        unpack(part, &scratch, pkg, &check)?;
        validate_payload(&scratch, pkg)?;
        fs::write(scratch.join(RECEIPT), receipt(pkg))?;
        check()?;
        remove_owned(&backup)?;
        let had_old = match fs::symlink_metadata(dest) {
            Ok(m) if m.is_dir() => {
                fs::rename(dest, &backup)?;
                true
            }
            Ok(_) => return Err(invalid("组件目标应为私有目录")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        if let Err(e) = fs::rename(&scratch, dest) {
            if had_old {
                fs::rename(&backup, dest)?;
            }
            return Err(e);
        }
        remove_owned(&backup)?;
        fs::remove_file(part)
    })();
    if result.is_err() {
        remove_owned(&scratch)?;
    }
    result
}
