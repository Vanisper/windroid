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

/// 从一份已准备的受管 API 34 环境克隆到临时目录，验证发布包和真实数据盘跨目录重开。
#[test]
#[cfg(target_os = "macos")]
#[ignore = "需要受管运行组件、API 34 镜像、官方 API 33 ZIP 和 Apple 芯片 Mac"]
fn migrated_images_keep_private_guest_data() {
    use runtime::provision::Provisioner;
    use runtime::storage::{StorageLocations, apply, preview};
    const CHILD: &str = "WINDROID_MIGRATION_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let runtime =
            std::env::var_os("WINDROID_TEST_RUNTIME").expect("指定含受管 API 34 的运行组件目录");
        let temp = Temp::create();
        let home = temp.0.join("home");
        fs::create_dir(&home).unwrap();
        let source = std::path::PathBuf::from(runtime);
        // 按包克隆，测试库独立登记目录归属，避免复制原工作区的目录标记。
        for relative in [
            "sdk/emulator",
            "sdk/platform-tools",
            "sdk/build-tools/34.0.0",
            "sdk/system-images/android-34/google_apis_playstore/arm64-v8a",
            "scrcpy-server.jar",
        ] {
            let target = home.join("runtime").join(relative);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            assert!(
                ChildCommand::new("cp")
                    .arg("-cR")
                    .arg(source.join(relative))
                    .arg(target)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let exit = ChildCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "migrated_images_keep_private_guest_data",
                "--ignored",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("WINDROID_HOME", home)
            .env("ANDROID_USER_HOME", temp.0.join("android"))
            .env_remove("WINDROID_SDK")
            .env_remove("WINDROID_ADB")
            .env_remove("WINDROID_SERVER_JAR")
            .status()
            .unwrap();
        assert!(exit.success());
        return;
    }
    let layout = Layout::default_root();
    let root = layout.root.parent().unwrap();
    let store = InstanceStore::new(layout.clone());
    let provisioner = Provisioner::managed(&layout.root);
    assert!(provisioner.snapshot().components_ready);
    let archive = std::path::PathBuf::from(
        std::env::var_os("WINDROID_TEST_API33_ZIP").expect("指定官方 API 33 ZIP"),
    );
    let row = provisioner.inspect_image_zip(&archive).unwrap();
    let alternate = row.image.unwrap();
    assert_eq!(alternate.api, 33);
    provisioner.import_image(row.id, archive).unwrap();
    let started = Instant::now();
    loop {
        let view = provisioner.snapshot();
        assert_ne!(view.phase, "failed", "{:?}", view.error);
        if view.images.iter().any(|r| r.id == row.id && r.ready) {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(300));
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(store.list().unwrap().is_empty());
    let a = store.create("A", spec("compact-720")).unwrap();
    let b = store.create("B", a.spec.clone()).unwrap();
    let mut spec = b.spec.clone();
    spec.image = alternate;
    let c = store.create("C", spec).unwrap();
    let target = StorageLocations {
        runtime_root: root.join("components"),
        image_root: Some(root.join("images")),
    };

    let (mut vm, mut facts) = boot(&layout, &a);
    guest(&a, "echo A > /data/local/tmp/windroid-private");
    let moving = preview(&layout.root, &target).unwrap();
    assert!(apply(&layout.root, &moving).is_err());
    stop(&mut vm, &mut facts);
    let moving = preview(&layout.root, &target).unwrap();
    assert!(!moving.files.is_empty());
    apply(&layout.root, &moving).unwrap();
    let (mut vm, mut facts) = boot(&layout, &a);
    assert_eq!(guest(&a, "cat /data/local/tmp/windroid-private"), "A");
    stop(&mut vm, &mut facts);
    let (mut vm, mut facts) = boot(&layout, &b);
    guest(&b, "test ! -e /data/local/tmp/windroid-private");
    stop(&mut vm, &mut facts);
    let target = StorageLocations {
        image_root: None,
        ..target
    };
    apply(&layout.root, &preview(&layout.root, &target).unwrap()).unwrap();
    let (mut vm, mut facts) = boot(&layout, &c);
    assert_eq!(guest(&c, "getprop ro.build.version.sdk"), "33");
    stop(&mut vm, &mut facts);
}

#[test]
#[ignore = "set WINDROID_TEST_RESOURCE_CONFIG to storage settings with managed tools and the official API 35 image"]
fn api35_initializes_private_userdata_from_a_separate_image_directory() {
    const NAME: &str = "api35_initializes_private_userdata_from_a_separate_image_directory";
    const MARKER: &str = "WINDROID_TEST_API35_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let config = std::env::var_os("WINDROID_TEST_RESOURCE_CONFIG")
            .expect("set WINDROID_TEST_RESOURCE_CONFIG");
        let temp = Temp::create();
        for dir in ["android-user", "emulator", "avd", "tmp"] {
            fs::create_dir(temp.0.join(dir)).unwrap();
        }
        let sources: windroid_locations::StorageLocations =
            serde_json::from_reader(fs::File::open(config).unwrap()).unwrap();
        let home = temp.0.join("home");
        let target = windroid_locations::StorageLocations {
            runtime_root: temp.0.join("tools"),
            image_root: Some(temp.0.join("images")),
        };
        let plan = runtime::storage::preview(&home, &target).unwrap();
        runtime::storage::apply(&home, &plan).unwrap();
        let snap = runtime::provision::Provisioner::managed(&home).snapshot();
        let image = snap
            .images
            .into_iter()
            .find(|row| row.id == "system-image-35")
            .unwrap();
        // 逐包复制到本次工作区，独立保留 SDK、镜像根目录各自的归属记录。
        for row in snap.components.into_iter().chain([image]) {
            let source = if row.image.is_some() {
                sources
                    .images()
                    .join(row.location.strip_prefix(target.images()).unwrap())
            } else {
                sources
                    .runtime_root
                    .join(row.location.strip_prefix(&target.runtime_root).unwrap())
            };
            fs::create_dir_all(row.location.parent().unwrap()).unwrap();
            assert!(
                ChildCommand::new("cp")
                    .arg("-cR")
                    .arg(source)
                    .arg(&row.location)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let result = ChildCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--ignored", "--nocapture"])
            .env(MARKER, "1")
            .env("WINDROID_HOME", &home)
            .env("ANDROID_USER_HOME", temp.0.join("android-user"))
            .env("ANDROID_EMULATOR_HOME", temp.0.join("emulator"))
            .env("ANDROID_AVD_HOME", temp.0.join("avd"))
            .env("TMPDIR", temp.0.join("tmp"))
            .env_remove("WINDROID_SDK")
            .env_remove("WINDROID_ADB")
            .env_remove("WINDROID_SERVER_JAR")
            .env_remove("WINDROID_SERIAL")
            .env_remove("WINDROID_AVD")
            .status()
            .unwrap();
        assert!(result.success());
        return;
    }
    let layout = Layout::default_root();
    let p = runtime::provision::Provisioner::managed(&layout.root);
    let mut wanted = spec("compact-720");
    let snap = p.snapshot();
    let row = snap
        .images
        .iter()
        .find(|row| row.id == "system-image-35")
        .unwrap();
    assert!(snap.components_ready && row.ready, "{:?}", row.issue);
    assert!(!row.location.starts_with(&p.paths().sdk));
    assert!(!row.location.join("userdata.img").exists());
    wanted.image = row.image.clone().unwrap();
    let id = contract::InstanceId::new("isolated-api35");
    let mut backend = EmulatorBackend::new(id.clone(), layout.clone(), 5578);
    backend
        .exec(&Command::Create { spec: wanted }, &mut Facts::default())
        .unwrap();
    let store = InstanceStore::new(layout.clone());
    let record = store.get(&id).unwrap();
    let (mut backend, mut facts) = boot(&layout, &record);
    assert_eq!(guest(&record, "getprop ro.build.version.sdk"), "35");
    guest(&record, "echo api35 > /data/local/tmp/windroid-image-test");
    assert_eq!(
        guest(&record, "cat /data/local/tmp/windroid-image-test"),
        "api35"
    );
    assert!(
        avd(&layout.root, &record)
            .join("userdata-qemu.img")
            .is_file()
    );
    stop(&mut backend, &mut facts);
    store.delete(&id).unwrap();
}
