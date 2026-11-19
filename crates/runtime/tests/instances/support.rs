use contract::{Image, Spec};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 每个测试创建全新的私有根目录，子测试只访问其父测试创建并传入的目录。
pub struct Temp(pub PathBuf);
impl Temp {
    pub fn create() -> Self {
        let base = std::env::temp_dir().canonicalize().unwrap();
        Self::in_directory(&base)
    }

    pub fn in_directory(base: &Path) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "windroid-instance-test-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("无法建立临时目录 {}：{e}", path.display()),
            }
        }
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
            abi: concat!("arm64", "-v8a").into(),
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
