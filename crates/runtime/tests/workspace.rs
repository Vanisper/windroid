//! 使用合成旧记录及独立外部卷路径验证格式门禁。快照覆盖文件内容、目录成员、权限和修改时刻。
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
    let before = tree(&temp.0);
    assert_eq!(workspace::inspect(&home).unwrap(), Workspace::Current);
    workspace::open(&home).unwrap();
    assert_eq!(tree(&temp.0), before);
}

#[test]
fn rejected_workspaces_and_referenced_files_are_unchanged() {
    for case in [
        "unknown",
        "legacy",
        "mixed-record",
        "mixed-layout",
        "unreadable",
        "uncommitted-transaction",
        "malformed-settings",
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
        if !["unknown", "legacy", "linked-home"].contains(&case) {
            workspace::open(&home).unwrap();
        }
        match case {
            "unknown" => fs::write(home.join("something"), "unknown").unwrap(),
            "legacy" => {
                let instance = home.join("instances/a");
                fs::create_dir_all(&instance).unwrap();
                fs::write(instance.join("instance.json"), r#"{"image":{"api":34}}"#).unwrap();
                fs::write(instance.join("userdata-qemu.img"), b"private disk sentinel").unwrap();
                fs::write(
                    home.join("storage-locations.json"),
                    format!(
                        r#"{{"runtimeRoot":"{}","imageRoot":null}}"#,
                        external.display()
                    ),
                )
                .unwrap();
                fs::write(home.join(".storage-migration.json"), "{}").unwrap();
            }
            "uncommitted-transaction" => {
                fs::write(home.join(".storage-migration-v2.json"), b"{").unwrap()
            }
            "damaged-marker" => {
                fs::write(home.join("workspace-format"), b"windroid-workspace-v9\n").unwrap()
            }
            "mixed-layout" => fs::create_dir_all(home.join("runtime/sdk/emulator")).unwrap(),
            "mixed-record" => {
                let store =
                    runtime::instances::InstanceStore::new(runtime::Layout { root: home.clone() });
                let record = store
                    .create("旧清单", support::spec("compact-720"))
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
                home.join("storage-v2.json"),
                r#"{"runtimeRoot":"/does-not-exist","imageRoot":null}"#,
            )
            .unwrap(),
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
            assert!(workspace::open(&home).is_err(), "{case}");
            assert_eq!(tree(&temp.0), before, "{case}");
        }
    }
}

#[test]
fn missing_ancestor_cannot_redirect_initialization_into_an_old_workspace() {
    let temp = support::Temp::create();
    let old = temp.0.join("old");
    fs::create_dir(&old).unwrap();
    fs::write(old.join("storage-locations.json"), "{}").unwrap();
    let before = tree(&temp.0);
    assert!(workspace::open(&temp.0.join("missing/../old")).is_err());
    assert_eq!(tree(&temp.0), before);
}

#[test]
fn current_marker_with_an_unversioned_image_is_rejected_without_writes() {
    let temp = support::Temp::create();
    let home = temp.0.join("home");
    workspace::open(&home).unwrap();
    let root = home.join("system-images");
    let spec = support::spec("compact-720");
    for (api, filename) in [
        (33, "system.img"),
        (34, "source.properties"),
        (35, "system.img"),
    ] {
        let parent = root
            .join(format!("android-{api}"))
            .join(&spec.image.tag)
            .join(&spec.image.abi);
        fs::create_dir_all(&parent).unwrap();
        let path = parent.join(filename);
        fs::write(&path, b"preserve old payload").unwrap();
        let before = tree(&temp.0);
        let problem = workspace::open(&home).unwrap_err().to_string();
        assert!(
            problem.contains("无法确认工作区格式") && problem.contains(filename),
            "{problem}"
        );
        assert_eq!(tree(&temp.0), before);
        fs::remove_file(path).unwrap();
    }
    fs::write(root.join(".windroid-workspace"), b"previous owner").unwrap();
    let before = tree(&temp.0);
    assert!(
        workspace::open(&home)
            .unwrap_err()
            .to_string()
            .contains(".windroid-workspace")
    );
    assert_eq!(tree(&temp.0), before);
}

#[test]
fn mixed_image_check_stays_inside_default_legacy_package_directories() {
    let temp = support::Temp::create();
    let home = temp.0.join("home");
    workspace::open(&home).unwrap();
    let image = support::spec("compact-720").image;
    let abi = Path::new(&image.tag).join(&image.abi);
    let root = home.join("system-images");
    let parent = root.join("android-34").join(&abi);
    fs::create_dir_all(parent.join("r14")).unwrap();
    fs::write(parent.join("r14/system.img"), b"revision payload").unwrap();
    fs::write(parent.join("readme"), b"user note").unwrap();
    let external = temp.0.join("manual-images");
    let target = external.join("android-33").join(&abi);
    fs::create_dir_all(&target).unwrap();
    fs::write(
        target.join("source.properties"),
        b"unrelated manual package",
    )
    .unwrap();
    symlink(external.join("android-33"), root.join("android-33")).unwrap();
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
