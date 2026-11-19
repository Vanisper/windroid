//! 实际版本目录上的共享使用权随进程关闭释放。写方还持有同一父目录中稳定的
//! 独占锁，覆盖原版本改名和新版本发布之间的间隔。只读方不创建任何锁文件。
use super::{absolute, check_volume, invalid, metadata};
use crate::provision::RuntimePaths;
use contract::Image;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub(super) fn lock(home: &Path, name: &str) -> io::Result<File> {
    fs::create_dir_all(home)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(home.join(name))?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != 0 || meta.nlink() != 1 {
        return Err(invalid(format!(
            "锁位置已被其他文件占用：{}",
            home.join(name).display()
        )));
    }
    Ok(file)
}

pub(crate) fn resource_lease(home: &Path, components: bool, images: bool) -> io::Result<Vec<File>> {
    workspace_locks(home, (components, images), false)
}

pub(super) fn workspace_locks(
    home: &Path,
    changed: (bool, bool),
    moving: bool,
) -> io::Result<Vec<File>> {
    let mut leases = Vec::new();
    for (needed, name) in [(changed.0, ".components.lock"), (changed.1, ".images.lock")] {
        if needed {
            let file = lock(home, name)?;
            let acquired = if moving {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            acquired.map_err(|_| {
                io::Error::new(
                    ErrorKind::WouldBlock,
                    "请等待本工作区相关资源准备或切换完成",
                )
            })?;
            leases.push(file);
        }
    }
    Ok(leases)
}

fn object(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
}

fn contended(path: &Path, error: std::fs::TryLockError) -> io::Error {
    let failure: io::Error = error.into();
    io::Error::new(
        failure.kind(),
        format!("版本 {} 正被使用或无法可靠加锁：{failure}", path.display()),
    )
}

pub(crate) fn version_read_lease(path: &Path) -> io::Result<File> {
    let file = object(path)?;
    match file.try_lock_shared() {
        Ok(()) => (),
        // 不支持对象锁的卷仍可读取。本实现的写方必须能取得对象锁才可发布或清理。
        Err(std::fs::TryLockError::Error(e)) if e.kind() == ErrorKind::Unsupported => (),
        Err(e) => return Err(contended(path, e)),
    }
    let opened = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    if !current.is_dir() || (opened.dev(), opened.ino()) != (current.dev(), current.ino()) {
        return Err(io::Error::new(
            ErrorKind::WouldBlock,
            "版本已被替换，请重试",
        ));
    }
    Ok(file)
}

// 不能证明两个独立打开的对象互斥时，此卷只允许接入读取。
fn confirm_exclusive(another: File, path: &Path) -> io::Result<()> {
    match another.try_lock_shared() {
        Err(std::fs::TryLockError::WouldBlock) => Ok(()),
        Err(e) => Err(contended(path, e)),
        Ok(()) => Err(io::Error::new(
            ErrorKind::Unsupported,
            format!(
                "{} 所在卷不支持可靠的独占锁，可只读接入完整版本",
                path.display()
            ),
        )),
    }
}

pub(crate) fn lock_staged_version(path: &Path) -> io::Result<File> {
    let file = object(path)?;
    file.try_lock().map_err(|e| contended(path, e))?;
    confirm_exclusive(object(path)?, path)?;
    Ok(file)
}

pub(crate) fn version_write_lease(path: &Path) -> io::Result<Vec<File>> {
    let parent = path.parent().ok_or_else(|| invalid("版本目录缺少父目录"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("版本目录名称无效"))?;
    check_volume(parent)?;
    let parent = absolute(parent)?;
    // 版本名由固定清单给出。父目录的真实位置决定协调点，同一卷上的路径别名共用它。
    let filename = format!(".windroid-write-{}.lock", name.to_ascii_lowercase());
    let writer = lock(&parent, &filename)?;
    writer.try_lock().map_err(|e| contended(path, e))?;
    confirm_exclusive(lock(&parent, &filename)?, path)?;
    let mut leases = vec![writer];
    if metadata(path)?.is_some() {
        leases.push(lock_staged_version(path)?);
    }
    Ok(leases)
}

pub(crate) fn instance_lease(home: &Path, image: &Image) -> io::Result<(RuntimePaths, Vec<File>)> {
    let _selection = resource_lease(home, true, true)?;
    let config = super::StorageLocations::load(home)?;
    let points = super::mounts::load(home)?;
    points.check(&config.runtime_root, false)?;
    points.check(&config.image_root, true)?;
    let paths = RuntimePaths::for_locations(&config);
    let mut leases = Vec::new();
    let mut selected = false;
    for pkg in RuntimePaths::managed_packages(&config) {
        if let Some(pinned) = pkg.image() {
            if pinned != *image {
                continue;
            }
            selected = true;
        }
        leases.push(version_read_lease(&pkg.path)?);
        pkg.check(home)?;
    }
    if !selected {
        return Err(invalid("实例绑定的确切镜像修订不在发布清单中"));
    }
    Ok((paths, leases))
}
