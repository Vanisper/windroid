use contract::{Image, Spec};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 每个测试创建全新的私有根目录，子测试只访问其父测试创建并传入的目录。
pub struct Temp(pub PathBuf);
impl Temp {
    pub fn create() -> Self {
        let base = std::env::temp_dir().canonicalize().unwrap();
        let path = base.join(format!(
            "windroid-instance-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("清理测试独占的实例目录");
    }
}

pub fn spec(template: &str) -> Spec {
    Spec {
        template: template.into(),
        image: Image {
            revision: 14,
            api: 34,
            tag: "google_apis_playstore".into(),
            abi: "arm64".into(),
        },
        disk_bytes: 8 << 30,
    }
}

pub fn avd(root: &Path, record: &runtime::instances::InstanceRecord) -> PathBuf {
    root.join("instances")
        .join(record.id.as_str())
        .join("avd")
        .join(format!("{}.avd", record.avd_name()))
}
