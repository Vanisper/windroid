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
            "linked-lock" => symlink(external.join("keep"), home.join(".storage.lock")).unwrap(),
            "unreadable" => {
                fs::set_permissions(home.join("workspace-format"), fs::Permissions::from_mode(0o0))
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
