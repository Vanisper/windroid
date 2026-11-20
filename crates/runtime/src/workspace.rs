//! 首次打开先检查工作区身份。此模块的 inspect 路径不会创建锁、日志或偏好，
//! 也不会恢复事务或打开实例私有数据盘。
use std::ffi::{CString, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

const FORMAT: &str = "workspace-format";
const VERSION: &[u8] = b"windroid-workspace-v1\n";

#[derive(Debug, PartialEq, Eq)]
pub enum Workspace {
    Empty,
    Current,
    Resources(Vec<String>),
}

fn unknown(detail: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("无法确认工作区格式：{detail}"),
    )
}

fn plain(path: &Path, directory: bool) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if (directory && m.is_dir()) || (!directory && m.is_file()) {
        Ok(())
    } else {
        Err(unknown(format!("路径类型异常：{}", path.display())))
    }
}

/// 使用现有凭据探测后续初始化需要的访问权，不创建文件，也不改变锁或权限。
pub(crate) fn access(path: &Path, mode: libc::c_int) -> io::Result<()> {
    let name = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: 路径在调用期间有效，access 只查询权限。
    if unsafe { libc::access(name.as_ptr(), mode) } == 0 {
        return Ok(());
    }
    let cause = io::Error::last_os_error();
    Err(io::Error::new(
        cause.kind(),
        format!("{}：{cause}", path.display()),
    ))
}

fn entries(path: &Path) -> io::Result<Vec<OsString>> {
    fs::read_dir(path)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect()
}

fn entry_type(path: &Path) -> io::Result<Option<fs::FileType>> {
    match fs::symlink_metadata(path) {
        Ok(info) => Ok(Some(info.file_type())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn inspect(home: &Path) -> io::Result<Workspace> {
    if !home.is_absolute()
        || home
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        return Err(unknown("工作区需要不含 .. 的绝对路径"));
    }
    crate::storage::check_volume(home).map_err(unknown)?;
    match fs::symlink_metadata(home) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Workspace::Empty),
        Err(e) => return Err(unknown(e)),
        Ok(m) if !m.is_dir() => return Err(unknown("所选路径应为普通目录")),
        Ok(_) => (),
    }
    // 与实际读写方使用相同的路径查询，遵循所在卷的大小写规则。
    let has = |name: &str| entry_type(&home.join(name)).map(|entry| entry.is_some());
    if !has(FORMAT)? {
        let items = entries(home).map_err(unknown)?;
        if items.is_empty() {
            access(home, libc::R_OK | libc::W_OK | libc::X_OK).map_err(unknown)?;
            return Ok(Workspace::Empty);
        }
        if items.iter().any(|s| s != "runtime" && s != "system-images") {
            return Err(unknown("目录已有内容，缺少 Windroid 工作区格式标记"));
        }
        access(home, libc::R_OK | libc::W_OK | libc::X_OK).map_err(unknown)?;
        let names = crate::provision::preview_seed(home).map_err(unknown)?;
        if names.is_empty() {
            return Err(unknown("目录非空，未识别到完整的清单资源"));
        }
        return Ok(Workspace::Resources(names));
    }
    let path = home.join(FORMAT);
    plain(&path, false)?;
    let mut value = Vec::new();
    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .and_then(|f| f.take(VERSION.len() as u64 + 1).read_to_end(&mut value))
        .map_err(|e| unknown(format!("{}：{e}", path.display())))?;
    if value != VERSION {
        return Err(unknown(format!(
            "{}：工作区版本记录损坏或不受支持",
            path.display()
        )));
    }
    // 标记确定工作区身份。只检查客户端会打开的路径，其他内容留在原处。
    access(home, libc::R_OK | libc::W_OK | libc::X_OK).map_err(unknown)?;
    for name in ["runtime", "system-images"] {
        if has(name)? {
            let dir = home.join(name);
            plain(&dir, true)?;
            access(&dir, libc::R_OK | libc::X_OK).map_err(unknown)?;
        }
    }
    if has("logs")? {
        plain(&home.join("logs"), true)?;
    }
    for name in ["instances", "preferences", "cache", crate::storage::CLEANUP] {
        if has(name)? {
            let dir = home.join(name);
            plain(&dir, true)?;
            access(&dir, libc::R_OK | libc::W_OK | libc::X_OK).map_err(unknown)?;
        }
    }
    for name in [".storage.lock", ".components.lock", ".images.lock"] {
        if has(name)? {
            let path = home.join(name);
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_file() || meta.len() != 0 || meta.nlink() != 1 {
                return Err(unknown(format!("锁文件类型或内容异常：{}", path.display())));
            }
            access(&path, libc::R_OK | libc::W_OK).map_err(unknown)?;
        }
    }
    if has("preferences")? {
        let prefs = home.join("preferences");
        for name in ["first-page.pending", "first-page-shown", "window-close"] {
            let path = prefs.join(name);
            if entry_type(&path)?.is_some() {
                plain(&path, false)?;
                access(&path, libc::R_OK).map_err(unknown)?;
            }
        }
    }
    if has("instances")? {
        crate::instances::inspect_registry(&home.join("instances")).map_err(unknown)?;
    }
    crate::storage::validate_journal(home).map_err(unknown)?;
    Ok(Workspace::Current)
}

/// 建库前重新探测，预放包必须与用户刚确认的只读预览相符。
pub fn initialize(home: &Path, choice: &Workspace) -> io::Result<()> {
    let actual = inspect(home)?;
    if actual == Workspace::Current {
        return Ok(());
    }
    if &actual != choice {
        return Err(unknown("目录内容在确认期间已改变"));
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(home)?;
    let temp = home.join(format!(".workspace-format-{}", crate::instances::nonce()?));
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        file.write_all(VERSION)?;
        file.sync_all()?;
        match fs::hard_link(&temp, home.join(FORMAT)) {
            Err(e)
                if e.kind() == io::ErrorKind::AlreadyExists
                    && inspect(home)? == Workspace::Current =>
            {
                Ok(())
            }
            other => other,
        }
    })();
    fs::remove_file(temp)?;
    result?;
    File::open(home)?.sync_all()
}

pub fn open(home: &Path) -> io::Result<()> {
    match inspect(home)? {
        Workspace::Empty => initialize(home, &Workspace::Empty),
        Workspace::Current => Ok(()),
        Workspace::Resources(_) => Err(unknown("请先确认使用预放资源新建工作区")),
    }
}
