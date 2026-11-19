use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::catalog::Artifact;
use super::integrity::invalid;
use super::paths::small_text;
use crate::storage::StableObject;

// 固定包的目录均为 ASCII。逐段检查原始名字，避免解包器静默「修正」路径。
pub(super) fn relative(raw: &[u8], prefix: &str) -> io::Result<PathBuf> {
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
pub(super) fn stage_path(dest: &Path, pkg: Artifact) -> PathBuf {
    dest.parent()
        .unwrap()
        .join(format!(".windroid-install-{}-{}", pkg.id, pkg.digest.hex()))
}

pub(super) fn workspace(dest: &Path, pkg: Artifact) -> io::Result<(PathBuf, File)> {
    let stage = stage_path(dest, pkg);
    let owner = stage.join("owner");
    const MARKER: &str = "windroid-package-install-v2\n";
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
    // 调用方已持有跨工作区的版本写锁；这个描述符只用于标识原卷和暂存对象。
    if !lock.metadata()?.is_file() || lock.metadata()?.nlink() != 1 {
        return Err(invalid("暂存身份记录必须为独立普通文件"));
    }
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

#[derive(serde::Serialize, serde::Deserialize)]
struct Publish {
    stage: StableObject,
    old: Option<StableObject>,
    new: StableObject,
}

pub(super) fn object(path: &Path) -> io::Result<Option<StableObject>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => StableObject::at(path).map(Some),
        Ok(_) => Err(invalid(format!(
            "发布目录被其他文件占用：{}",
            path.display()
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 事务仅操作同卷私有路径。重入时若版本已换入，先核验再清理；否则恢复原目录。
pub(super) fn recover(
    home: &Path,
    stage: &Path,
    dest: &Path,
    pkg: Artifact,
    guard: &impl Fn() -> io::Result<()>,
) -> io::Result<Option<File>> {
    guard()?;
    let journal = stage.join("publish.json");
    let record: Publish = match small_text(&journal) {
        Ok(s) => serde_json::from_str(&s)
            .map_err(|_| invalid("发布记录缺少可核对的卷及对象身份，已保留暂存和原版本"))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !record.stage.matches(stage)? {
        return Err(invalid("安装暂存目录或所在卷已更换，无法恢复此发布"));
    }
    let backup = stage.join("previous");
    let current = object(dest)?;
    let previous = object(&backup)?;
    let mut restored = None;
    if previous.is_some() && previous != record.old {
        return Err(invalid(format!("原版本备份发生变化：{}", backup.display())));
    }
    if current.as_ref() == Some(&record.new) {
        match super::manifest::check(home, dest, pkg, true, guard) {
            Ok(_) if previous.is_some() => {
                let _lease = crate::storage::lock_staged_version(&backup)?;
                super::manifest::repairable(&backup, pkg)?;
                remove_version(&backup, record.old.as_ref(), guard)?;
            }
            Ok(_) => (),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::InvalidData
                        | io::ErrorKind::NotFound
                        | io::ErrorKind::UnexpectedEof
                ) =>
            {
                super::manifest::repairable(dest, pkg)?;
                if previous.is_some() {
                    restored = Some(crate::storage::lock_staged_version(&backup)?);
                    require_object(dest, Some(&record.new), guard)?;
                    crate::storage::rename_exclusive(dest, &stage.join("rejected"))?;
                    require_object(&backup, record.old.as_ref(), guard)?;
                    crate::storage::rename_exclusive(&backup, dest)?;
                }
                // 首次发布没有备份时保留现状，本次明确修复会在新副本通过验收后再替换。
            }
            Err(e) => return Err(e),
        }
    } else if current.is_none() && previous.is_some() {
        restored = Some(crate::storage::lock_staged_version(&backup)?);
        require_object(&backup, record.old.as_ref(), guard)?;
        crate::storage::rename_exclusive(&backup, dest)?;
    } else if current != record.old {
        return Err(invalid(format!(
            "目标版本在事务期间改变：{}",
            dest.display()
        )));
    }
    let rejected = stage.join("rejected");
    if let Some(identity) = object(&rejected)? {
        if identity != record.new {
            return Err(invalid("撤回版本的对象身份已变化"));
        }
        super::manifest::repairable(&rejected, pkg)?;
        remove_version(&rejected, Some(&record.new), guard)?;
    }
    guard()?;
    fs::remove_file(journal)?;
    Ok(restored)
}

fn require_object(
    path: &Path,
    expected: Option<&StableObject>,
    guard: &impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    guard()?;
    if object(path)?.as_ref() != expected {
        return Err(invalid(format!("卷或版本目录已更换：{}", path.display())));
    }
    Ok(())
}

fn remove_version(
    path: &Path,
    expected: Option<&StableObject>,
    guard: &impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    require_object(path, expected, guard)?;
    remove_owned(path)
}

/// 调用方已取得版本写锁并恢复发布事务，可以回收该次中断留下的解压空间。
pub(super) fn discard_unpack(stage: &Path) -> io::Result<()> {
    remove_owned(&stage.join("unpack"))
}

pub(super) fn install(
    home: &Path,
    stage: &Path,
    mut input: &File,
    dest: &Path,
    pkg: Artifact,
    approved: Option<&StableObject>,
    check: impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    let scratch = stage.join("unpack");
    let backup = stage.join("previous");
    let stage_identity = StableObject::at(stage)?;
    check()?;
    discard_unpack(stage)?;
    if object(&backup)?.is_some() {
        return Err(invalid("无法确认原版本备份的来源"));
    }
    input.rewind()?;
    fs::create_dir(&scratch)?;
    let scratch_identity = StableObject::at(&scratch)?;
    let result = (|| {
        if pkg.prefix.is_some() {
            unpack(input, &scratch, pkg, &check)?;
        } else {
            io::copy(
                &mut input,
                &mut File::create_new(scratch.join("scrcpy-server.jar"))?,
            )?;
        }
        super::manifest::full(&scratch, pkg, &check)?;
        check()?;
        let _next = crate::storage::lock_staged_version(&scratch)?;
        let old = object(dest)?;
        if old.is_some() {
            if old.as_ref() != approved && !super::manifest::empty_skeleton(dest, pkg)? {
                return Err(invalid("待安装版本已出现，请重新检查后确认操作"));
            }
            super::manifest::repairable(dest, pkg)?;
        }
        let record = Publish {
            stage: stage_identity.clone(),
            old,
            new: object(&scratch)?.unwrap(),
        };
        let journal = stage.join("publish.json");
        let temp = stage.join("publish.new");
        check()?;
        remove_owned(&temp)?;
        let mut f = File::create_new(&temp)?;
        serde_json::to_writer(&mut f, &record)?;
        f.sync_all()?;
        crate::storage::rename_exclusive(&temp, &journal)?;
        require_object(dest, record.old.as_ref(), &check)?;
        if record.old.is_some() {
            crate::storage::rename_exclusive(dest, &backup)?;
        }
        require_object(&scratch, Some(&record.new), &check)?;
        if let Err(e) = crate::storage::rename_exclusive(&scratch, dest) {
            if record.old.is_some() && object(dest)?.is_none() {
                require_object(&backup, record.old.as_ref(), &check)?;
                crate::storage::rename_exclusive(&backup, dest)?;
            }
            return Err(e);
        }
        // 当前进程已持有备份（原目标）对象的独占锁，此处无需再次申请。
        if let Err(e) = super::manifest::check(home, dest, pkg, true, &check) {
            super::manifest::repairable(dest, pkg)?;
            require_object(dest, Some(&record.new), &check)?;
            crate::storage::rename_exclusive(dest, &scratch)?;
            if record.old.is_some() {
                require_object(&backup, record.old.as_ref(), &check)?;
                crate::storage::rename_exclusive(&backup, dest)?;
            }
            check()?;
            fs::remove_file(journal)?;
            return Err(e);
        }
        if record.old.is_some() {
            super::manifest::repairable(&backup, pkg)?;
            require_object(dest, Some(&record.new), &check)?;
            remove_version(&backup, record.old.as_ref(), &check)?;
        }
        check()?;
        fs::remove_file(journal)?;
        // 另一线程可能正在 fork 子进程，显式解锁后才向调用方报告新版本可读。
        File::unlock(&_next)
    })();
    if result.is_err()
        && check().is_ok()
        && stage_identity.matches(stage).unwrap_or(false)
        && scratch_identity.matches(&scratch).unwrap_or(false)
    {
        remove_owned(&scratch)?;
    }
    result
}

pub(super) fn is_workspace(stage: &Path) -> bool {
    fs::symlink_metadata(stage).is_ok_and(|m| m.is_dir())
        && small_text(&stage.join("owner")).is_ok_and(|s| s == "windroid-package-install-v2\n")
}
