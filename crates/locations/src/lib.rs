//! 运行时与 adb 共用的目录设置。实例、日志和设置始终归属原工作区。
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

pub const SETTINGS: &str = "storage-locations.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageLocations {
    pub runtime_root: PathBuf,
    /// None 随受管 SDK 迁移。显式目录直接存放 android-* 版本。
    pub image_root: Option<PathBuf>,
}

impl StorageLocations {
    pub fn defaults(home: &Path) -> Self {
        Self {
            runtime_root: home.join("runtime"),
            image_root: None,
        }
    }

    pub fn load(home: &Path) -> io::Result<Self> {
        match File::open(home.join(SETTINGS)) {
            Ok(file) => {
                let locations: Self = serde_json::from_reader(file)?;
                for path in std::iter::once(&locations.runtime_root).chain(&locations.image_root) {
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
        self.image_root
            .clone()
            .unwrap_or_else(|| self.runtime_root.join("sdk/system-images"))
    }
}
