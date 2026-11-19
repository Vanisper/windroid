//! 两个独立的工作区按正、反两种次序接入同根或父子根，再真实启动各自已登记的实例。
#![cfg(target_os = "macos")]

#[path = "instances/guest.rs"]
mod guest;
#[path = "instances/support.rs"]
mod support;

use contract::{Backend, Command, Facts, InstanceId};
use guest::{boot, guest, stop};
use runtime::instances::{InstanceRecord, InstanceStore};
use runtime::storage::{StorageLocations, SwitchMode, VersionStatus, apply, preview_switch};
use runtime::{EmulatorBackend, Layout};
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use support::{Temp, avd, spec};

fn prepare_resources(source: &Path, root: &Path) {
    for (package, target) in [
        ("runtime", root.to_owned()),
        ("system-images/android-34", root.join("android-34")),
    ] {
        assert!(
            Process::new("/bin/cp")
                .arg("-cR")
                .arg(source.join(package))
                .arg(target)
                .status()
                .unwrap()
                .success(),
            "需要与测试临时目录同卷的 APFS 官方资源"
        );
    }
}

fn attach(layout: &Layout, target: &StorageLocations, runtime_first: bool) {
    runtime::workspace::open(&layout.root).unwrap();
    // 两次确认与设置中的两个原生选择器对应，第一根先接入不应排斥第二根。
    let mut selected = StorageLocations::defaults(&layout.root);
    for runtime in [runtime_first, !runtime_first] {
        if runtime {
            selected.runtime_root = target.runtime_root.clone();
        } else {
            selected.image_root = target.image_root.clone();
        }
        let review = preview_switch(&layout.root, &selected, SwitchMode::Use, false).unwrap();
        assert!(review.blockers.is_empty(), "{:?}", review.blockers);
        assert_eq!(review.required_bytes, 0);
        assert_eq!(
            review
                .versions
                .iter()
                .filter(|v| v.status == VersionStatus::Reuse)
                .count(),
            if runtime { 4 } else { 1 }
        );
        apply(&layout.root, &review).unwrap();
    }
    assert_eq!(StorageLocations::load(&layout.root).unwrap(), *target);
}

fn create(layout: &Layout, id: &str, port: u16) -> InstanceRecord {
    let id = InstanceId::new(id);
    let mut backend = EmulatorBackend::new(id.clone(), layout.clone(), port);
    let spec = spec("compact-720");
    backend
        .exec(
            &Command::Create { spec: spec.clone() },
            &mut Facts::default(),
        )
        .unwrap();
    let record = InstanceStore::new(layout.clone()).get(&id).unwrap();
    assert_eq!(record.spec.image, spec.image);
    assert_eq!(record.port, port);
    record
}

fn run_pair(root: &Path, nested: bool, reverse: bool) {
    let parent = root.join("official");
    let child = parent.join("more-versions");
    let first = StorageLocations {
        runtime_root: parent.clone(),
        image_root: if nested {
            child.clone()
        } else {
            parent.clone()
        },
    };
    let second = StorageLocations {
        runtime_root: if nested { child } else { parent.clone() },
        image_root: parent,
    };
    let pair = root.join(format!("nested-{nested}-reverse-{reverse}"));
    let a = Layout {
        root: pair.join("a"),
    };
    let b = Layout {
        root: pair.join("b"),
    };
    println!(
        "接入：{}；次序 {}",
        if nested { "父子根" } else { "同一根" },
        if reverse { "B → A" } else { "A → B" }
    );
    let open_and_create = |layout: &Layout, target: &StorageLocations, id, port, runtime_first| {
        attach(layout, target, runtime_first);
        create(layout, id, port)
    };
    // 先接入的一方在第二个工作区选择目录前就已有实例。
    let (ar, br) = if reverse {
        let br = open_and_create(&b, &second, "workspace-b", 5572, false);
        (open_and_create(&a, &first, "workspace-a", 5570, true), br)
    } else {
        let ar = open_and_create(&a, &first, "workspace-a", 5570, false);
        (ar, open_and_create(&b, &second, "workspace-b", 5572, true))
    };
    let (mut vm_a, mut facts_a) = boot(&a, &ar);
    // A 保持运行，共享版本的下一位读者仍能启动。写入在两台设备中使用同一客户机路径。
    assert_eq!(guest(&vm_a, &ar, "getprop ro.build.version.sdk"), "34");
    guest(&vm_a, &ar, "echo A > /data/local/tmp/windroid-shared-roots");
    let (mut vm_b, mut facts_b) = boot(&b, &br);
    assert_eq!(guest(&vm_b, &br, "getprop ro.build.version.sdk"), "34");
    guest(
        &vm_b,
        &br,
        "test ! -e /data/local/tmp/windroid-shared-roots",
    );
    guest(&vm_b, &br, "echo B > /data/local/tmp/windroid-shared-roots");
    for (vm, layout, record, selection, value) in [
        (&vm_a, &a, &ar, &first, "A"),
        (&vm_b, &b, &br, &second, "B"),
    ] {
        let active = vm.active_runtime().current().unwrap();
        assert_eq!(active.paths.runtime_root, selection.runtime_root);
        assert_eq!(active.paths.images, selection.image_root);
        let cfg = fs::read_to_string(avd(&layout.root, record).join("config.ini")).unwrap();
        let image = &record.spec.image;
        let sysdir = selection.image_root.join(format!(
            "android-{}/{}/{}/r{}",
            image.api, image.tag, image.abi, image.revision
        ));
        assert!(
            cfg.lines()
                .any(|line| line == format!("image.sysdir.1={}/", sysdir.display()))
        );
        assert_eq!(
            guest(vm, record, "cat /data/local/tmp/windroid-shared-roots"),
            value
        );
    }
    stop(&mut vm_b, &mut facts_b);
    stop(&mut vm_a, &mut facts_a);
}

#[test]
#[ignore = "需要 APFS 上的官方 macOS ARM64 组件和 API 34 r14，使用 5570、5572 两台真实模拟器"]
fn workspaces_boot_with_shared_and_nested_version_directories_in_either_selection_order() {
    const TEST: &str =
        "workspaces_boot_with_shared_and_nested_version_directories_in_either_selection_order";
    if let Some(root) = std::env::var_os("WINDROID_TEST_SHARED_ROOTS_CHILD") {
        let root = PathBuf::from(root);
        for nested in [false, true] {
            for reverse in [false, true] {
                run_pair(&root, nested, reverse);
            }
        }
        return;
    }
    let source = PathBuf::from(
        std::env::var_os("WINDROID_TEST_RESOURCES")
            .expect("请指定含 runtime 和 system-images 的官方展开目录"),
    );
    let temp = Temp::create();
    let root = &temp.0;
    prepare_resources(&source, &root.join("official"));
    prepare_resources(&source, &root.join("official/more-versions"));
    for name in ["user", "android-user", "tmp"] {
        fs::create_dir(root.join(name)).unwrap();
    }
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    let result = Process::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--ignored", "--nocapture"])
        .env("WINDROID_TEST_SHARED_ROOTS_CHILD", root)
        .env("HOME", root.join("user"))
        .env("TMPDIR", root.join("tmp"))
        .env("ANDROID_USER_HOME", root.join("android-user"))
        .env_remove("ANDROID_HOME")
        .env_remove("ANDROID_SDK_ROOT")
        .env_remove("ANDROID_ADB_SERVER_ADDRESS")
        // 禁用 adb 对常规模拟器端口的自动扫描。模拟器启动时会向选定的服务登记。
        .env("ADB_LOCAL_TRANSPORT_MAX_PORT", "5553")
        .env("ANDROID_ADB_SERVER_PORT", &port)
        .env("ADB_SERVER_SOCKET", format!("tcp:localhost:{port}"))
        .status()
        .unwrap();
    // 只回收此用例使用的本地 adb 服务。
    let _ = Process::new(root.join("official/platform-tools/37.0.1/adb"))
        .args(["-P", &port, "kill-server"])
        .env_remove("ADB_SERVER_SOCKET")
        .output();
    assert!(result.success(), "双工作区真实启动用例失败");
}
