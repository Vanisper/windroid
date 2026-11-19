//! 真实模拟器的启动、目标 AVD 身份核验与优雅停止，供隔离的 macOS 集成测试共用。
use contract::{Backend, Command, Facts, State};
use runtime::{EmulatorBackend, Layout, instances::InstanceRecord};
use std::time::{Duration, Instant};

pub fn guest(backend: &EmulatorBackend, record: &InstanceRecord, cmd: &str) -> String {
    let resources = backend.active_runtime().current().unwrap();
    record.verify_device(&resources.paths.adb).unwrap();
    let out = adb::shell(resources.device(&record.serial()), cmd).unwrap();
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    out.stdout.trim().to_owned()
}

pub fn boot(layout: &Layout, record: &InstanceRecord) -> (EmulatorBackend, Facts) {
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
            match record.verify_device(&backend.active_runtime().current().unwrap().paths.adb) {
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

pub fn stop(backend: &mut EmulatorBackend, facts: &mut Facts) {
    backend.exec(&Command::Stop, facts).expect("优雅关闭");
    backend.observe(facts);
    assert_eq!(State::of(facts), State::Stopped);
    assert!(!facts.adb_visible && !facts.services.ready());
}
