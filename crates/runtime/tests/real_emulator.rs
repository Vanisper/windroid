//! Apple 芯片实测。WINDROID_TEST_RESOURCES 指向展开后的 runtime 与 system-images 的父目录。
//! 每个用例克隆所需官方版本，使用独立的工作区、Android 用户目录和 adb 服务。
#![cfg(target_os = "macos")]

#[path = "instances/guest.rs"]
mod guest;
#[path = "instances/support.rs"]
mod support;
use guest::{boot, guest, stop};

use contract::{Backend, Command, Facts};
use runtime::{EmulatorBackend, Layout, instances::InstanceStore};
use std::fs;
use std::process::Command as ChildCommand;
use std::time::{Duration, Instant};
use support::{Temp, avd, spec};

fn isolated(name: &str, apis: &[u32]) -> bool {
    use std::path::{Path, PathBuf};
    if std::env::var_os("WINDROID_REAL_EMULATOR_CHILD").is_some() {
        return false;
    }
    let source =
        PathBuf::from(std::env::var_os("WINDROID_TEST_RESOURCES").expect("请提供官方整包展开目录"));
    let temp = Temp::create();
    let root = &temp.0;
    let home = root.join("workspace");
    runtime::workspace::open(&home).unwrap();
    let clone = |relative: &Path| {
        let target = home.join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        assert!(
            ChildCommand::new("/bin/cp")
                .arg("-cR")
                .arg(source.join(relative))
                .arg(target)
                .status()
                .unwrap()
                .success()
        );
    };
    clone(Path::new("runtime"));
    for api in apis {
        clone(&PathBuf::from(format!("system-images/android-{api}")));
    }
    for dir in ["android-user", "user", "tmp", "poison"] {
        fs::create_dir(root.join(dir)).unwrap();
    }
    // PATH 和旧变量指向会留下证据的替身。所有产品命令都应使用绑定版本。
    let trap = root.join("poison/adb");
    use std::os::unix::fs::PermissionsExt;
    fs::write(&trap, "#!/bin/sh\ntouch \"$WINDROID_REAL_TRAP\"\nexit 91\n").unwrap();
    fs::set_permissions(&trap, fs::Permissions::from_mode(0o755)).unwrap();
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    let result = ChildCommand::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--ignored", "--nocapture"])
        .env("WINDROID_REAL_EMULATOR_CHILD", name)
        .env("WINDROID_HOME", &home)
        .env("HOME", root.join("user"))
        .env("ANDROID_USER_HOME", root.join("android-user"))
        .env("TMPDIR", root.join("tmp"))
        // 每个测试服务只接收 Emulator 的显式登记，避免它探测其他工作区的模拟器端口。
        .env("ADB_LOCAL_TRANSPORT_MAX_PORT", "5553")
        .env("ANDROID_ADB_SERVER_PORT", &port)
        .env("ADB_SERVER_SOCKET", format!("tcp:localhost:{port}"))
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                root.join("poison").display()
            ),
        )
        .env("WINDROID_SDK", root.join("poison"))
        .env("WINDROID_ADB", &trap)
        .env("WINDROID_SERVER_JAR", root.join("poison/server.jar"))
        .env("WINDROID_GPU", "invalid-test-value")
        .env("ANDROID_HOME", root.join("poison"))
        .env("ANDROID_SDK_ROOT", root.join("poison"))
        .env("WINDROID_REAL_TRAP", root.join("trap-used"))
        .status()
        .unwrap();
    // 只回收本用例创建在随机端口的服务。
    let adb = home.join("runtime/platform-tools/37.0.1/adb");
    let _ = ChildCommand::new(adb)
        .args(["-P", &port, "kill-server"])
        .env_remove("ADB_SERVER_SOCKET")
        .output();
    assert!(result.success(), "{name}");
    assert!(!root.join("trap-used").exists());
    true
}

#[test]
#[ignore = "需要官方运行组件和 Android 14 镜像"]
fn real_adb_facts_drive_the_state_machine() {
    if isolated("real_adb_facts_drive_the_state_machine", &[34]) {
        return;
    }
    let layout = Layout::default_root();
    let store = InstanceStore::new(layout.clone());
    let a = store.create("A", spec("compact-720")).unwrap();
    let b = store.create("B", spec("phone-1080")).unwrap();
    assert_ne!(a.port, b.port);
    store.select(&a.id).unwrap();
    let (mut backend, mut facts) = boot(&layout, &a);
    // Android 每次完成系统启动都会增加 BOOT_COUNT。内存快照恢复保留已运行的 framework。
    // https://developer.android.com/reference/android/provider/Settings.Global#BOOT_COUNT
    let boot_count = guest(&backend, &a, "settings get global boot_count");
    assert!(
        boot_count.parse::<u64>().is_ok_and(|n| n > 0),
        "设备启动次数无效：{boot_count:?}"
    );
    guest(
        &backend,
        &a,
        "echo instance-a > /data/local/tmp/windroid-isolation",
    );
    stop(&mut backend, &mut facts);
    assert!(store.quick_boot_saved(&a.id).unwrap());
    let snapshot = avd(&layout.root, &a).join("snapshots/default_boot/snapshot.pb");
    assert!(
        fs::metadata(snapshot).is_ok_and(|m| m.len() > 0),
        "A 的私有 Quick Boot 快照应已保存"
    );
    store.select(&b.id).unwrap();
    let (mut backend, mut facts) = boot(&layout, &b);
    guest(&backend, &b, "test ! -e /data/local/tmp/windroid-isolation");
    stop(&mut backend, &mut facts);
    let reopened = InstanceStore::new(layout.clone());
    assert_eq!(reopened.selected().unwrap(), Some(b.id.clone()));
    reopened.select(&a.id).unwrap();
    let (mut backend, mut facts) = boot(&layout, &a);
    assert_eq!(
        guest(&backend, &a, "settings get global boot_count"),
        boot_count,
        "应从 A 的内存快照恢复"
    );
    assert_eq!(
        guest(&backend, &a, "cat /data/local/tmp/windroid-isolation"),
        "instance-a"
    );
    stop(&mut backend, &mut facts);
    store.delete(&a.id).unwrap();
    store.delete(&b.id).unwrap();
    assert!(reopened.list().unwrap().is_empty());
}

/// 从预置官方组件和 API 34 创建实例，再导入 API 33，迁移后核对两个私有盘。
#[test]
#[ignore = "需要官方运行组件、API 34 及 WINDROID_TEST_API33_ZIP"]
fn migrated_images_keep_private_guest_data() {
    use runtime::provision::Provisioner;
    use runtime::storage::{StorageLocations, apply, preview};
    if isolated("migrated_images_keep_private_guest_data", &[34]) {
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
    let create = |name: &str, port, spec| {
        let id = contract::InstanceId::new(name);
        let mut backend = EmulatorBackend::new(id.clone(), layout.clone(), port);
        backend
            .exec(&Command::Create { spec }, &mut Facts::default())
            .unwrap();
        store.get(&id).unwrap()
    };
    let a = create("A", 5562, spec("compact-720"));
    let b = create("B", 5564, a.spec.clone());
    let mut alternate_spec = b.spec.clone();
    alternate_spec.image = alternate;
    let c = create("C", 5566, alternate_spec);
    let mounted = std::env::var_os("WINDROID_TEST_MIGRATION_VOLUME").map(std::path::PathBuf::from);
    let external = mounted.as_deref().map(Temp::in_directory);
    let destination = external.as_ref().map_or(root, |t| t.0.as_path());
    if mounted.is_some() {
        use std::os::unix::fs::MetadataExt;
        let from = fs::metadata(root).unwrap().dev();
        let to = fs::metadata(destination).unwrap().dev();
        assert_ne!(from, to, "必须是另一个实际卷");
        println!("跨卷迁移 dev={from} → dev={to}");
    }
    let target = StorageLocations {
        runtime_root: destination.join("components"),
        image_root: destination.join("images"),
    };
    let (mut vm, mut facts) = boot(&layout, &a);
    guest(&vm, &a, "echo A > /data/local/tmp/windroid-private");
    let moving = preview(&layout.root, &target).unwrap();
    assert!(moving.required_bytes > 0);
    apply(&layout.root, &moving).unwrap();
    assert!(
        vm.active_runtime()
            .current()
            .unwrap()
            .paths
            .adb
            .starts_with(layout.root.join("runtime"))
    );
    assert_eq!(guest(&vm, &a, "cat /data/local/tmp/windroid-private"), "A");
    stop(&mut vm, &mut facts);
    let (mut vm, mut facts) = boot(&layout, &a);
    assert_eq!(guest(&vm, &a, "cat /data/local/tmp/windroid-private"), "A");
    stop(&mut vm, &mut facts);
    let (mut vm, mut facts) = boot(&layout, &b);
    guest(&vm, &b, "test ! -e /data/local/tmp/windroid-private");
    stop(&mut vm, &mut facts);
    let target = StorageLocations {
        image_root: layout.root.join("system-images"),
        ..target
    };
    apply(&layout.root, &preview(&layout.root, &target).unwrap()).unwrap();
    let (mut vm, mut facts) = boot(&layout, &c);
    assert_eq!(guest(&vm, &c, "getprop ro.build.version.sdk"), "33");
    stop(&mut vm, &mut facts);
}

#[test]
#[ignore = "需要官方运行组件与 API 35 镜像"]
fn api35_initializes_private_userdata_from_a_separate_image_directory() {
    if isolated(
        "api35_initializes_private_userdata_from_a_separate_image_directory",
        &[35],
    ) {
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
    assert!(!row.location.starts_with(&p.paths().runtime_root));
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
    assert_eq!(
        guest(&backend, &record, "getprop ro.build.version.sdk"),
        "35"
    );
    guest(
        &backend,
        &record,
        "echo api35 > /data/local/tmp/windroid-image-test",
    );
    assert_eq!(
        guest(&backend, &record, "cat /data/local/tmp/windroid-image-test"),
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

#[test]
#[ignore = "需要官方运行组件与 API 33/34/35 镜像"]
fn resource_limits_use_each_installed_revision_and_its_final_display() {
    if isolated(
        "resource_limits_use_each_installed_revision_and_its_final_display",
        &[33, 34, 35],
    ) {
        return;
    }
    let layout = Layout::default_root();
    let store = InstanceStore::new(layout);
    for (api, revision, minimum) in [(33, 9, 2048), (34, 14, 2560), (35, 9, 2560)] {
        let mut image = spec("phone-1080").image;
        image.api = api;
        image.revision = revision;
        let phone = store.spec_limits("phone-1080", &image).unwrap();
        let large = store.spec_limits("tv-4k", &image).unwrap();
        assert_eq!((phone.min_ram_mib, large.min_ram_mib), (minimum, 4096));
        println!(
            "API {api} r{revision}：手机 {} MiB，4K {} MiB，单台最高 {} 核、{} MiB",
            phone.min_ram_mib, large.min_ram_mib, phone.max_cpu_cores, phone.max_ram_mib
        );
    }
}

#[test]
#[ignore = "使用全新 WINDROID_HOME、受管运行组件与 Android 14 镜像验证冷重启与改配后开机"]
fn stopped_hardware_edits_preserve_the_private_disk_and_bind_the_next_boot() {
    use runtime::instances::{GraphicsMode, Hardware};
    if isolated(
        "stopped_hardware_edits_preserve_the_private_disk_and_bind_the_next_boot",
        &[34],
    ) {
        return;
    }
    let layout = Layout::default_root();
    let store = InstanceStore::new(layout.clone());
    // 配置低于镜像内存下限时，启动预留量应提升，私有 AVD 仍保留保存值。
    let original = store.create("既有实例", spec("compact-720")).unwrap();
    let config_path = avd(&layout.root, &original).join("config.ini");
    let config = fs::read_to_string(&config_path)
        .unwrap()
        .replace("windroid.graphics.batch=false\n", "");
    fs::write(&config_path, &config).unwrap();
    let saved = original.config(&layout);
    assert_eq!(
        (saved.cpu_cores, saved.ram_mib, saved.graphics),
        (Some(2), Some(1536), Some(GraphicsMode::Host))
    );
    assert_eq!(store.launch_hardware(&original).unwrap().ram_mib, 2560);
    assert_eq!(fs::read_to_string(config_path).unwrap(), config);

    let mut chosen = spec("phone-1080");
    chosen.disk_bytes = 6 << 30;
    let first = Hardware {
        cpu_cores: 1,
        ram_mib: 2560,
        graphics: GraphicsMode::Host,
    };
    let second = Hardware {
        cpu_cores: 2,
        ram_mib: 3072,
        graphics: GraphicsMode::Batch,
    };
    let record = store
        .create_with_hardware("规格验证", chosen.clone(), first)
        .unwrap();
    let (mut backend, mut facts) = boot(&layout, &record);
    let allocated = facts.allocation.as_ref().unwrap();
    assert_eq!(
        (allocated.cpu_cores, allocated.ram_mib),
        (Some(1), Some(2560))
    );
    assert!(store.update_hardware(&record.id, second).is_err());
    guest(
        &backend,
        &record,
        "echo retained > /data/local/tmp/windroid-hardware",
    );
    let retained = "cat /data/local/tmp/windroid-hardware";
    assert_eq!(guest(&backend, &record, retained), "retained");
    guest::start_and_wait(
        &layout,
        &record,
        &mut backend,
        &mut facts,
        Command::Restart { cold: true },
    );
    assert_eq!(guest(&backend, &record, retained), "retained");
    guest(
        &backend,
        &record,
        "echo edited > /data/local/tmp/windroid-hardware",
    );
    assert_eq!(guest(&backend, &record, retained), "edited");
    stop(&mut backend, &mut facts);
    store.update_hardware(&record.id, second).unwrap();
    let reopened = InstanceStore::new(layout.clone());
    let saved = reopened.get(&record.id).unwrap();
    assert_eq!(saved.spec, chosen);
    assert_eq!(saved.config(&layout).graphics, Some(GraphicsMode::Batch));
    assert_eq!(reopened.launch_hardware(&saved).unwrap(), second);
    let (mut backend, mut facts) = boot(&layout, &saved);
    let allocated = facts.allocation.as_ref().unwrap();
    assert_eq!(
        (allocated.cpu_cores, allocated.ram_mib),
        (Some(2), Some(3072))
    );
    assert_eq!(
        guest(&backend, &saved, "cat /data/local/tmp/windroid-hardware"),
        "edited"
    );
    stop(&mut backend, &mut facts);
}
