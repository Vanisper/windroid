//! 使用独立目录验证工作区身份和只读检查。快照覆盖文件内容、目录成员、权限和修改时刻。
#[path = "instances/support.rs"]
#[allow(dead_code)]
mod support;

use runtime::workspace::{self, Workspace};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

type Files = BTreeMap<PathBuf, (u32, i64, i64, Vec<u8>)>;
fn tree(root: &Path) -> Files {
    fn walk(path: &Path, result: &mut Files) {
        let m = fs::symlink_metadata(path).unwrap();
        let bytes = if m.is_file() {
            fs::read(path).unwrap()
        } else if m.is_symlink() {
            fs::read_link(path)
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        } else {
            vec![]
        };
        result.insert(path.into(), (m.mode(), m.mtime(), m.mtime_nsec(), bytes));
        if m.is_dir() {
            for item in fs::read_dir(path).unwrap() {
                walk(&item.unwrap().path(), result)
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, &mut files);
    files
}

#[test]
fn fresh_and_current_accept_only_the_intended_initialization() {
    let temp = support::Temp::create();
    let home = temp.0.join("workspace");
    assert_eq!(workspace::inspect(&home).unwrap(), Workspace::Empty);
    assert!(!home.exists());
    fs::create_dir(&home).unwrap();
    workspace::open(&home).unwrap();
    assert_eq!(
        fs::read(home.join("workspace-format")).unwrap(),
        b"windroid-workspace-v1\n"
    );
    let before = tree(&temp.0);
    assert_eq!(workspace::inspect(&home).unwrap(), Workspace::Current);
    workspace::open(&home).unwrap();
    assert_eq!(tree(&temp.0), before);
}

#[test]
fn rejected_workspaces_and_referenced_files_are_unchanged() {
    for case in [
        "unknown",
        "unmarked-records",
        "invalid-instance",
        "invalid-root",
        "incomplete-resource",
        "unreadable",
        "uncommitted-transaction",
        "malformed-settings",
        "private-resource-location",
        "invalid-volume-binding",
        "damaged-marker",
        "bad-selection",
        "linked-lock",
        "linked-home",
    ] {
        let temp = support::Temp::create();
        let home = temp.0.join("workspace");
        let external = temp.0.join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("keep"), b"archive bytes").unwrap();
        if case == "linked-home" {
            symlink(&external, &home).unwrap();
        } else {
            fs::create_dir(&home).unwrap();
        }
        if ![
            "unknown",
            "unmarked-records",
            "incomplete-resource",
            "linked-home",
        ]
        .contains(&case)
        {
            workspace::open(&home).unwrap();
        }
        match case {
            "unknown" => fs::write(home.join("something"), "unknown").unwrap(),
            "unmarked-records" => {
                // 即使目录设置有效，也只有通过工作区身份检查后才可能使用它。
                let config = windroid_locations::StorageLocations {
                    runtime_root: external.clone(),
                    image_root: external.join("images"),
                };
                fs::write(
                    home.join(windroid_locations::SETTINGS),
                    serde_json::to_vec(&config).unwrap(),
                )
                .unwrap();
                fs::write(home.join(".storage-migration.json"), b"interrupted").unwrap();
            }
            "uncommitted-transaction" => {
                fs::write(home.join(".storage-migration.json"), b"{").unwrap()
            }
            "damaged-marker" => {
                fs::write(home.join("workspace-format"), b"windroid-workspace-v9\n").unwrap()
            }
            "invalid-root" => fs::write(home.join("runtime"), b"file").unwrap(),
            "incomplete-resource" => {
                let dir = home.join("runtime/scrcpy-server/5.0.1");
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("scrcpy-server.jar"), b"incomplete").unwrap();
            }
            "invalid-instance" => {
                let store =
                    runtime::instances::InstanceStore::new(runtime::Layout { root: home.clone() });
                let record = store
                    .create("测试实例", support::spec("compact-720"))
                    .unwrap();
                let path = home
                    .join("instances")
                    .join(record.id.as_str())
                    .join("instance.json");
                let mut json = serde_json::to_value(record).unwrap();
                json["spec"]["image"]
                    .as_object_mut()
                    .unwrap()
                    .remove("revision");
                fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
            }
            "malformed-settings" => fs::write(
                home.join(windroid_locations::SETTINGS),
                r#"{"runtimeRoot":"/does-not-exist","imageRoot":null}"#,
            )
            .unwrap(),
            "invalid-volume-binding" => fs::write(
                home.join(windroid_locations::SETTINGS),
                r#"{"runtimeRoot":"/runtime","imageRoot":"/images","mounts":{"runtime":"relative"}}"#,
            )
            .unwrap(),
            "private-resource-location" => {
                let settings = windroid_locations::StorageLocations {
                    runtime_root: home.join("instances/nested"),
                    image_root: external.clone(),
                };
                fs::write(
                    home.join(windroid_locations::SETTINGS),
                    serde_json::to_vec(&settings).unwrap(),
                )
                .unwrap();
            }
            "bad-selection" => {
                fs::create_dir(home.join("instances")).unwrap();
                fs::write(home.join("instances/.selected"), [0xff]).unwrap();
            }
            "linked-lock" => symlink(external.join("keep"), home.join(".storage.lock")).unwrap(),
            "unreadable" => {
                fs::set_permissions(
                    home.join("workspace-format"),
                    fs::Permissions::from_mode(0o0),
                )
                .unwrap();
                if fs::File::open(home.join("workspace-format")).is_ok() {
                    continue;
                }
            }
            "linked-home" => (),
            _ => unreachable!(),
        }
        // 对不可读标记记录 metadata，其他用例还逐字节比对全部文件。
        if case == "unreadable" {
            let before = fs::read_dir(&home).unwrap().count();
            assert!(workspace::open(&home).is_err());
            assert_eq!(fs::read_dir(&home).unwrap().count(), before);
        } else {
            let before = tree(&temp.0);
            let reason = workspace::open(&home).unwrap_err().to_string();
            let detail = match case {
                "unknown" | "unmarked-records" => "目录已有内容，缺少 Windroid 工作区格式标记",
                "invalid-instance" => "revision",
                "uncommitted-transaction" => ".storage-migration.json",
                "malformed-settings" | "invalid-volume-binding" => windroid_locations::SETTINGS,
                "private-resource-location" => "私有目录",
                "bad-selection" => ".selected",
                "invalid-root" => "runtime",
                "incomplete-resource" => "scrcpy-server.jar",
                "damaged-marker" => "工作区版本记录",
                "linked-lock" => ".storage.lock",
                "linked-home" => "普通目录",
                _ => unreachable!(),
            };
            assert!(reason.contains(detail), "{case}: {reason}");
            assert_eq!(tree(&temp.0), before, "{case}");
        }
    }
}

#[test]
fn missing_ancestor_cannot_redirect_initialization_into_a_nonempty_directory() {
    let temp = support::Temp::create();
    let other = temp.0.join("other");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("notes"), "keep").unwrap();
    let before = tree(&temp.0);
    assert!(workspace::open(&temp.0.join("missing/../other")).is_err());
    assert_eq!(tree(&temp.0), before);
}

#[test]
fn preflight_checks_the_paths_that_this_volume_resolves() {
    let temp = support::Temp::create();
    let home = temp.0.join("home");
    workspace::open(&home).unwrap();
    let bad = home.join("Instances/example");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("instance.json"), b"{").unwrap();
    let case_insensitive = home.join("instances").try_exists().unwrap();

    let check = |detail: &str| {
        let before = tree(&temp.0);
        let result = workspace::open(&home);
        if case_insensitive {
            let reason = result.unwrap_err().to_string();
            assert!(reason.contains(detail), "{reason}");
        } else {
            result.unwrap();
        }
        assert_eq!(tree(&temp.0), before);
    };
    check("instance.json");

    fs::remove_dir_all(home.join("Instances")).unwrap();
    let external = temp.0.join("outside");
    fs::create_dir(&external).unwrap();
    symlink(&external, home.join("Preferences")).unwrap();
    check("preferences");
}

#[test]
fn current_workspace_ignores_files_outside_its_control_records() {
    let temp = support::Temp::create();
    let home = temp.0.join("home");
    workspace::open(&home).unwrap();
    let resource_dir = home.join("resources");
    fs::create_dir(&resource_dir).unwrap();
    fs::write(resource_dir.join("notes.txt"), b"local file").unwrap();
    fs::write(home.join("inventory.json"), b"user file").unwrap();
    symlink(temp.0.join("absent"), home.join("personal-link")).unwrap();

    let before = tree(&temp.0);
    assert_eq!(workspace::inspect(&home).unwrap(), Workspace::Current);
    workspace::open(&home).unwrap();
    assert_eq!(tree(&temp.0), before);
    use runtime::storage::{StorageLocations, SwitchMode, apply, preview_switch};
    let mut locations = StorageLocations::defaults(&home);
    locations.runtime_root = resource_dir;
    let plan = preview_switch(&home, &locations, SwitchMode::Future, false).unwrap();
    apply(&home, &plan).unwrap();
    assert_eq!(workspace::inspect(&home).unwrap(), Workspace::Current);
    locations.runtime_root = home.join("instances");
    assert!(preview_switch(&home, &locations, SwitchMode::Future, false).is_err());
}

#[test]
fn external_resource_locations_can_be_opened_while_offline() {
    let temp = support::Temp::create();
    let home = temp.0.join("home");
    workspace::open(&home).unwrap();
    let external = temp.0.join("manual-images");
    let config = windroid_locations::StorageLocations {
        image_root: external,
        ..windroid_locations::StorageLocations::defaults(&home)
    };
    fs::write(
        home.join(windroid_locations::SETTINGS),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let before = tree(&temp.0);
    assert_eq!(workspace::inspect(&home).unwrap(), Workspace::Current);
    assert_eq!(tree(&temp.0), before);
}

#[test]
fn stored_resource_roots_cannot_resolve_into_private_workspace_paths() {
    let temp = support::Temp::create();
    let home = temp.0.join("workspace");
    workspace::open(&home).unwrap();
    symlink(&home, temp.0.join("workspace-link")).unwrap();
    symlink("preferences", home.join("local-link")).unwrap();
    symlink("workspace-link/local-link", temp.0.join("preferences-link")).unwrap();
    let insensitive = home.join("WORKSPACE-FORMAT").try_exists().unwrap();
    let folds_long_s = home.join("workſpace-format").try_exists().unwrap();
    fs::create_dir(home.join("logs")).unwrap();
    let settings = home.join(windroid_locations::SETTINGS);
    for (root, rejected) in [
        (home.join("INSTANCES/future"), insensitive),
        (home.join("inſtances/future"), folds_long_s),
        (home.join("logſ/future"), folds_long_s),
        (temp.0.join("WORKSPACE/logs/future"), insensitive),
        (temp.0.join("workspace-link/cache/future"), true),
        (home.join("local-link/future"), true),
        (temp.0.join("preferences-link/future"), true),
        (temp.0.join("workspace-link/public/future"), false),
    ] {
        let config = windroid_locations::StorageLocations {
            runtime_root: root.clone(),
            ..windroid_locations::StorageLocations::defaults(&home)
        };
        fs::write(&settings, serde_json::to_vec(&config).unwrap()).unwrap();
        let before = tree(&temp.0);
        let result = workspace::open(&home);
        if rejected {
            let reason = result.unwrap_err().to_string();
            assert!(reason.contains("私有目录"), "{}: {reason}", root.display());
        } else {
            result.unwrap();
        }
        assert_eq!(tree(&temp.0), before, "{}", root.display());
    }
}
