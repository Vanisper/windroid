//! 操作系统分配的卷与对象身份用于验证写操作的授权范围，与发布内容摘要独立。
use super::{existing_ancestor, invalid};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(target_os = "macos")]
fn filesystem_uuid(file: &File) -> io::Result<[u8; 16]> {
    use std::os::fd::AsRawFd;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // getattrlist(2)：请求单个 ATTR_VOL_UUID，返回 4 字节总长度及 16 字节 uuid_t。
    // 从同一已打开的对象读取，防止按路径二次查询时取到替换卷。
    let mut data = [0u8; 20];
    let result = unsafe {
        libc::fgetattrlist(
            file.as_raw_fd(),
            &mut attrs as *mut _ as *mut _,
            data.as_mut_ptr().cast(),
            data.len(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let id: [u8; 16] = data[4..].try_into().unwrap();
    if u32::from_ne_bytes(data[..4].try_into().unwrap()) != 20 || id == [0; 16] {
        return Err(io::Error::new(ErrorKind::Unsupported, "卷未提供稳定 UUID"));
    }
    Ok(id)
}

#[cfg(not(target_os = "macos"))]
fn filesystem_uuid(_: &File) -> io::Result<[u8; 16]> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "此系统无法核验持久卷标识",
    ))
}

pub(crate) fn volume_id(path: &Path) -> io::Result<[u8; 16]> {
    filesystem_uuid(&open(&existing_ancestor(path)?.canonicalize()?)?)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StableObject {
    volume: [u8; 16],
    file: u64,
    born: std::time::SystemTime,
}

impl StableObject {
    pub(crate) fn from_file(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        if (!meta.is_dir() && !meta.is_file()) || meta.ino() == 0 {
            return Err(invalid("无法确认资源对象类型与身份"));
        }
        Ok(Self {
            volume: filesystem_uuid(file)?,
            file: meta.ino(),
            born: meta.created()?,
        })
    }
    pub(crate) fn at(path: &Path) -> io::Result<Self> {
        Self::from_file(&open(path)?)
    }
    pub(crate) fn matches(&self, path: &Path) -> io::Result<bool> {
        match Self::at(path) {
            Ok(current) => Ok(*self == current),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    pub(super) fn require(&self, path: &Path) -> io::Result<()> {
        if self.matches(path)? {
            Ok(())
        } else {
            Err(invalid(format!(
                "已确认的卷或对象与当前位置不符，保留文件：{}",
                path.display()
            )))
        }
    }
}
