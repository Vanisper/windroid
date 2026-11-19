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
    assert!(snap.candidate_target.is_none());
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
    assert_eq!(paths.runtime_root, root.join("runtime"));
    assert_eq!(
        paths.server_jar,
        root.join("runtime/scrcpy-server/5.0.1/scrcpy-server.jar")
    );
    assert_eq!(paths.images, root.join("system-images"));
    assert!(!root.exists());
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
        image_root: std::env::var_os("WINDROID_QA_VOLUME")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.clone())
            .join(format!("images-import-{}", std::process::id())),
    };
    fs::write(
        home.join(windroid_locations::SETTINGS),
        serde_json::to_vec(&locations).unwrap(),
    )
    .unwrap();
    fs::create_dir(&locations.image_root).unwrap();
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
            .all(|c| c.ready && c.location.starts_with(&locations.image_root))
    );
    assert!(!p.snapshot().components_ready);
    assert!(!home.join("instances").exists());
    fs::remove_dir_all(&locations.image_root).unwrap();
    fs::remove_dir_all(root).unwrap();
}
