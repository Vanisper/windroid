//! 运行时与 adb 共用的目录设置。实例、日志和设置始终归属原工作区。
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

pub const SETTINGS: &str = "storage-v2.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageLocations {
    pub runtime_root: PathBuf,
    /// 直接存放 android-* 及其修订目录，位置独立于运行组件。
    pub image_root: PathBuf,
}

impl StorageLocations {
    pub fn defaults(home: &Path) -> Self {
        Self {
            runtime_root: home.join("runtime"),
            image_root: home.join("system-images"),
        }
    }

    pub fn load(home: &Path) -> io::Result<Self> {
        let path = home.join(SETTINGS);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                if !meta.is_file() || meta.len() > 32 * 1024 {
                    return Err(io::Error::new(ErrorKind::InvalidData, "资源位置设置必须为普通 JSON 文件"));
                }
                let file = File::open(path)?;
                let locations: Self = serde_json::from_reader(file)?;
                for path in [&locations.runtime_root, &locations.image_root] {
                    if !path.is_absolute()
                        || path
                            .components()
                            .any(|p| p == std::path::Component::ParentDir)
                    {
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            "存储位置必须为绝对路径",
                        ));
                    }
                }
                Ok(locations)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::defaults(home)),
            Err(e) => Err(e),
        }
    }

    pub fn images(&self) -> PathBuf {
        self.image_root.clone()
    }
}
