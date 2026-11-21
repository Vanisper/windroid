//! 通过产品后端启动受控的真实子进程。故障仅注入本用例独占的实例和采集器。
use contract::{Backend, Command, Facts};
use runtime::{
    EmulatorBackend, Layout, app_log,
    instances::{InstanceRecord, InstanceStore},
};
use std::fs;
use std::path::Path;
use std::process::Command as Process;
use std::time::{Duration, Instant};
use windroid_diagnostics::{Catalog, read_page};

pub(super) const ORPHAN: &str = "WINDROID_TEST_ORPHAN_INSTANCE";

pub(super) fn collector_wrapper(root: &Path) {
    super::executable(
        &root.join("collector"),
        r#"#!/bin/sh
echo "$$" > "$WINDROID_BACKEND_TEST_DIR/collector-pid"
exec "$WINDROID_TEST_COLLECTOR_BIN" "$@"
"#,
    );
}

/// 由父测试启动一次，继承相同的隔离 adb 服务和组件路径。
pub(super) fn orphan_host(backend: &mut EmulatorBackend) -> ! {
    backend
        .exec(&Command::Start, &mut Facts::default())
        .unwrap();
    // 跳过 OwnedEmulator::drop 及所有刷新。保留模拟器与采集器各自的进程组。
    std::process::exit(17)
}

pub(super) fn faults(
    test: &str,
    layout: &Layout,
    instance: &InstanceRecord,
    backend: &mut EmulatorBackend,
    facts: &mut Facts,
) {
    let root = &layout.root;
    let id = instance.id.as_str();
    let logs = layout.instance(&instance.id).join("logs");
    let mode = root.join("log-test-mode");
    let permit = root.join("emit-after-host");
    let app_id = app_log::AppLog::allocate_id();
    let app = app_log::AppLog::open(root, &app_id);
    let history = || Catalog::load(root, Some(id)).unwrap().runs;
    let issue = || app_log::capture_issue(root, Some(id));
    let start = |backend: &mut EmulatorBackend, facts: &mut Facts| {
        backend.exec(&Command::Start, facts).unwrap()
    };

    // 输出目录不可创建时走空设备，且能取得无输出子进程的真实退出结果。
    let before = history().len();
    let saved = logs.with_file_name("saved-logs");
    fs::rename(&logs, &saved).unwrap();
    fs::write(&logs, "占用目录名称").unwrap();
    fs::write(&mode, "quiet").unwrap();
    start(backend, facts);
    assert!(issue().unwrap().contains("采集不可用"));
    super::await_observation(backend, facts, |f| !f.process_alive);
    fs::remove_file(&logs).unwrap();
    fs::rename(&saved, &logs).unwrap();
    assert_eq!(history().len(), before);

    start(backend, facts);
    super::await_observation(backend, facts, |f| !f.process_alive);
    let quiet = history().remove(0);
    let page = read_page(root, Some(id), &quiet.id, None, 0, 100).unwrap();
    assert!(quiet.outcome.as_deref().unwrap().contains("exit status: 0"));
    assert!(page.records.iter().all(|r| r.source == "windroid"));
    assert_eq!(quiet.app_run_id.as_deref(), Some(app_id.as_str()));
    assert!(issue().is_none());

    // 仅结束本轮登记的采集器。超过管道容量的双流仍可完全写出并自行退出。
    fs::write(&mode, "flood").unwrap();
    start(backend, facts);
    let broken = history().remove(0).id;
    let pid: i32 = fs::read_to_string(root.join("collector-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_for(|| issue().is_some_and(|s| s.contains("意外退出")));
    fs::write(&permit, "").unwrap();
    super::await_observation(backend, facts, |f| !f.process_alive);
    assert!(root.join("both-streams-drained").is_file());
    let repaired = history().remove(0);
    assert_eq!(repaired.id, broken);
    assert!(repaired.lost && !repaired.active);
    assert!(
        repaired
            .outcome
            .as_deref()
            .unwrap()
            .contains("exit status: 37")
    );
    let recovered = read_page(root, Some(id), &broken, None, 0, 100).unwrap();
    assert!(recovered.records.iter().any(|r| r.source == "gap"));
    app.finish("测试关闭");
    let persisted = read_page(root, None, &app_id, None, 0, 100).unwrap();
    for expected in ["意外退出", "exit status: 37"] {
        assert!(
            persisted
                .records
                .iter()
                .any(|r| r.instance_id.as_deref() == Some(id)
                    && r.emulator_run_id.as_deref() == Some(&broken)
                    && r.text.contains(expected))
        );
    }

    // 再由一个新的宿主创建进程并直接退出；父测试确认退出后，才允许 stderr 继续输出。
    fs::remove_file(&permit).unwrap();
    fs::write(&mode, "orphan").unwrap();
    let status = Process::new(std::env::current_exe().unwrap())
        .args(["--exact", test])
        .env(ORPHAN, id)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(17));
    let active = history().remove(0);
    assert!(active.active);
    let store = InstanceStore::new(layout.clone());
    assert!(store.delete(&instance.id).is_err());
    fs::write(&permit, "").unwrap();
    wait_for(|| history().iter().any(|r| r.id == active.id && !r.active));
    let sealed = history().into_iter().find(|r| r.id == active.id).unwrap();
    assert!(sealed.outcome.is_none());
    let page = read_page(root, Some(id), &sealed.id, None, 0, 100).unwrap();
    for (source, text) in [("stdout", "emulator-stdout"), ("stderr", "after-host-exit")] {
        assert!(
            page.records
                .iter()
                .any(|r| r.source == source && r.text == text)
        );
    }
    fs::remove_file(&mode).unwrap();
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "采集状态变更超时");
        std::thread::sleep(Duration::from_millis(10));
    }
}
