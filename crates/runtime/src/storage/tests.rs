use super::*;
use std::process::Command;

struct Temp(PathBuf);
impl Temp {
    fn at(parent: &Path) -> Self {
        let path = parent.canonicalize().unwrap().join(format!(
            "windroid-storage-{}",
            crate::instances::nonce().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn new() -> Self {
        Self::at(&std::env::temp_dir())
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn save(home: &Path, locations: &StorageLocations) {
    fs::create_dir_all(home).unwrap();
    fs::write(
        home.join(windroid_locations::SETTINGS),
        serde_json::to_vec(locations).unwrap(),
    )
    .unwrap();
}
fn future(home: &Path, target: &StorageLocations) {
    let plan = preview_switch(home, target, SwitchMode::Future, false).unwrap();
    assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);
    apply(home, &plan).unwrap();
}

#[test]
fn preview_and_empty_location_selection_leave_both_resource_roots_untouched() {
    let tmp = Temp::new();
    let home = tmp.0.join("workspace");
    let expected = StorageLocations::defaults(&home);
    let separate = StorageLocations {
        runtime_root: tmp.0.join("external"),
        image_root: tmp.0.join("external/images"),
    };
    let p = preview_switch(&home, &separate, SwitchMode::Future, false).unwrap();
    assert!(!home.exists() && !separate.runtime_root.exists());
    assert_eq!(p.changed_roots(), (true, true));
    assert_eq!(p.required_bytes, 0);
    apply(&home, &p).unwrap();
    assert!(!separate.runtime_root.exists());
    let mut reset = separate.clone();
    reset.runtime_root = expected.runtime_root.clone();
    future(&home, &reset);
    assert_eq!(
        StorageLocations::load(&home).unwrap().image_root,
        separate.image_root
    );
    reset.image_root = expected.image_root;
    future(&home, &reset);
    // 两个工作区可按任一顺序选择相同根和父子根。
    for reverse in [false, true] {
        let a = tmp.0.join(format!("a-{reverse}"));
        let b = tmp.0.join(format!("b-{reverse}"));
        for workspace in if reverse { [&b, &a] } else { [&a, &b] } {
            future(workspace, &separate);
        }
        assert_eq!(
            StorageLocations::load(&a).unwrap(),
            StorageLocations::load(&b).unwrap()
        );
    }
}

#[test]
fn unrelated_offline_root_does_not_block_selection_and_unknown_entries_are_preserved() {
    let tmp = Temp::new();
    let home = tmp.0.join("home");
    let mut before = StorageLocations::defaults(&home);
    before.image_root = PathBuf::from("/Volumes")
        .join(format!(
            "windroid-offline-{}",
            crate::instances::nonce().unwrap()
        ))
        .join("images");
    save(&home, &before);
    let target = StorageLocations {
        runtime_root: tmp.0.join("tools"),
        ..before.clone()
    };
    let jar = target.runtime_root.join("scrcpy-server/5.0.1");
    fs::create_dir_all(&jar).unwrap();
    fs::write(jar.join("notes"), b"personal").unwrap();
    let p = preview_switch(&home, &target, SwitchMode::Use, false).unwrap();
    let row = p.versions.iter().find(|v| v.id == "scrcpy-server").unwrap();
    assert_eq!(row.status, VersionStatus::Conflict);
    assert!(row.detail.contains("notes"), "{}", row.detail);
    assert!(apply(&home, &p).is_err());
    assert_eq!(fs::read(jar.join("notes")).unwrap(), b"personal");
    assert_eq!(StorageLocations::load(&home).unwrap(), before);
}

#[test]
fn absent_bound_revision_is_reported_by_instance_and_only_it_is_downloadable() {
    let tmp = Temp::new();
    let home = tmp.0.join("home");
    let image = RuntimePaths::managed_packages(&StorageLocations::defaults(&home))
        .into_iter()
        .find(|p| p.id() == "system-image")
        .unwrap()
        .image()
        .unwrap();
    InstanceStore::new(Layout { root: home.clone() })
        .create(
            "Device A",
            contract::Spec {
                template: "compact-720".into(),
                disk_bytes: 6 << 30,
                image: image.clone(),
            },
        )
        .unwrap();
    let target = StorageLocations {
        image_root: tmp.0.join("new images"),
        ..StorageLocations::defaults(&home)
    };
    let p = preview_switch(&home, &target, SwitchMode::Use, false).unwrap();
    let needed: Vec<_> = p.versions.iter().filter(|v| v.downloadable).collect();
    assert_eq!(needed.len(), 1);
    assert_eq!(needed[0].id, "system-image");
    assert!(p.blockers.iter().any(|line| line.contains("Device A")));
    assert!(p.affected_instances[0].contains(&format!("r{}", image.revision)));
    assert!(apply(&home, &p).is_err());
    assert!(!target.image_root.exists());
}

#[test]
fn processes_coordinate_by_version_across_rename() {
    const CHILD: &str = "WINDROID_TEST_VERSION_LOCK_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let path = root.join("r14");
        assert_eq!(
            version_write_lease(&path).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        if root.join("after-rename").exists() {
            assert_eq!(
                version_read_lease(&path).unwrap_err().kind(),
                ErrorKind::WouldBlock
            );
        }
        return;
    }
    let tmp = Temp::new();
    let version = tmp.0.join("r14");
    fs::create_dir(&version).unwrap();
    let check = || {
        assert!(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "storage::tests::processes_coordinate_by_version_across_rename"
                ])
                .env(CHILD, &tmp.0)
                .status()
                .unwrap()
                .success()
        )
    };
    let reader = version_read_lease(&version).unwrap();
    check();
    assert_eq!(fs::read_dir(&version).unwrap().count(), 0);
    drop(reader);
    let writer = version_write_lease(&version).unwrap();
    let stage = tmp.0.join("stage");
    fs::create_dir(&stage).unwrap();
    let staged = lock_staged_version(&stage).unwrap();
    fs::rename(&version, tmp.0.join("old")).unwrap();
    fs::rename(&stage, &version).unwrap();
    fs::write(tmp.0.join("after-rename"), []).unwrap();
    check();
    // 同 API 的另一个修订具有独立的协调点。
    let sibling = tmp.0.join("r15");
    fs::create_dir(&sibling).unwrap();
    let other = version_write_lease(&sibling).unwrap();
    drop(other);
    drop(staged);
    drop(writer);
    version_read_lease(&version).unwrap();
}

fn copy_official_jar(resource_root: &Path, config: &StorageLocations) {
    let package = RuntimePaths::managed_packages(config)
        .into_iter()
        .find(|p| p.id() == "scrcpy-server")
        .unwrap();
    fs::create_dir_all(&package.path).unwrap();
    fs::copy(
        resource_root.join("scrcpy-server/5.0.1/scrcpy-server.jar"),
        package.path.join("scrcpy-server.jar"),
    )
    .unwrap();
}

#[test]
#[ignore = "set WINDROID_TEST_VERSIONED_RUNTIME and WINDROID_TEST_VOLUME to a writable independent volume"]
fn official_jar_round_trip_reuses_content_and_defers_busy_cleanup() {
    let tools = PathBuf::from(std::env::var_os("WINDROID_TEST_VERSIONED_RUNTIME").unwrap());
    let volume = PathBuf::from(std::env::var_os("WINDROID_TEST_VOLUME").unwrap());
    let primary = Temp::new();
    let target = Temp::at(&volume);
    assert_ne!(
        fs::metadata(&primary.0).unwrap().dev(),
        fs::metadata(&target.0).unwrap().dev()
    );
    let home = primary.0.join("home");
    let original = StorageLocations::defaults(&home);
    copy_official_jar(&tools, &original);
    let alternate = StorageLocations {
        runtime_root: target.0.join("arbitrary-name"),
        ..original.clone()
    };
    let jar = Path::new("scrcpy-server/5.0.1");
    fs::create_dir_all(alternate.runtime_root.join(jar)).unwrap();
    let p = preview(&home, &alternate).unwrap();
    assert_eq!(
        p.versions
            .iter()
            .filter(|v| v.status == VersionStatus::Copy)
            .count(),
        1
    );
    assert!(p.available_bytes.is_some_and(|n| n >= p.required_bytes));
    apply(&home, &p).unwrap();
    assert!(original.runtime_root.join(jar).is_dir() && alternate.runtime_root.join(jar).is_dir());
    let reader = version_read_lease(&alternate.runtime_root.join(jar)).unwrap();
    let back = preview_switch(&home, &original, SwitchMode::Migrate, true).unwrap();
    assert_eq!(back.required_bytes, 0);
    assert!(
        back.versions
            .iter()
            .any(|v| v.id == "scrcpy-server" && v.cleanup && v.status == VersionStatus::Reuse)
    );
    apply(&home, &back).unwrap();
    assert!(alternate.runtime_root.join(jar).is_dir());
    assert!(!pending_cleanup(&home).unwrap().is_empty());
    drop(reader);
    recover(&home).unwrap();
    assert!(!alternate.runtime_root.join(jar).exists() && original.runtime_root.join(jar).is_dir());
    assert!(pending_cleanup(&home).unwrap().is_empty());
}

/// 只读目标包含全部真实核心组件。源卷故意不存在，设置文件是唯一变更。
#[test]
#[ignore = "set WINDROID_TEST_VERSIONED_RUNTIME to the complete runtime root on a read-only test volume"]
fn attaches_complete_read_only_components_with_offline_source() {
    let target = PathBuf::from(std::env::var_os("WINDROID_TEST_VERSIONED_RUNTIME").unwrap());
    let home = Temp::new();
    assert!(volume_space(&target).unwrap().2, "目标应实际挂载为只读");
    let source = StorageLocations {
        runtime_root: PathBuf::from("/Volumes/windroid-test-disconnected-sources/core"),
        ..StorageLocations::defaults(&home.0)
    };
    save(&home.0, &source);
    let after = StorageLocations {
        runtime_root: target,
        ..source
    };
    let plan = preview_switch(&home.0, &after, SwitchMode::Use, false).unwrap();
    assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);
    assert_eq!(
        plan.versions
            .iter()
            .filter(|v| v.status == VersionStatus::Reuse)
            .count(),
        4
    );
    assert_eq!(plan.required_bytes, 0);
    apply(&home.0, &plan).unwrap();
    assert_eq!(StorageLocations::load(&home.0).unwrap(), after);
}
