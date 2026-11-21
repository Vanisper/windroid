use super::{TEST_PORTS, Temp, avd, spec};
use contract::{Backend, Command, ErrorCode, Facts, InstanceId, Rejection};
use runtime::admission::{Admission, MemoryObservation, MemoryPressure};
use runtime::instances::InstanceStore;
use runtime::provision::RuntimePaths;
use runtime::{ActiveRuntime, EmulatorBackend, Layout};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};

#[test]
fn preview_includes_only_eligible_guests_in_order_and_leaves_no_state() {
    let _ports = TEST_PORTS.lock().unwrap();
    let root = Temp::create();
    let layout = Layout {
        root: root.0.clone(),
    };
    let store = InstanceStore::new(layout.clone());
    let first = store.create("一", spec("compact-720")).unwrap();
    let second = store.create("二", spec("compact-720")).unwrap();
    let lock = layout.instance(&first.id).join("run.lock");
    let stale_pid = layout.instance(&first.id).join("active.pid");
    fs::write(&stale_pid, i32::MAX.to_string()).unwrap();
    let admission_dir = root.0.join("private-admission");
    let runtime = ActiveRuntime::new(|home, _| Ok((RuntimePaths::discover(home), Vec::new())))
        .with_admission(Admission::new(admission_dir.clone(), || {
            MemoryObservation::current(MemoryPressure::Normal, 5050)
        }));
    let paths = RuntimePaths::discover(&root.0);
    let image = paths
        .images
        .join("android-34/google_apis_playstore/arm64-v8a/r14");
    fs::create_dir_all(&image).unwrap();
    fs::create_dir_all(paths.emulator.parent().unwrap()).unwrap();
    fs::write(
        paths.emulator.parent().unwrap().join("source.properties"),
        "Pkg.Revision=37.2.13\nPkg.BuildId=16740729\n",
    )
    .unwrap();
    fs::write(
        image.join("build.prop"),
        "ro.build.version.sdk=34\nro.product.cpu.abi=arm64-v8a\nro.product.name=sdk_gphone64_arm64",
    )
    .unwrap();
    for part in ["system.img", "ramdisk.img", "kernel-ranchu", "vendor.img"] {
        fs::write(image.join(part), []).unwrap();
    }
    let config = avd(&root.0, &first).join("config.ini");
    let original = fs::read(&config).unwrap();
    let ids = [
        first.id.clone(),
        InstanceId::new("removed"),
        second.id.clone(),
    ];
    let result = store.preview_starts(&ids, &runtime);
    assert!(result[0].is_ok(), "{:?}", result[0]);
    assert!(
        result[1]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("已不存在")
    );
    assert!(
        result[2]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("需要 2560 MiB，宿主当前预计可分配 2490 MiB")
    );

    {
        let _listener = TcpListener::bind((Ipv4Addr::LOCALHOST, first.port + 1)).unwrap();
        let checked = store.preview_starts(&ids, &runtime);
        assert_eq!(
            checked[0].as_ref().unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse
        );
        assert!(checked[2].is_ok());
    }
    let mut backend = EmulatorBackend::with_runtime(
        second.id.clone(),
        layout.clone(),
        second.port,
        runtime.clone(),
    );
    for (address, offset) in [
        (IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        (IpAddr::V6(Ipv6Addr::LOCALHOST), 1),
        (IpAddr::V4(Ipv4Addr::UNSPECIFIED), 1),
        (IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    ] {
        let _listener = TcpListener::bind((address, second.port + offset)).unwrap();
        let retry = store.preview_starts(&ids, &runtime);
        assert!(retry[0].is_ok());
        assert_eq!(
            retry[2].as_ref().unwrap_err().kind(),
            std::io::ErrorKind::AddrInUse
        );
        assert!(matches!(
            backend.exec(&Command::Start, &mut Facts::default()),
            Err(Rejection::Error {
                code: ErrorCode::NoFreePort,
                ..
            })
        ));
    }
    // 旧监听已收回时，同端口上尚未退出的连接及随后进入的 TIME_WAIT 均允许重新启动。
    let previous = TcpListener::bind((Ipv4Addr::LOCALHOST, second.port)).unwrap();
    let client = TcpStream::connect(previous.local_addr().unwrap()).unwrap();
    let (accepted, _) = previous.accept().unwrap();
    drop(previous);
    assert!(store.preview_starts(std::slice::from_ref(&second.id), &runtime)[0].is_ok());
    drop(accepted);
    drop(client);
    assert!(store.preview_starts(std::slice::from_ref(&second.id), &runtime)[0].is_ok());

    let held = fs::File::open(layout.instance(&second.id).join("run.lock")).unwrap();
    held.lock().unwrap();
    assert_eq!(
        store.preview_starts(std::slice::from_ref(&second.id), &runtime)[0]
            .as_ref()
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    held.unlock().unwrap();
    assert!(store.preview_starts(std::slice::from_ref(&second.id), &runtime)[0].is_ok());
    fs::remove_file(image.join("vendor.img")).unwrap();
    assert!(
        store.preview_starts(&ids, &runtime)[0]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("vendor.img")
    );

    assert_eq!(fs::read(&config).unwrap(), original);
    assert_eq!(fs::read_to_string(stale_pid).unwrap(), i32::MAX.to_string());
    assert!(!admission_dir.exists() && !lock.exists() && runtime.current().is_none());
    assert!(!layout.instance(&first.id).join("logs").exists());
    // 生产解析器遇到缺失的官方组件；首次预览也不建立工作区写锁或核验缓存。
    for name in [".components.lock", ".images.lock"] {
        fs::remove_file(root.0.join(name)).unwrap();
    }
    fs::rename(
        paths.emulator.parent().unwrap(),
        root.0.join("fixture-emulator"),
    )
    .unwrap();
    let missing = store.preview_starts(&ids, &ActiveRuntime::default());
    assert!(
        missing[0]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("Android Emulator 37.2.13")
    );
    assert!(!root.0.join(".components.lock").exists() && !root.0.join(".images.lock").exists());
    assert!(!root.0.join("cache").exists());
}
