//! 使用独立测试进程为每个场景注入 adb 与 aapt，逐项核对实际命令及回读结果。
#![cfg(unix)]

use contract::ErrorCode;
use runtime::{
    catalog,
    install::{self, Outcome},
    running,
};
use std::{env, fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

const SERIAL: &str = "app-ops-fixture";
const FIXTURE: &str = "WINDROID_APP_OPERATIONS_FIXTURE";
const PACKAGES: &str = "pm list packages --show-versioncode --user 0";
const ACTIVITIES: &str = "dumpsys activity activities";
const RECENTS: &str = "dumpsys activity -p dev.fixture.app recents";
const EMPTY: &str = "ACTIVITY MANAGER ACTIVITIES (dumpsys activity activities)\nDisplay #0 (activities from top to bottom):\n";
const NO_RECENTS: &str = "ACTIVITY MANAGER RECENT TASKS (dumpsys activity recents)\nmRecentsUid=10173\nmFreezeTaskListReorderingPendingTimeout=false\n";
const PKG: &str = "dev.fixture.app";
const MANIFEST: &str = r#"N: android=http://schemas.android.com/apk/res/android
  E: manifest (line=1)
    A: android:versionCode(0x0101021b)=(type 0x10)0x2a
    A: android:versionCodeMajor(0x01010576)=(type 0x10)0x1
    A: package="dev.fixture.app" (Raw: "dev.fixture.app")
    E: application
      A: android:label(0x01010001)="Example"
"#;
const INDEX: &str =
    "package:android versionCode:34\npackage:dev.fixture.app versionCode:4294967338\n";

struct Fixture {
    root: PathBuf,
    adb: PathBuf,
    expected: Vec<Vec<String>>,
}

impl Fixture {
    fn device(&self) -> adb::Device<'_> {
        adb::Device::injected(&self.adb, SERIAL)
    }

    /// 工具根据调用顺序重放保存的两路输出；每个 argv 独立记录，可检测空格、选项及重复命令。
    fn returns(&mut self, args: &[&str], stdout: &str, stderr: &str, exit: u8) {
        let index = self.expected.len();
        for (field, contents) in [
            ("out", stdout.to_string()),
            ("err", stderr.into()),
            ("code", exit.to_string()),
        ] {
            fs::write(self.root.join(format!("{field}.{index}")), contents + "\n").unwrap();
        }
        self.expected
            .push(args.iter().map(|s| (*s).to_owned()).collect());
    }
    fn shell(&mut self, command: &str, stdout: &str) {
        let mut stdout = stdout.to_owned();
        if command == ACTIVITIES && stdout.starts_with(EMPTY) {
            stdout.push_str("\nActivityTaskSupervisor state:\n");
        }
        self.returns(&["adb", "-s", SERIAL, "shell", command], &stdout, "", 0);
    }
    fn apk(&self) -> String {
        self.root
            .join("-chosen 应用.apk")
            .to_str()
            .unwrap()
            .to_string()
    }
    fn metadata(&mut self, xml: &str) {
        self.returns(
            &[
                "aapt",
                "dump",
                "xmltree",
                &self.apk(),
                "AndroidManifest.xml",
            ],
            xml,
            "",
            0,
        );
    }
    fn installer(&mut self, stdout: &str, stderr: &str, code: u8) {
        self.returns(
            &[
                "adb",
                "-s",
                SERIAL,
                "install",
                "--no-incremental",
                "-r",
                "--user",
                "0",
                &self.apk(),
            ],
            stdout,
            stderr,
            code,
        );
    }
    fn install(&self) -> Outcome {
        install::install(self.device(), &self.apk(), &self.root.join("aapt")).unwrap()
    }
    fn verify(&self) {
        let count: usize = fs::read_to_string(self.root.join("next"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(count, self.expected.len(), "未执行预期数量的操作");
        for (i, expected) in self.expected.iter().enumerate() {
            let got = fs::read_to_string(self.root.join(format!("actual.{i}"))).unwrap();
            assert_eq!(got.lines().collect::<Vec<_>>(), *expected, "第 {i} 次调用");
        }
    }
}

fn with_fixture(name: &str, run: impl FnOnce(&mut Fixture)) {
    if let Some(root) = env::var_os(FIXTURE) {
        let mut fixture = Fixture {
            adb: PathBuf::from(&root).join("adb"),
            root: root.into(),
            expected: Vec::new(),
        };
        run(&mut fixture);
        fixture.verify();
        return;
    }
    let dir = env::temp_dir().join(format!("windroid-app-ops-{}-{name}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let script = r##"#!/bin/sh
set -eu
root="$WINDROID_APP_OPERATIONS_FIXTURE"
IFS= read -r index < "$root/next"
printf '%s\n' "$((index + 1))" > "$root/next"
printf '%s\n' "${0##*/}" "$@" > "$root/actual.$index"
cat "$root/out.$index"
cat "$root/err.$index" >&2
IFS= read -r code < "$root/code.$index"
exit "$code"
"##;
    for name in ["adb", "aapt"] {
        let path = dir.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(dir.join("next"), "0\n").unwrap();
    fs::write(dir.join("-chosen 应用.apk"), b"mock APK").unwrap();
    let out = Command::new(env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(FIXTURE, &dir)
        .output()
        .unwrap();
    fs::remove_dir_all(dir).unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// 标签测试允许两个设备的拉取同时进行，各自记录实际落盘路径。
fn with_label_devices(name: &str, run: impl FnOnce(&Fixture)) {
    with_fixture(name, |d| {
        let aapt = d.root.join("aapt");
        fs::create_dir_all(aapt.parent().unwrap()).unwrap();
        fs::write(
            &aapt,
            r##"#!/bin/sh
set -eu
[ "$1" = dump ]
case "$2" in
  badging) cat "$3" ;;
  xmltree) [ "$4" = AndroidManifest.xml ] && cat "$3" ;;
  *) exit 3 ;;
esac
"##,
        )
        .unwrap();
        fs::set_permissions(aapt, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            d.root.join("adb"),
            r##"#!/bin/sh
set -eu
base="$WINDROID_APP_OPERATIONS_FIXTURE/$2"
[ "$1" = -s ]
case "$3" in
  shell) [ "$4" = 'pm list packages -f --show-versioncode --user 0' ] && cat "$base.index" ;;
  pull)
    IFS= read -r expected < "$base.path"
    [ "$4" = "$expected" ]
    printf '%s\n' "$5" >> "$base.pulls"
    cp "$base.payload" "$5"
    if [ -f "$base.after" ]; then mv "$base.after" "$base.index"; fi ;;
  *) exit 4 ;;
esac
"##,
        )
        .unwrap();
        run(d);
    });
}

fn label_package(d: &Fixture, serial: &str, path: &str, version: u64, label: &str) {
    fs::write(
        d.root.join(format!("{serial}.index")),
        format!("package:{path}={PKG} versionCode:{version}\n"),
    )
    .unwrap();
    fs::write(d.root.join(format!("{serial}.path")), format!("{path}\n")).unwrap();
    fs::write(d.root.join(format!("{serial}.payload")), format!("package: name='{PKG}' versionCode='{}' versionName='unchanged'\napplication-label:'Fallback'\napplication-label-zh-CN:'{label}'\n", version as u32)).unwrap();
}

#[test]
fn labels_recheck_version_path_and_instance_before_using_cache() {
    with_label_devices(
        "labels_recheck_version_path_and_instance_before_using_cache",
        |d| {
            use contract::InstanceId;
            use runtime::labels::Labels;
            let packages = [PKG.to_owned()];
            let id = InstanceId::new("instance-a");
            let mut labels = Labels::new(&d.root, &id);
            let read = |labels: &mut Labels| {
                labels
                    .resolve(d.device(), &d.root.join("aapt"), &packages)
                    .remove(PKG)
            };
            let path = "/data/app/~~pin==/old==/base.apk";
            let next = "/data/app/~~pin==/new==/base.apk";
            label_package(d, SERIAL, path, 1, "首版:应用");
            assert_eq!(read(&mut labels).as_deref(), Some("首版:应用"));
            let mut restarted = Labels::new(&d.root, &id);
            assert_eq!(read(&mut restarted).as_deref(), Some("首版:应用"));
            let pulls = || fs::read_to_string(d.root.join(format!("{SERIAL}.pulls"))).unwrap();
            assert_eq!(pulls().lines().count(), 1, "重开实例仍应命中磁盘缓存");

            let version = (1u64 << 32) | 1;
            label_package(d, SERIAL, path, version, "高位版本");
            assert_eq!(read(&mut restarted).as_deref(), Some("高位版本"));
            label_package(d, SERIAL, next, version, "重装版本");
            assert_eq!(read(&mut restarted).as_deref(), Some("重装版本"));
            assert_eq!(pulls().lines().count(), 3);

            label_package(d, SERIAL, next, version, "同端口的新实例");
            let mut other = Labels::new(&d.root, &InstanceId::new("instance-b"));
            assert_eq!(read(&mut other).as_deref(), Some("同端口的新实例"));
            fs::write(
                d.root.join(format!("{SERIAL}.index")),
                "Can't find service: package",
            )
            .unwrap();
            assert_eq!(read(&mut other), None, "离线时不能复用未核实的旧标签");
        },
    );
}

#[test]
fn labels_isolate_simultaneous_pulls_and_discard_racing_updates() {
    with_label_devices(
        "labels_isolate_simultaneous_pulls_and_discard_racing_updates",
        |d| {
            use contract::InstanceId;
            use runtime::labels::Labels;
            let resolve = |serial: &str| {
                Labels::new(&d.root, &InstanceId::new(serial))
                    .resolve(
                        adb::Device::injected(&d.adb, serial),
                        &d.root.join("aapt"),
                        &[PKG.to_owned()],
                    )
                    .remove(PKG)
            };
            let path = "/data/app/~~p==/base.apk";
            let (a, b) = ("device-a", "device-b");
            label_package(d, a, path, 1, "甲");
            label_package(d, b, path, 1, "乙");
            let names = std::thread::scope(|s| {
                let left = s.spawn(|| resolve(a));
                let right = s.spawn(|| resolve(b));
                (left.join().unwrap(), right.join().unwrap())
            });
            assert_eq!(names, (Some("甲".into()), Some("乙".into())));
            let dest = |serial| {
                fs::read_to_string(d.root.join(format!("{serial}.pulls")))
                    .unwrap()
                    .trim()
                    .to_owned()
            };
            let (left, right) = (dest(a), dest(b));
            assert_ne!(left, right);
            for dest in [left, right] {
                assert!(!std::path::Path::new(&dest).exists());
            }

            label_package(d, a, path, 2, "更新期间已失效");
            fs::write(
                d.root.join(format!("{a}.after")),
                format!("package:/data/app/new/base.apk={PKG} versionCode:3"),
            )
            .unwrap();
            assert_eq!(resolve(a), None);
            label_package(d, a, path, 2, "新快照");
            assert_eq!(resolve(a).as_deref(), Some("新快照"));
            label_package(d, a, path, 3, "");
            fs::write(
                d.root.join(format!("{a}.payload")),
                "package: name='dev.unrelated' versionCode='3'\napplication-label:'wrong'",
            )
            .unwrap();
            assert_eq!(resolve(a), None);
            fs::write(d.root.join(format!("{a}.payload")), format!("package: name='{PKG}' versionCode='3'\n  E: application (line=2)\n    A: android:label(0x01010001)=\"清单字面量\" (Raw: \"清单字面量\")\n")).unwrap();
            assert_eq!(resolve(a).as_deref(), Some("清单字面量"));
        },
    );
}

#[test]
fn installs_non_launcher_with_matching_long_version() {
    with_fixture("installs_non_launcher_with_matching_long_version", |d| {
        d.metadata(MANIFEST);
        d.installer("Performing Streamed Install\nSuccess", "", 0);
        d.shell(PACKAGES, INDEX);
        assert_eq!(d.install(), Outcome::Installed);
    });
}

#[test]
fn incomplete_metadata_prevents_install() {
    with_fixture("incomplete_metadata_prevents_install", |d| {
        for xml in [
            MANIFEST.replace("android:versionCode(", "android:other("),
            MANIFEST.replace(PKG, "bad;name"),
            "no manifest".into(),
        ] {
            d.metadata(&xml);
            assert!(matches!(
                d.install(),
                Outcome::Failed {
                    code: ErrorCode::InvalidApk,
                    ..
                }
            ));
        }
    });
}

#[test]
fn mismatches_missing_receipt_and_unreadable_device_are_unverified() {
    with_fixture(
        "mismatches_missing_receipt_and_unreadable_device_are_unverified",
        |d| {
            for (receipt, index) in [
                ("Success", INDEX.replace("4294967338", "42")),
                ("Success", INDEX.replace(PKG, "dev.fixture.app2")),
                ("Performing Streamed Install", INDEX.into()),
                ("Success", "cmd: Can't find service: package".into()),
                ("Success", "".into()),
            ] {
                d.metadata(MANIFEST);
                d.installer(receipt, "", 0);
                d.shell(PACKAGES, &index);
                assert!(
                    matches!(d.install(), Outcome::Unverified { detail } if detail.contains(PKG))
                );
            }
        },
    );
}

#[test]
fn classifies_install_errors_from_either_stream() {
    with_fixture("classifies_install_errors_from_either_stream", |d| {
        for (stdout, stderr, expected) in [
            (
                "Failure",
                "adb: failed: INSTALL_FAILED_UPDATE_INCOMPATIBLE",
                ErrorCode::SignatureMismatch,
            ),
            (
                "Success",
                "Failure [INSTALL_FAILED_VERSION_DOWNGRADE]",
                ErrorCode::VersionDowngrade,
            ),
            (
                "Requested internal only, but not enough space",
                "",
                ErrorCode::OutOfSpace,
            ),
            (
                "",
                "Failure [INSTALL_PARSE_FAILED_NO_CERTIFICATES]",
                ErrorCode::InvalidApk,
            ),
            (
                "Failure [INSTALL_FAILED_NO_MATCHING_ABIS]",
                "",
                ErrorCode::AdbFailure,
            ),
        ] {
            d.metadata(MANIFEST);
            d.installer(stdout, stderr, 0);
            assert!(
                matches!(d.install(), Outcome::Failed { code, detail } if code == expected && detail != "Failure")
            );
        }
    });
}

#[test]
fn uninstall_requires_a_valid_package_index() {
    with_fixture("uninstall_requires_a_valid_package_index", |d| {
        d.shell("pm uninstall -k dev.fixture.app", "Success");
        d.shell(PACKAGES, INDEX);
        assert!(install::uninstall(d.device(), PKG, true).is_err());
        d.returns(&["adb", "-s", SERIAL, "uninstall", PKG], "Success", "", 0);
        d.shell(PACKAGES, &INDEX.replace(PKG, "dev.fixture.app2"));
        assert_eq!(install::uninstall(d.device(), PKG, false), Ok(true));
        d.returns(&["adb", "-s", SERIAL, "uninstall", PKG], "Success", "", 0);
        d.shell(PACKAGES, "package:android versionCode:34\nBAD PACKAGE LINE");
        assert!(install::uninstall(d.device(), PKG, false).is_err());
        for (index, present) in [
            (INDEX.to_string(), true),
            (INDEX.replace(PKG, "dev.fixture.app2"), false),
        ] {
            d.returns(
                &["adb", "-s", SERIAL, "uninstall", PKG],
                "Failure [DELETE_FAILED_INTERNAL_ERROR]",
                "",
                1,
            );
            d.shell(PACKAGES, &index);
            let result = install::uninstall(d.device(), PKG, false);
            if present {
                assert!(result.is_err())
            } else {
                assert_eq!(result, Ok(false))
            }
        }
    });
}

#[test]
fn home_is_confirmed_by_resolving_the_same_user() {
    with_fixture("home_is_confirmed_by_resolving_the_same_user", |d| {
        let query = "cmd package query-activities --brief --user 0 -a android.intent.action.MAIN -c android.intent.category.HOME";
        let resolve = "cmd package resolve-activity --brief --user 0 -a android.intent.action.MAIN -c android.intent.category.HOME";
        let component = "dev.fixture.app/.Home$Entry";
        assert!(catalog::set_default_home(d.device(), "broken;package").is_err());
        d.shell(query, "1 activities found:\ndev.fixture.app2/.Home");
        assert_eq!(catalog::set_default_home(d.device(), PKG), Ok(false));
        for actual in ["dev.fixture.app2/.Shell", component] {
            d.shell(
                query,
                &format!("2 activities found:\nandroid.example/.Home\n{component}"),
            );
            d.shell(
                "cmd package set-home-activity --user 0 'dev.fixture.app/.Home$Entry'",
                "Success",
            );
            d.shell(resolve, &format!("priority=0 preferredOrder=0\n{actual}"));
            let r = catalog::set_default_home(d.device(), PKG);
            if actual == component {
                assert_eq!(r, Ok(true))
            } else {
                assert!(r.is_err())
            }
        }
    });
}

#[test]
fn activities_distinguish_background_displays_cached_and_exiting_apps() {
    with_fixture(
        "activities_distinguish_background_displays_cached_and_exiting_apps",
        |d| {
            d.shell(ACTIVITIES, &format!("{EMPTY}  * Task{{aa #18 A=10001:dev.fixture.app U=0}}\n    * Hist  #0: ActivityRecord{{a u0 dev.fixture.app/.Main t18}}\n  mLastPausedActivity: ActivityRecord{{b u0 dev.cached/.Gone t9}}\n  * Task{{b #44 A=10001:dev.taskonly U=0}}\nDisplay #3 (activities from top to bottom):\n  topResumedActivity=ActivityRecord{{c u0 dev.fixture.app/.Notes t31}}\n    * Hist  #1: ActivityRecord{{c u0 dev.fixture.app/.Notes t31}}\n    * Hist  #0: ActivityRecord{{d u0 dev.fixture.app/.Main t31}}\n    * Hist  #2: ActivityRecord{{e u10 dev.other.user/.Main t4}}\n    * Hist  #3: ActivityRecord{{f u0 dev.exiting/.Main t5 f}}}}\n"));
            let tasks = running::snapshot(d.device()).unwrap();
            assert_eq!(tasks.len(), 2);
            assert_eq!(
                tasks
                    .iter()
                    .map(|t| (t.package.as_str(), t.task_id, t.display_id, t.resumed))
                    .collect::<Vec<_>>(),
                [(PKG, 18, 0, false), (PKG, 31, 3, true)]
            );
            d.returns(&["adb", "-s", SERIAL, "shell", ACTIVITIES], EMPTY, "", 0);
            assert!(
                running::snapshot(d.device()).is_err(),
                "截断的输出无法证明任务已结束"
            );
            d.shell(ACTIVITIES, "Can't find service: activity");
            assert!(running::snapshot(d.device()).is_err());
        },
    );
}

#[test]
fn launch_rechecks_the_resumed_package_on_display_zero() {
    with_fixture("launch_rechecks_the_resumed_package_on_display_zero", |d| {
        let resolve = "cmd package resolve-activity --brief --user 0 -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -p dev.fixture.app";
        let start = "am start -W --user 0 --display 0 -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n 'dev.fixture.app/.Main'";
        d.shell(resolve, "No activity found");
        assert_eq!(catalog::launch(d.device(), PKG), Ok(false));
        for (display, resumed) in [(3, true), (0, false), (0, true)] {
            d.shell(
                resolve,
                "priority=0 preferredOrder=0\ndev.fixture.app/.Main",
            );
            d.shell(start, "Status: ok\nLaunchState: HOT");
            let mut state = EMPTY.to_owned();
            if display != 0 {
                state.push_str(&format!(
                    "Display #{display} (activities from top to bottom):\n"
                ));
            }
            let record = "ActivityRecord{a3 u0 dev.fixture.app/.Main t41}";
            state.push_str(&format!("   * Hist  #0: {record}\n"));
            if resumed {
                state.push_str(&format!("  Resumed: {record}\n"));
            }
            d.shell(ACTIVITIES, &state);
            assert_eq!(
                catalog::launch(d.device(), PKG).is_ok(),
                display == 0 && resumed
            );
        }
    });
}

const OWN_RECENTS: &str = "ACTIVITY MANAGER RECENT TASKS (dumpsys activity recents)\n  Recent tasks:\n  * Recent #0: Task{a #8 type=standard U=0}\n    mActivityComponent=dev.fixture.app2/.Shell\n    mCallingPackage=dev.fixture.app\n  * Recent #1: Task{b #13 type=standard U=0}\n    intent={act=android.intent.action.MAIN cmp=dev.fixture.app/.Main}\n    taskId=13 rootTaskId=13\n  * Recent #2: Task{c #4 type=standard U=10}\n    mActivityComponent=dev.fixture.app/.Main\n  * RecentTaskInfo #1:\n    id=8 userId=0 hasTask=true\n    topActivity={dev.fixture.app/.Sub}\n";

#[test]
fn force_stop_verifies_package_active_tasks_and_recent_cards() {
    with_fixture(
        "force_stop_verifies_package_active_tasks_and_recent_cards",
        |d| {
            let state = "Packages:\n  Package [dev.fixture.app] (d3bc):\n    User 0: installed=true hidden=false stopped=true enabled=0\n";
            for first in [true, false] {
                d.shell("am force-stop --user 0 dev.fixture.app", "");
                d.shell("dumpsys package dev.fixture.app", state);
                d.shell(ACTIVITIES, EMPTY);
                d.shell(RECENTS, OWN_RECENTS);
                d.shell(RECENTS, OWN_RECENTS);
                d.shell(ACTIVITIES, EMPTY);
                d.shell("am stack remove 13", "");
                d.shell(ACTIVITIES, EMPTY);
                d.shell(RECENTS, if first { NO_RECENTS } else { OWN_RECENTS });
                assert_eq!(running::force_stop(d.device(), PKG).is_ok(), first);
            }
            d.shell("am force-stop --user 0 dev.fixture.app", "");
            d.shell(
                "dumpsys package dev.fixture.app",
                &state.replace("stopped=true", "stopped=false"),
            );
            assert!(running::force_stop(d.device(), PKG).is_err());
        },
    );
}

#[test]
fn auto_exit_cleanup_spares_resumed_and_shared_tasks() {
    with_fixture("auto_exit_cleanup_spares_resumed_and_shared_tasks", |d| {
        d.shell(ACTIVITIES, EMPTY);
        d.shell(RECENTS, OWN_RECENTS);
        d.shell(RECENTS, OWN_RECENTS);
        d.shell(
            ACTIVITIES,
            &format!("{EMPTY}  * Hist  #0: ActivityRecord{{a u0 dev.other/.Foreign t13}}"),
        );
        assert!(running::clear_inactive_recents(d.device(), PKG).is_err());
        d.shell(
            ACTIVITIES,
            &format!("{EMPTY}  * Hist  #0: ActivityRecord{{a u0 dev.fixture.app/.Main t20}}"),
        );
        assert!(running::clear_inactive_recents(d.device(), PKG).is_err());
        d.shell(ACTIVITIES, EMPTY);
        d.shell(
            RECENTS,
            &OWN_RECENTS.replace("taskId=13 rootTaskId=13", "taskId=8 rootTaskId=13"),
        );
        assert!(running::clear_inactive_recents(d.device(), PKG).is_err());
        d.shell(ACTIVITIES, EMPTY);
        d.shell(RECENTS, "unrecognised response");
        assert!(running::clear_inactive_recents(d.device(), PKG).is_err());
    });
}
