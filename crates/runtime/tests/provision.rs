//! 发布目录的外部契约。普通测试使用空目录，不依赖主机 SDK 或外网。
use runtime::provision::Provisioner;
use std::path::PathBuf;

#[test]
fn cold_open_exposes_sources_sizes_terms_without_writing() {
    let root =
        std::env::temp_dir().join(format!("windroid-empty-provision-{}", std::process::id()));
    assert!(!root.exists());
    let provision = Provisioner::managed(&root);
    provision.recheck().unwrap();
    let snap = provision.snapshot();
    assert!(!snap.borrowed);
    assert_eq!(snap.components.len(), 5);
    assert_eq!(snap.downloaded_bytes, 0);
    assert_eq!(snap.total_bytes, 2_058_422_264);
    for c in snap.components {
        assert!(!c.ready && c.issue.is_some());
        assert!(c.source.starts_with("https://"));
        assert!(c.size_bytes > 0 && c.unpacked_bytes >= c.size_bytes);
        assert!(c.license_url.starts_with("https://") && c.license_text.len() > 10_000);
    }
    let paths = provision.paths();
    assert_eq!(paths.sdk, root.join("runtime/sdk"));
    assert_eq!(paths.server_jar, root.join("runtime/scrcpy-server.jar"));
    assert!(
        paths
            .image
            .ends_with("system-images/android-34/google_apis_playstore/arm64-v8a")
    );
    assert!(!root.exists());
}

/// 仅显式指定开发 SDK 时执行。自检不启动模拟器或 adb，不写入 SDK 或主目录。
#[test]
#[ignore = "set WINDROID_SDK (and optionally WINDROID_SERVER_JAR) to a compatible read-only SDK"]
fn explicitly_borrowed_sdk_passes_the_same_layout_and_jar_checks() {
    let sdk = PathBuf::from(std::env::var_os("WINDROID_SDK").expect("set WINDROID_SDK"));
    let home = std::env::temp_dir().join(format!(
        "windroid-borrowed-provision-{}",
        std::process::id()
    ));
    assert!(!home.exists());
    let provision = Provisioner::new(&home);
    assert_eq!(provision.paths().sdk, sdk);
    assert!(provision.snapshot().borrowed);
    let issues: Vec<_> = provision
        .snapshot()
        .components
        .into_iter()
        .filter_map(|c| c.issue)
        .collect();
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(provision.snapshot().phase, "ready");
    provision.start().unwrap();
    assert!(!home.exists());
}
