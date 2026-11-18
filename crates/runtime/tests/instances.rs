//! 实例持久化和真实后端的边界测试。设备命令均进入临时目录里的替身。
#[path = "instances/support.rs"]
mod support;

use contract::{
    Backend, CatalogStatus, Command, ErrorCode, Facts, InstanceId, PackageName, Rejection, State,
};
use runtime::{
    EmulatorBackend, Layout,
    instances::{InstanceStore, physical_memory_mib, ram_mib},
};
use std::fs;
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command as ChildCommand;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use support::{Temp, avd, spec};

// 每个套件可独占一个实例库，端口仍属于宿主机。
static TEST_PORTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn selection_and_avds_survive_reopening_and_deleting_the_selected_instance() {
    let _ports = TEST_PORTS.lock().unwrap();
    let root = Temp::create();
    let layout = Layout {
        root: root.0.clone(),
    };
    let store = InstanceStore::new(layout.clone());
    assert_eq!(store.selected().unwrap(), None);
    let a = store.create("  工作 A  ", spec("compact-720")).unwrap();
    let mut wide = spec("tv-4k");
    wide.disk_bytes = 9 << 30;
    let b = store.create("工作 B", wide.clone()).unwrap();
    assert_eq!(a.name, "工作 A");
    assert_ne!(a.id, b.id);
    assert_ne!(a.avd_name(), b.avd_name());
    assert_ne!(a.port, b.port);
    assert_eq!((a.port % 2, b.port % 2), (0, 0));
    let reopened = InstanceStore::new(layout.clone());
    assert_eq!(reopened.list().unwrap(), vec![a.clone(), b.clone()]);
    assert_eq!(reopened.selected().unwrap(), Some(b.id.clone()));
    reopened.select(&a.id).unwrap();
    assert_eq!(store.selected().unwrap(), Some(a.id.clone()));
    for (record, template, w, h, density) in [
        (&a, "compact-720", 1280, 720, 240),
        (&b, "tv-4k", 3840, 2160, 320),
    ] {
        let dir = avd(&root.0, record);
        let saved = record.config(&layout);
        assert_eq!(saved.cpu_cores, Some(2));
        assert_eq!(saved.ram_mib, Some(ram_mib(template) as u32));
        assert_eq!(
            (saved.screen_width, saved.screen_height),
            (Some(w), Some(h))
        );
        assert_eq!(saved.spec.disk_bytes, record.spec.disk_bytes);
        let config = fs::read_to_string(dir.join("config.ini")).unwrap();
        for expected in [
            format!("hw.lcd.width={w}"),
            format!("hw.lcd.height={h}"),
            format!("hw.lcd.density={density}"),
            format!("hw.ramSize={}", ram_mib(template)),
            format!("disk.dataPartition.size={}", record.spec.disk_bytes),
            format!("AvdId={}", record.avd_name()),
        ] {
            assert!(config.lines().any(|l| l == expected), "缺少 {expected}");
        }
        assert!(!config.contains("forceColdBoot=yes"));
        let pointer = dir
            .parent()
            .unwrap()
            .join(format!("{}.ini", record.avd_name()));
        assert!(
            fs::read_to_string(pointer)
                .unwrap()
                .lines()
                .any(|l| l == format!("path={}", dir.display()))
        );
    }
    // 旧清单没有专门的硬件字段，展示磁盘上已有的值。残缺项不套用当前模板默认值。
    let config_path = avd(&root.0, &a).join("config.ini");
    fs::write(
        &config_path,
        "hw.cpu.ncore = 4\nhw.ramSize=invalid\nhw.lcd.width=1\nhw.lcd.width=2",
    )
    .unwrap();
    let saved = reopened.get(&a.id).unwrap().config(&layout);
    assert_eq!(saved.cpu_cores, Some(4));
    assert_eq!(saved.ram_mib, None);
    assert_eq!((saved.screen_width, saved.screen_height), (None, None));
    fs::remove_file(config_path).unwrap();
    assert_eq!(reopened.get(&a.id).unwrap().config(&layout).spec, a.spec);
    assert!(physical_memory_mib().unwrap() > 0);
    store.delete(&a.id).unwrap();
    store.delete(&a.id).unwrap();
    assert_eq!(reopened.selected().unwrap(), Some(b.id.clone()));
    assert!(avd(&root.0, &b).is_dir());
    store.delete(&b.id).unwrap();
    assert_eq!(reopened.selected().unwrap(), None);
}

#[test]
fn concurrent_creators_reserve_separate_pairs_and_section_injection_is_rejected() {
    let _ports = TEST_PORTS.lock().unwrap();
    let root = Temp::create();
    let store = InstanceStore::new(Layout {
        root: root.0.clone(),
    });
    let gate = Arc::new(Barrier::new(4));
    let mut ports = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let gate = gate.clone();
                let store = store.clone();
                scope.spawn(move || {
                    gate.wait();
                    store
                        .create(&format!("并行 {i}"), spec("phone-1080"))
                        .unwrap()
                        .port
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    ports.sort_unstable();
    assert!(ports.windows(2).all(|p| p[1] >= p[0] + 2));
    let mut bad = spec("phone-1080");
    bad.image.tag = "../elsewhere".into();
    assert!(store.create("坏镜像", bad).is_err());
    assert!(store.create("含\n换行", spec("tv-1080")).is_err());
    assert!(store.create(" ", spec("tv-1080")).is_err());
    let mut too_large = spec("compact-720");
    too_large.disk_bytes = i64::MAX as u64;
    assert_eq!(
        store.create("盘", too_large).unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    assert!(
        store
            .delete(&InstanceId::new("../not-an-instance"))
            .is_err()
    );
    let target = root.0.join("external");
    fs::create_dir(&target).unwrap();
    symlink(&target, root.0.join("instances/linked")).unwrap();
    assert!(store.delete(&InstanceId::new("linked")).is_err());
    assert!(target.is_dir());
    assert_eq!(store.list().unwrap().len(), 4);
}

fn executable(path: &Path, source: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn bounded_device_backend_checks_ports_process_identity_and_services() {
    let _ports = TEST_PORTS.lock().unwrap();
    const TEST: &str = "bounded_device_backend_checks_ports_process_identity_and_services";
    const ROOT: &str = "WINDROID_BACKEND_TEST_DIR";
    let Some(path) = std::env::var_os(ROOT) else {
        let dir = Temp::create();
        let sdk = dir.0.join("explicit SDK");
        let image = sdk
            .join("system-images/android-34/google_apis_playstore")
            .join(concat!("arm64", "-v8a"));
        fs::create_dir_all(&image).unwrap();
        for part in [
            "system.img",
            "vendor.img",
            "kernel-ranchu",
            "ramdisk.img",
            "userdata.img",
        ] {
            fs::write(image.join(part), part).unwrap();
        }
        executable(
            &sdk.join("emulator/emulator"),
            r#"#!/bin/sh
printf '%s\n' "$@" > "$WINDROID_BACKEND_TEST_DIR/args"
printf '%s' "$ANDROID_AVD_HOME" > "$WINDROID_BACKEND_TEST_DIR/avd_home"
printf '%s' "$ANDROID_EMULATOR_HOME" > "$WINDROID_BACKEND_TEST_DIR/emulator_home"
while test "$1" != -datadir; do shift; done
snapshot_dir="$2/snapshots/default_boot"
save() {
  if test -e "$WINDROID_BACKEND_TEST_DIR/save-on-exit"; then
    mkdir -p "$snapshot_dir" && echo snapshot >> "$snapshot_dir/snapshot.pb"
  fi
  exit 0
}
trap save TERM
while :; do sleep 60; done
"#,
        );
        let adb = sdk.join("platform-tools/adb");
        executable(
            &adb,
            r#"#!/bin/sh
printf '%s\n' "$4" >> "$WINDROID_BACKEND_TEST_DIR/calls"
case "$4" in
  'getprop ro.boot.qemu.avd_name') cat "$WINDROID_BACKEND_TEST_DIR/identity" ;;
  'getprop ro.boot.qemu.avd_name; getprop sys.boot_completed; service list')
    cat "$WINDROID_BACKEND_TEST_DIR/identity"; echo; echo 1; cat "$WINDROID_BACKEND_TEST_DIR/services" ;;
  'pm list packages -3 --user 0') echo package:dev.sample ;;
  'pm list packages -s --user 0') echo package:android ;;
  'cmd package query-activities'*'category.LAUNCHER')
    if test -e "$WINDROID_BACKEND_TEST_DIR/catalog-failure"; then echo 'Error: package manager unavailable' >&2; exit 1; fi
    echo dev.sample/.Main ;;
  'cmd package query-activities'*'category.HOME') echo dev.desktop/.Home ;;
  'cmd package resolve-activity'*'category.HOME') echo dev.desktop/.Home ;;
  *) exit 1 ;;
esac
"#,
        );
        let result = ChildCommand::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(ROOT, &dir.0)
            .env("WINDROID_HOME", &dir.0)
            .env("WINDROID_SDK", &sdk)
            .env("WINDROID_ADB", &adb)
            .env("WINDROID_AVD", "someone-elses-avd")
            .status()
            .unwrap();
        assert!(result.success());
        return;
    };
    let root = std::path::PathBuf::from(path);
    let layout = Layout { root: root.clone() };
    let store = InstanceStore::new(layout.clone());
    let a = store.create("工作 A", spec("compact-720")).unwrap();
    let mut backend = EmulatorBackend::new(a.id.clone(), layout.clone(), a.port);
    let mut facts = Facts::default();
    backend.observe(&mut facts);
    assert_eq!(State::of(&facts), State::Stopped);
    // 占用任一半都必须拒绝启动。停机观察、abort 与 Stop 不应通过 adb 接管或杀掉陌生设备。
    for port in [a.port, a.port + 1] {
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        assert!(matches!(
            backend.exec(&Command::Start, &mut facts),
            Err(Rejection::Error {
                code: ErrorCode::NoFreePort,
                ..
            })
        ));
        assert!(store.delete(&a.id).is_err());
        backend.abort(&mut facts);
        backend.exec(&Command::Stop, &mut facts).unwrap();
        assert!(occupied.local_addr().is_ok());
        assert!(!root.join("calls").exists());
    }
    // 上次进程号即使已被其他程序复用，也只能用来阻止操作。
    let pid_marker = layout.instance(&a.id).join("active.pid");
    fs::write(&pid_marker, std::process::id().to_string()).unwrap();
    assert_eq!(
        store.delete(&a.id).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(backend.exec(&Command::Start, &mut facts).is_err());
    backend.exec(&Command::Stop, &mut facts).unwrap();
    assert!(pid_marker.exists());
    fs::remove_file(pid_marker).unwrap();
    let hardware = avd(&root, &a).join("hardware-qemu.ini");
    fs::write(&hardware, "hw.cpu.ncore=8\nhw.ramSize=8192").unwrap();
    backend.exec(&Command::Start, &mut facts).unwrap();
    assert!(store.delete(&a.id).is_err());
    let mut another = EmulatorBackend::new(a.id.clone(), layout.clone(), a.port);
    assert!(
        another
            .exec(&Command::Start, &mut Facts::default())
            .is_err()
    );
    fs::write(root.join("identity"), "wrong-avd").unwrap();
    fs::write(
        root.join("services"),
        "1 activity: [interface]\n2 package: [interface]\n3 window: [interface]\n",
    )
    .unwrap();
    backend.observe(&mut facts);
    assert!(facts.process_alive && !facts.adb_visible && !facts.services.ready());
    assert!(a.verify_device().is_err());
    assert!(
        backend
            .exec(
                &Command::ClearData {
                    package: PackageName::new("dev.sample")
                },
                &mut facts
            )
            .is_err()
    );
    assert!(
        !fs::read_to_string(root.join("calls"))
            .unwrap()
            .contains("pm clear")
    );
    fs::write(root.join("identity"), a.avd_name()).unwrap();
    fs::write(root.join("services"), "1 activity_task: [interface]\n2 package_native: [interface]\n3 windowinfo: [interface]\n4 activity: [interface]").unwrap();
    await_observation(&mut backend, &mut facts, |f| f.adb_visible);
    assert!(facts.services.activity && !facts.services.package && !facts.services.window);
    fs::write(
        root.join("services"),
        "1 activity: [interface]\n2 package: [interface]\n3 window: [interface]",
    )
    .unwrap();
    await_observation(&mut backend, &mut facts, |f| f.services.ready());
    assert_eq!(backend.catalog_status(), CatalogStatus::Pending);
    assert!(
        !fs::read_to_string(root.join("calls"))
            .unwrap()
            .contains("pm list packages")
    );
    assert!(
        facts.allocation.is_none(),
        "前次启动遗留的有效参数不能算作本次观测"
    );
    fs::write(&hardware, "hw.cpu.ncore = 2\nhw.ramSize = 2560").unwrap();
    await_observation(&mut backend, &mut facts, |f| f.allocation.is_some());
    let allocation = facts.allocation.unwrap();
    assert_eq!(
        (allocation.cpu_cores, allocation.ram_mib),
        (Some(2), Some(2560))
    );
    assert_eq!(a.config(&layout).ram_mib, Some(1536));
    assert_eq!(State::of(&facts), State::Ready);
    assert_eq!(backend.catalog_status(), CatalogStatus::Ready);
    assert_eq!(backend.apps()[0].package.as_str(), "dev.sample");
    fs::write(root.join("catalog-failure"), "").unwrap();
    assert!(backend.exec(&Command::RefreshApps, &mut facts).is_err());
    assert!(matches!(
        backend.catalog_status(),
        CatalogStatus::Failed { .. }
    ));
    assert_eq!(backend.apps()[0].package.as_str(), "dev.sample");
    assert_eq!(backend.desktop().default.unwrap().as_str(), "dev.desktop");
    let data = avd(&root, &a);
    let args = fs::read_to_string(root.join("args")).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("emulator_home")).unwrap(),
        layout.instance(&a.id).to_str().unwrap()
    );
    assert_eq!(
        fs::read_to_string(root.join("avd_home")).unwrap(),
        data.parent().unwrap().to_str().unwrap()
    );
    for (flag, value) in [
        ("-avd", a.avd_name()),
        ("-datadir", data.to_str().unwrap()),
        ("-data", data.join("userdata-qemu.img").to_str().unwrap()),
    ] {
        assert!(args.contains(&format!("{flag}\n{value}\n")));
    }
    assert!(!args.contains("someone-elses-avd") && !args.contains("-no-snapshot"));
    fs::write(root.join("save-on-exit"), "").unwrap();
    assert!(!store.quick_boot_saved(&a.id).unwrap());
    backend.exec(&Command::Stop, &mut facts).unwrap();
    assert!(store.quick_boot_saved(&a.id).unwrap());
    assert!(facts.allocation.is_none());
    // 下一次退出时若没有写入，磁盘上残留的旧快照不能让界面误报“已冻结”。
    fs::remove_file(root.join("save-on-exit")).unwrap();
    backend.exec(&Command::Start, &mut facts).unwrap();
    assert!(!store.quick_boot_saved(&a.id).unwrap());
    await_observation(&mut backend, &mut facts, |f| f.services.ready());
    assert!(facts.allocation.is_none());
    backend.exec(&Command::Stop, &mut facts).unwrap();
    assert!(!store.quick_boot_saved(&a.id).unwrap());
    assert_eq!(backend.catalog_status(), CatalogStatus::Pending);
    assert_eq!(State::of(&facts), State::Stopped);
    assert!(!layout.instance(&a.id).join("active.pid").exists());
    assert!(
        !fs::read_to_string(root.join("calls"))
            .unwrap()
            .contains("emu kill")
    );
    store.delete(&a.id).unwrap();
}

fn await_observation(
    backend: &mut EmulatorBackend,
    facts: &mut Facts,
    done: impl Fn(&Facts) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        backend.observe(facts);
        if done(facts) {
            return;
        }
        assert!(Instant::now() < deadline, "观察超时 {facts:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
