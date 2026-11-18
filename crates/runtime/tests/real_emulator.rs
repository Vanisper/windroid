//! 用显式开发 SDK 在临时实例库中依次启动 A、B、A，验证真实 adb、数据隔离和 Quick Boot。
//! 指定 WINDROID_SDK，然后运行 cargo test -p runtime --test real_emulator -- --ignored --nocapture。

#[path = "instances/support.rs"]
mod support;

use contract::{Backend, Command, Facts, State};
use runtime::{
    EmulatorBackend, Layout,
    instances::{InstanceRecord, InstanceStore},
};
use std::fs;
use std::process::Command as ChildCommand;
use std::time::{Duration, Instant};
use support::{Temp, avd, spec};

fn guest(record: &InstanceRecord, cmd: &str) -> String {
    record.verify_device().unwrap();
    let out = adb::shell(&record.serial(), cmd).unwrap();
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    out.stdout.trim().to_owned()
}

const TEST: &str = "real_adb_facts_drive_the_state_machine";
const CHILD: &str = "WINDROID_INSTANCE_REAL_CHILD";

fn boot(layout: &Layout, record: &InstanceRecord) -> (EmulatorBackend, Facts) {
    let mut backend = EmulatorBackend::new(record.id.clone(), layout.clone(), record.port);
    let mut facts = Facts::default();
    backend.observe(&mut facts);
    assert_eq!(State::of(&facts), State::Stopped);
    backend
        .exec(&Command::Start, &mut facts)
        .expect("启动私有 AVD");
    assert_eq!(State::of(&facts), State::Starting);
    println!(
        "已启动实例 {}，端口 {}，私有 AVD {}",
        record.name,
        record.port,
        record.avd_name()
    );
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(240) {
        backend.observe(&mut facts);
        if facts.boot_completed && State::of(&facts) == State::Ready {
            match record.verify_device() {
                Ok(()) => {
                    println!("{} 在 {:?} 后就绪", record.name, started.elapsed());
                    return (backend, facts);
                }
                // 系统首次启动期间 adbd 会暂时离线，下一轮重新核对三个服务和设备身份。
                Err(e) if e.kind() == std::io::ErrorKind::NotConnected => (),
                Err(e) => panic!("核对 {} 私有 AVD 名失败：{e}", record.name),
            }
        }
        assert!(
            facts.process_alive,
            "模拟器提前退出，日志在 {}",
            layout.instance(&record.id).join("emulator.log").display()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("实例 {} 启动未完成，事实为 {facts:?}", record.name);
}

fn stop(backend: &mut EmulatorBackend, facts: &mut Facts) {
    backend.exec(&Command::Stop, facts).expect("优雅关闭");
    backend.observe(facts);
    assert_eq!(State::of(facts), State::Stopped);
    assert!(!facts.adb_visible && !facts.services.ready());
}

#[test]
#[ignore = "需要显式 WINDROID_SDK、API 34 Google Play 镜像与模拟器资源"]
fn real_adb_facts_drive_the_state_machine() {
    if std::env::var_os(CHILD).is_none() {
        assert!(
            std::env::var_os("WINDROID_SDK").is_some_and(|p| !p.is_empty()),
            "请通过 WINDROID_SDK 指定只读 SDK"
        );
        let temp = Temp::create();
        for dir in ["android-user", "emulator", "avd", "tmp"] {
            fs::create_dir(temp.0.join(dir)).unwrap();
        }
        let result = ChildCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--ignored", "--nocapture"])
            .env(CHILD, "1")
            .env("WINDROID_HOME", &temp.0)
            .env("ANDROID_USER_HOME", temp.0.join("android-user"))
            .env("ANDROID_EMULATOR_HOME", temp.0.join("emulator"))
            .env("ANDROID_AVD_HOME", temp.0.join("avd"))
            .env("TMPDIR", temp.0.join("tmp"))
            .env_remove("WINDROID_AVD")
            .env_remove("WINDROID_SERIAL")
            .env_remove("WINDROID_ADB")
            .status()
            .unwrap();
        assert!(result.success());
        return;
    }
    let layout = Layout::default_root();
    assert_eq!(
        layout.root.join("tmp"),
        std::env::temp_dir().canonicalize().unwrap()
    );
    assert!(
        layout
            .root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("windroid-instance-test-")
    );
    let store = InstanceStore::new(layout.clone());
    let a = store.create("A", spec("compact-720")).unwrap();
    let b = store.create("B", spec("phone-1080")).unwrap();
    assert_ne!(a.port, b.port);
    store.select(&a.id).unwrap();
    let (mut backend, mut facts) = boot(&layout, &a);
    // Android 每次完成系统启动都会增加 BOOT_COUNT。内存快照恢复保留已运行的 framework。
    // https://developer.android.com/reference/android/provider/Settings.Global#BOOT_COUNT
    let boot_count = guest(&a, "settings get global boot_count");
    assert!(
        boot_count.parse::<u64>().is_ok_and(|n| n > 0),
        "设备启动次数无效：{boot_count:?}"
    );
    guest(&a, "echo instance-a > /data/local/tmp/windroid-isolation");
    stop(&mut backend, &mut facts);
    assert!(store.quick_boot_saved(&a.id).unwrap());
    let snapshot = avd(&layout.root, &a).join("snapshots/default_boot/snapshot.pb");
    assert!(
        fs::metadata(snapshot).is_ok_and(|m| m.len() > 0),
        "A 的私有 Quick Boot 快照应已保存"
    );
    store.select(&b.id).unwrap();
    let (mut backend, mut facts) = boot(&layout, &b);
    guest(&b, "test ! -e /data/local/tmp/windroid-isolation");
    stop(&mut backend, &mut facts);
    let reopened = InstanceStore::new(layout.clone());
    assert_eq!(reopened.selected().unwrap(), Some(b.id.clone()));
    reopened.select(&a.id).unwrap();
    let (mut backend, mut facts) = boot(&layout, &a);
    assert_eq!(
        guest(&a, "settings get global boot_count"),
        boot_count,
        "应从 A 的内存快照恢复"
    );
    assert_eq!(
        guest(&a, "cat /data/local/tmp/windroid-isolation"),
        "instance-a"
    );
    stop(&mut backend, &mut facts);
    store.delete(&a.id).unwrap();
    store.delete(&b.id).unwrap();
    assert!(reopened.list().unwrap().is_empty());
}
