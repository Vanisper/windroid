use super::*;
use std::os::unix::fs::symlink;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "windroid-storage-{}",
            crate::instances::nonce().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn file(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn mark_package(settings: &StorageLocations, directory: &Path) {
    let pkg = RuntimePaths::managed_packages(settings)
        .into_iter()
        .find(|pkg| pkg.path == directory)
        .unwrap();
    file(
        &directory.join(".windroid-component"),
        pkg.receipt().unwrap().as_bytes(),
    );
}

#[test]
fn independent_roots_move_only_catalog_packages_and_can_restore_inheritance() {
    let temp = Temp::new();
    let home = temp.0.join("workspace");
    let old = StorageLocations::defaults(&home);
    let emulator = Path::new("sdk/emulator/emulator");
    let img = Path::new("android-34/google_apis_playstore")
        .join(concat!("arm64", "-v8a"))
        .join("system.img");
    file(&old.runtime_root.join(emulator), b"emulator");
    file(&old.images().join(&img), b"shared-system");
    mark_package(&old, old.runtime_root.join(emulator).parent().unwrap());
    mark_package(&old, old.images().join(&img).parent().unwrap());
    file(&old.runtime_root.join("scrcpy-server.jar"), b"personal jar");
    file(&old.images().join("unknown/private-file"), b"keep");
    file(&home.join("instances/personal/data"), b"private");
    // 已存在的空目标目录可以选作新根，旁边无关的文件需原样保留。
    let detached = StorageLocations {
        runtime_root: temp.0.join("tools"),
        image_root: Some(temp.0.join("images")),
    };
    file(&detached.images().join("notes"), b"user notes");
    let plan = preview(&home, &detached).unwrap();
    assert_eq!(plan.files.len(), 4);
    let p = crate::provision::Provisioner::managed(&home);
    apply(&home, &plan).unwrap();
    p.refresh_locations();
    assert_eq!(p.paths().sdk, detached.runtime_root.join("sdk"));
    assert!(
        p.snapshot()
            .images
            .iter()
            .all(|row| row.location.starts_with(detached.images()))
    );
    assert_eq!(
        fs::read(detached.runtime_root.join(emulator)).unwrap(),
        b"emulator"
    );
    assert_eq!(
        fs::read(detached.images().join(&img)).unwrap(),
        b"shared-system"
    );
    assert_eq!(
        fs::read(old.images().join("unknown/private-file")).unwrap(),
        b"keep"
    );
    assert!(!old.runtime_root.join(emulator).exists());
    assert_eq!(
        fs::read(old.runtime_root.join("scrcpy-server.jar")).unwrap(),
        b"personal jar"
    );
    let inherited = StorageLocations {
        image_root: None,
        ..detached.clone()
    };
    apply(&home, &preview(&home, &inherited).unwrap()).unwrap();
    assert_eq!(StorageLocations::load(&home).unwrap(), inherited);
    assert_eq!(
        fs::read(inherited.images().join(&img)).unwrap(),
        b"shared-system"
    );
    assert_eq!(
        fs::read(detached.images().join("notes")).unwrap(),
        b"user notes"
    );
    assert_eq!(
        fs::read(home.join("instances/personal/data")).unwrap(),
        b"private"
    );
}

#[test]
fn checks_related_leases_or_orphan_processes_and_declines_stale_previews() {
    let temp = Temp::new();
    let home = temp.0.join("workspace");
    let mut changed = StorageLocations::defaults(&home);
    changed.image_root = Some(temp.0.join("images"));
    let plan = preview(&home, &changed).unwrap();
    let active_tools = resource_lease(&home, true, false).unwrap();
    let active_image = resource_lease(&home, false, true).unwrap();
    assert_eq!(
        apply(&home, &plan).unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    drop(active_image);
    apply(&home, &plan).unwrap();
    let mut moved = changed.clone();
    moved.runtime_root = temp.0.join("moved");
    let plan = preview(&home, &moved).unwrap();
    assert_eq!(
        apply(&home, &plan).unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    drop(active_tools);
    let active_image = resource_lease(&home, false, true).unwrap();
    file(&changed.runtime_root.join("sdk/emulator/new"), b"new");
    mark_package(&changed, &changed.runtime_root.join("sdk/emulator"));
    assert!(
        apply(&home, &plan)
            .unwrap_err()
            .to_string()
            .contains("重新预览")
    );
    let plan = preview(&home, &moved).unwrap();
    apply(&home, &plan).unwrap();
    drop(active_image);
    file(&changed.runtime_root.join("scrcpy-server.jar"), b"foreign");
    assert!(
        preview(&home, &changed)
            .unwrap_err()
            .to_string()
            .contains("已存在")
    );
    assert_eq!(
        fs::read(changed.runtime_root.join("scrcpy-server.jar")).unwrap(),
        b"foreign"
    );
    assert_eq!(StorageLocations::load(&home).unwrap(), moved);

    let store = InstanceStore::new(Layout { root: home.clone() });
    let instance = store
        .create(
            "原有实例",
            contract::Spec {
                template: "compact-720".into(),
                image: contract::Image {
                    api: 34,
                    tag: "google_apis_playstore".into(),
                    abi: "arm64".into(),
                },
                disk_bytes: 6 << 30,
            },
        )
        .unwrap();
    file(
        &home
            .join("instances")
            .join(instance.id.as_str())
            .join("active.pid"),
        std::process::id().to_string().as_bytes(),
    );
    let independent = StorageLocations {
        image_root: Some(temp.0.join("other")),
        ..moved
    };
    assert_eq!(
        apply(&home, &preview(&home, &independent).unwrap())
            .unwrap_err()
            .kind(),
        ErrorKind::WouldBlock
    );
}

#[test]
fn interrupted_publish_recovers_according_to_committed_settings() {
    for committed in [false, true] {
        let temp = Temp::new();
        let home = temp.0.join("workspace");
        let before = StorageLocations::defaults(&home);
        let after = StorageLocations {
            runtime_root: temp.0.join("tools"),
            image_root: None,
        };
        let source = before.runtime_root.join("scrcpy-server.jar");
        let target = after.runtime_root.join("scrcpy-server.jar");
        file(&source, b"trusted");
        file(&target, b"trusted");
        let journal = Journal {
            before: before.clone(),
            after: after.clone(),
            nonce: crate::instances::nonce().unwrap(),
            packages: vec![Package {
                source: source.clone(),
                target: target.clone(),
                source_stamp: tree_stamp(&source).unwrap(),
                staged_stamp: Some(tree_stamp(&target).unwrap()),
            }],
        };
        if committed {
            write_json(&home.join(SETTINGS), &after).unwrap();
        }
        write_json(&home.join(JOURNAL), &journal).unwrap();
        recover(&home).unwrap();
        recover(&home).unwrap();
        assert_eq!(target.exists(), committed);
        assert_eq!(source.exists(), !committed);
        assert_eq!(
            StorageLocations::load(&home).unwrap(),
            if committed { after } else { before }
        );
        assert!(!home.join(JOURNAL).exists());
    }
}

#[test]
fn rejects_nested_roots_and_linked_packages_before_any_move() {
    let temp = Temp::new();
    let home = temp.0.join("workspace");
    let before = StorageLocations::defaults(&home);
    let pkg = before.runtime_root.join("sdk/emulator");
    file(&pkg.join("emulator"), b"core");
    mark_package(&before, &pkg);
    for root in [
        home.clone(),
        home.join("instances/a"),
        pkg.join("subfolder"),
    ] {
        assert!(
            preview(
                &home,
                &StorageLocations {
                    runtime_root: root,
                    image_root: None
                }
            )
            .is_err()
        );
    }
    let after = StorageLocations {
        runtime_root: temp.0.join("new"),
        image_root: None,
    };
    symlink(temp.0.join("external"), pkg.join("foreign")).unwrap();
    assert!(preview(&home, &after).is_err());
    assert!(!after.runtime_root.exists());
}

/// 在指定卷内只创建一个新临时目录，使用受控的小包验证跨卷发布。
#[test]
#[ignore = "mount a writable test volume and set WINDROID_TEST_VOLUME"]
fn migrates_between_different_filesystems() {
    let primary = Temp::new();
    let volume =
        PathBuf::from(std::env::var_os("WINDROID_TEST_VOLUME").expect("set WINDROID_TEST_VOLUME"));
    assert_ne!(
        fs::metadata(&primary.0).unwrap().dev(),
        fs::metadata(&volume).unwrap().dev()
    );
    let folder = volume.join(format!(
        "windroid-test-{}",
        crate::instances::nonce().unwrap()
    ));
    fs::create_dir(&folder).unwrap();
    let external = Temp(folder);
    let home = primary.0.join("workspace");
    let original = StorageLocations::defaults(&home);
    let part = Path::new("sdk/emulator/emulator");
    file(&original.runtime_root.join(part), b"emulator-fixture");
    mark_package(
        &original,
        original.runtime_root.join(part).parent().unwrap(),
    );
    let moved = StorageLocations {
        runtime_root: external.0.join("core"),
        image_root: None,
    };
    apply(&home, &preview(&home, &moved).unwrap()).unwrap();
    assert_eq!(
        fs::read(moved.runtime_root.join(part)).unwrap(),
        b"emulator-fixture"
    );
    apply(&home, &preview(&home, &original).unwrap()).unwrap();
    assert_eq!(
        fs::read(original.runtime_root.join(part)).unwrap(),
        b"emulator-fixture"
    );
    assert!(!moved.runtime_root.join(part).exists());
}

#[test]
fn newly_selected_directories_have_one_owner_until_released() {
    let temp = Temp::new();
    let a = temp.0.join("A");
    let b = temp.0.join("B");
    let shared = StorageLocations {
        runtime_root: temp.0.join("common"),
        image_root: None,
    };
    let b_plan = preview(&b, &shared).unwrap();
    apply(&a, &preview(&a, &shared).unwrap()).unwrap();
    assert!(
        apply(&b, &b_plan)
            .unwrap_err()
            .to_string()
            .contains("另一个工作区")
    );
    assert!(preview(&b, &shared).is_err());
    let nested = StorageLocations {
        runtime_root: shared.runtime_root.join("nested"),
        image_root: None,
    };
    assert!(preview(&b, &nested).is_err());
    // 手动复制设置也不会取得另一个工作区已登记目录的写权限。
    write_json(&b.join(SETTINGS), &shared).unwrap();
    let paths = RuntimePaths::discover(&b);
    assert!(paths.claim_locations(&b, true, true).is_err());
    fs::remove_file(b.join(SETTINGS)).unwrap();
    apply(&a, &preview(&a, &StorageLocations::defaults(&a)).unwrap()).unwrap();
    apply(&b, &b_plan).unwrap();
    assert_eq!(StorageLocations::load(&b).unwrap(), shared);
}

#[test]
fn interrupted_cleanup_preserves_changed_contents_even_with_the_same_package_inode() {
    let temp = Temp::new();
    let home = temp.0.join("workspace");
    let before = StorageLocations::defaults(&home);
    let after = StorageLocations {
        runtime_root: temp.0.join("moved"),
        image_root: None,
    };
    let source = before.runtime_root.join("sdk/emulator");
    let target = after.runtime_root.join("sdk/emulator");
    file(&source.join("binary"), b"original");
    file(&target.join("binary"), b"original");
    let mut pkg = Package::planned(&source, &target).unwrap();
    pkg.staged_stamp = Some(tree_stamp(&target).unwrap());
    let journal = Journal {
        before,
        after: after.clone(),
        nonce: crate::instances::nonce().unwrap(),
        packages: vec![pkg],
    };
    write_json(&home.join(SETTINGS), &after).unwrap();
    write_json(&home.join(JOURNAL), &journal).unwrap();
    file(&source.join("private"), b"added after shutdown");
    file(&source.join("binary"), b"changed");
    let message = recover(&home).unwrap_err().to_string();
    assert!(
        message.contains(&*source.display().to_string()),
        "{message}"
    );
    assert_eq!(
        fs::read(source.join("private")).unwrap(),
        b"added after shutdown"
    );
    assert_eq!(fs::read(source.join("binary")).unwrap(), b"changed");
    assert_eq!(fs::read(target.join("binary")).unwrap(), b"original");
    recover(&home).unwrap();
    assert_eq!(StorageLocations::load(&home).unwrap(), after);
}
