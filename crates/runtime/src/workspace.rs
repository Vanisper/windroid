//! 首次打开先检查工作区身份。此模块的 inspect 路径不会创建锁、日志或偏好，
//! 也不会恢复事务或打开实例私有数据盘。
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

const FORMAT: &str = "workspace-format";
const VERSION: &[u8] = b"windroid-workspace-v2\n";
const LEGACY: &[&str] = &[
    "storage-locations.json",
    ".storage-migration.json",
    "staging",
    "groups.json",
];

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

fn entries(path: &Path) -> io::Result<Vec<OsString>> {
    fs::read_dir(path)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect()
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
    let items = entries(home).map_err(unknown)?;
    if items.is_empty() {
        return Ok(Workspace::Empty);
    }
    let has = |name: &str| items.iter().any(|item| item == name);
    let current = has(FORMAT);
    // 文件存在本身就表明旧布局，包括断开的链接。避免打开旧设置中引用的外部卷。
    let old_runtime = home.join("runtime");
    let old = LEGACY
        .iter()
        .find(|name| has(name))
        .map(|s| s.to_string())
        .or_else(|| {
            (!current
                && ["instances", "preferences", "logs", "cache", ".storage.lock"]
                    .iter()
                    .any(|name| has(name)))
            .then(|| "既有工作区状态".into())
        });
    if let Some(item) = old {
        return if current {
            Err(unknown(format!("新格式记录与旧状态 {item} 并存")))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("此目录是旧版 Windroid 工作区（{item}），原文件已保留。"),
            ))
        };
    }
    if has("runtime") {
        plain(&old_runtime, true).map_err(unknown)?;
        let names = entries(&old_runtime).map_err(unknown)?;
        if names
            .iter()
            .any(|s| s == "sdk" || s == "scrcpy-server.jar" || s == ".windroid-workspace")
        {
            return Err(if current {
                unknown("新格式工作区内存在旧运行组件布局")
            } else {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "此目录使用旧版运行组件布局，原文件已保留。",
                )
            });
        }
    }
    if !current {
        if items.iter().any(|s| s != "runtime" && s != "system-images") {
            return Err(unknown("目录中存在未识别的文件或工作区记录"));
        }
        let names = crate::provision::preview_seed(home).map_err(unknown)?;
        if names.is_empty() {
            return Err(unknown("目录非空，未识别到完整的清单资源"));
        }
        return Ok(Workspace::Resources(names));
    }
    let path = home.join(FORMAT);
    plain(&path, false)?;
    let mut value = Vec::new();
    File::open(&path)
        .and_then(|f| f.take(128).read_to_end(&mut value))
        .map_err(unknown)?;
    if value != VERSION {
        return Err(unknown("工作区版本记录损坏或不受支持"));
    }
    for dir in ["instances", "preferences", "cache", "logs", "system-images"] {
        if has(dir) {
            plain(&home.join(dir), true).map_err(unknown)?;
        }
    }
    // 其他目录可以是用户保留在工作区内的自选资源根。仅识别程序确实使用的
    // 顶层状态文件，避免误把旧设置或未知的持久记录当作本版状态。
    for item in &items {
        let path = home.join(item);
        let name = item.to_str().ok_or_else(|| unknown("工作区文件名无效"))?;
        let meta = fs::symlink_metadata(&path).map_err(unknown)?;
        match name {
            ".storage.lock" | ".components.lock" | ".images.lock" => {
                if !meta.is_file() || meta.len() != 0 || meta.nlink() != 1 {
                    return Err(unknown(format!(
                        "工作区锁文件类型或内容异常：{}",
                        path.display()
                    )));
                }
            }
            FORMAT | windroid_locations::SETTINGS | ".storage-migration-v2.json" | ".DS_Store" => {
                plain(&path, false).map_err(unknown)?
            }
            _ if meta.is_dir() => (),
            _ if name.starts_with(".storage-writing-")
                || name.starts_with(".workspace-format-") =>
            {
                plain(&path, false).map_err(unknown)?
            }
            _ => return Err(unknown(format!("未识别的工作区记录：{}", path.display()))),
        }
    }
    if has("preferences") {
        for key in entries(&home.join("preferences")).map_err(unknown)? {
            plain(&home.join("preferences").join(key), false).map_err(unknown)?;
        }
    }
    if has("instances") {
        crate::instances::inspect_registry(&home.join("instances")).map_err(unknown)?;
    }
    if has(windroid_locations::SETTINGS) {
        plain(&home.join(windroid_locations::SETTINGS), false)?;
        windroid_locations::StorageLocations::load(home).map_err(unknown)?;
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
        return Err(unknown("目录在检查后已变化，请重新打开"));
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
