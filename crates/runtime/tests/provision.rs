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
    assert_eq!(snap.components.len(), 4);
    assert_eq!(snap.downloaded_bytes, 0);
    assert!(!snap.components_ready);
    assert_eq!(
        snap.images
            .iter()
            .map(|c| c.image.as_ref().unwrap().api)
            .collect::<Vec<_>>(),
        [34, 35, 33]
    );
    assert_eq!(snap.total_bytes, 509_516_883);
    for c in snap.components.into_iter().chain(snap.images) {
        assert!(!c.ready && c.issue.is_none());
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

/// 使用从清单 URL 单独取得的真实 ZIP。三个镜像都安装在隔离的可配置目录。
#[test]
#[ignore = "set WINDROID_TEST_IMAGE_ARCHIVES to a directory containing all official image ZIPs"]
fn verifies_and_imports_each_official_image_into_the_same_library() {
    use std::{
        fs, thread,
        time::{Duration, Instant},
    };
    let archives = PathBuf::from(
        std::env::var_os("WINDROID_TEST_IMAGE_ARCHIVES").expect("set WINDROID_TEST_IMAGE_ARCHIVES"),
    );
    let root =
        std::env::temp_dir().join(format!("windroid-official-import-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let home = root.join("home");
    fs::create_dir(&home).unwrap();
    let locations = windroid_locations::StorageLocations {
        runtime_root: root.join("tools"),
        image_root: Some(root.join("images")),
    };
    fs::write(
        home.join(windroid_locations::SETTINGS),
        serde_json::to_vec(&locations).unwrap(),
    )
    .unwrap();
    let p = Provisioner::managed(&home);
    for row in p.snapshot().images {
        let archive = archives.join(row.source.rsplit('/').next().unwrap());
        let preview = p.inspect_image_zip(&archive).unwrap();
        assert_eq!(preview.id, row.id);
        assert!(!preview.ready && !preview.location.exists());
        p.import_image(row.id, archive).unwrap();
        let start = Instant::now();
        loop {
            let snap = p.snapshot();
            assert!(snap.error.is_none(), "{:?}", snap.error);
            if snap.images.iter().any(|c| c.id == row.id && c.ready) {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(240));
            thread::sleep(Duration::from_millis(200));
        }
        eprintln!("{} r{} → {}", row.name, row.version, row.location.display());
    }
    let p = Provisioner::managed(&home);
    assert!(
        p.snapshot()
            .images
            .iter()
            .all(|c| c.ready && c.location.starts_with(root.join("images")))
    );
    assert!(!p.snapshot().components_ready);
    assert!(!home.join("instances").exists());
    fs::remove_dir_all(root).unwrap();
}
