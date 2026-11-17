//! 状态机与失败分类的测试。不需要 GUI，不需要 Android，不需要子进程。

use super::*;

fn dir() -> Facts {
    Facts { dir_exists: true, ..Default::default() }
}

fn running() -> Facts {
    Facts { dir_exists: true, process_alive: true, ever_launched: true, ..Default::default() }
}

fn ready() -> Facts {
    Facts {
        services: Services { activity: true, package: true, window: true },
        adb_visible: true,
        boot_completed: true,
        ..running()
    }
}

#[test]
fn absent_without_dir() {
    assert_eq!(State::of(&Facts::default()), State::Absent);
}

#[test]
fn created_but_never_started_is_stopped_not_failed() {
    let f = dir();
    assert_eq!(State::of(&f), State::Stopped);
}

#[test]
fn process_exit_after_launch_is_failed() {
    let f = Facts { ever_launched: true, ..dir() };
    assert_eq!(State::of(&f), State::Failed);
}

#[test]
fn process_exit_after_stop_request_is_stopped() {
    let f = Facts { ever_launched: true, stop_requested: true, ..dir() };
    assert_eq!(State::of(&f), State::Stopped);
}

#[test]
fn stop_requested_while_alive_is_stopping() {
    let f = Facts { stop_requested: true, ..ready() };
    assert_eq!(State::of(&f), State::Stopping);
}

#[test]
fn ready_needs_all_three_services() {
    // 实测：pm 可用时只有两个服务，此时不该 Ready。
    let f = Facts {
        services: Services { activity: false, package: true, window: false },
        adb_visible: true,
        boot_completed: false,
        ..running()
    };
    assert_eq!(State::of(&f), State::Starting);
    assert!(!State::of(&f).accepts_stream());
}

#[test]
fn two_of_three_services_is_not_ready() {
    for missing in 0..3 {
        let s = Services {
            activity: missing != 0,
            package: missing != 1,
            window: missing != 2,
        };
        let f = Facts { services: s, adb_visible: true, ..running() };
        assert_eq!(State::of(&f), State::Starting, "missing idx {missing}");
        assert!(!s.ready());
    }
}

#[test]
fn all_services_without_boot_completed_is_ready() {
    // Ready 的门限是服务齐备，不是 boot_completed——后者晚约 2 秒。
    let f = Facts { boot_completed: false, ..ready() };
    assert_eq!(State::of(&f), State::Ready);
    assert!(State::of(&f).accepts_stream());
}

#[test]
fn sessions_drive_ready_streaming() {
    let f = Facts { session_count: 2, ..ready() };
    assert_eq!(State::of(&f), State::Streaming);
    let f = Facts { session_count: 0, ..ready() };
    assert_eq!(State::of(&f), State::Ready);
}

#[test]
fn stalled_requires_alive_process() {
    let f = Facts { boot_stalled: true, adb_visible: true, ..running() };
    assert_eq!(State::of(&f), State::Stalled);
    // 超时但进程已死：明确失败，不是 Stalled。
    let f = Facts { boot_stalled: true, ..dir() };
    assert_eq!(State::of(&f), State::Stopped);
}

#[test]
fn stalled_reports_where_it_sticks() {
    let waiting_adb = Facts { boot_stalled: true, ..running() };
    assert_eq!(Phase::of(&waiting_adb), Some(Phase::WaitingAdb));

    let warming = Facts { boot_stalled: true, adb_visible: true, ..running() };
    assert_eq!(Phase::of(&warming), Some(Phase::Warming));

    let starting = Facts {
        boot_stalled: true,
        adb_visible: true,
        services: Services { package: true, activity: false, window: false },
        ..running()
    };
    assert_eq!(Phase::of(&starting), Some(Phase::Starting));
}

#[test]
fn phase_is_none_when_not_starting() {
    assert_eq!(Phase::of(&ready()), None);
    assert_eq!(Phase::of(&dir()), None);
    assert_eq!(Phase::of(&Facts::default()), None);
}

#[test]
fn stalled_does_not_accept_stream() {
    let f = Facts { boot_stalled: true, ..running() };
    assert!(!State::of(&f).accepts_stream());
}

#[test]
fn state_is_pure_function_of_facts() {
    // 同一事实集必须得到同一状态；状态没有独立存储，也就没有漂移的可能。
    let f = ready();
    assert_eq!(State::of(&f), State::of(&f));
    // 事实不变时状态不变，与调用次数无关。
    let a = State::of(&f);
    for _ in 0..100 { assert_eq!(State::of(&f), a); }
}

// --- 失败分类 ---

#[test]
fn classify_adb_install_failures() {
    assert_eq!(
        ErrorCode::from_adb_stderr("Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Package x signatures do not match previously installed version]"),
        ErrorCode::SignatureMismatch
    );
    assert_eq!(
        ErrorCode::from_adb_stderr("Failure [INSTALL_FAILED_VERSION_DOWNGRADE]"),
        ErrorCode::VersionDowngrade
    );
    assert_eq!(
        ErrorCode::from_adb_stderr("adb: failed to install: Requested internal only, but not enough space"),
        ErrorCode::OutOfSpace
    );
    assert_eq!(
        ErrorCode::from_adb_stderr("Failure [INSTALL_FAILED_INVALID_APK: ... code is missing]"),
        ErrorCode::InvalidApk
    );
    assert_eq!(
        ErrorCode::from_adb_stderr("Failure [INSTALL_PARSE_FAILED_NO_CERTIFICATES]"),
        ErrorCode::InvalidApk
    );
    assert_eq!(
        ErrorCode::from_adb_stderr("Failure [DELETE_FAILED_INTERNAL_ERROR]"),
        ErrorCode::PackageNotFound
    );
    assert_eq!(ErrorCode::from_adb_stderr("Success"), ErrorCode::Other);
}

#[test]
fn adb_success_text_is_not_a_classification() {
    // adb install 在「已存在同签名版本」时输出 Success 却没真更新。
    // 分类函数不把 Success 当作任何错误，调用方必须回读验证。
    assert_eq!(ErrorCode::from_adb_stderr("Success"), ErrorCode::Other);
    assert_eq!(ErrorCode::from_adb_stderr(""), ErrorCode::Other);
}

// --- 序列化：接口必须可跨进程传递 ---

#[test]
fn command_roundtrips_through_json() {
    let cmds = vec![
        Command::Create {
            spec: Spec {
                template: "phone-1080".into(),
                image: Image { api: 34, tag: "google_apis_playstore".into(), abi: "arm64-v8a".into() },
                disk_bytes: 8 << 30,
            },
        },
        Command::Delete,
        Command::Start,
        Command::Stop,
        Command::Install { apk_path: "/tmp/a.apk".into() },
        Command::Uninstall { package: PackageName::new("com.x"), keep_data: true },
        Command::RefreshApps,
        Command::Launch { package: PackageName::new("com.x") },
    ];
    for c in cmds {
        let js = serde_json::to_string(&c).unwrap();
        let back: Command = serde_json::from_str(&js).unwrap();
        assert_eq!(c, back, "roundtrip failed for {js}");
    }
}

#[test]
fn event_roundtrips_and_keeps_cursor() {
    let e = Event::StateChanged {
        seq: 7,
        from: Some(State::Starting),
        to: State::Stalled,
        phase: Some(Phase::Warming),
        facts: running(),
    };
    let js = serde_json::to_string(&e).unwrap();
    let back: Event = serde_json::from_str(&js).unwrap();
    assert_eq!(e, back);
    assert_eq!(back.seq(), 7);
}

#[test]
fn snapshot_roundtrips() {
    let s = Snapshot {
        serial: "emulator-5554".into(),
        id: InstanceId::new("i-1"),
        state: State::Ready,
        phase: None,
        facts: ready(),
        apps: vec![AppEntry {
            launcher: true,
            package: PackageName::new("com.android.settings"),
            system: true,
            label: Some("设置".into()),
            icon_key: None,
        }],
        seq: 3,
    };
    let js = serde_json::to_string(&s).unwrap();
    let back: Snapshot = serde_json::from_str(&js).unwrap();
    assert_eq!(s, back);
}

#[test]
fn wire_format_is_camel_case_kebab_enum() {
    // 前端在 TSX 侧消费这些值，字段名固定，改了就是破坏性变更。
    let s = Snapshot {
        serial: "emulator-5554".into(),
        id: InstanceId::new("i-1"),
        state: State::Stalled,
        phase: Some(Phase::WaitingAdb),
        facts: Facts::default(),
        apps: vec![],
        seq: 0,
    };
    let v: serde_json::Value = serde_json::to_value(&s).unwrap();
    assert_eq!(v["state"], "stalled");
    assert_eq!(v["phase"], "waiting-adb");
    assert!(v["facts"]["bootCompleted"].is_boolean());
    assert!(v["facts"]["services"]["package"].is_boolean());
    assert_eq!(v["id"], "i-1");
}

#[test]
fn rejection_separates_expected_from_error() {
    // 两类拒绝在线上格式里必须可区分：前端据此决定是提示「稍后再试」还是上报错误。
    let expected = Rejection::WrongState { current: State::Starting, wanted: "ready".into() };
    let actual = Rejection::Error { code: ErrorCode::NoFreePort, detail: "all 16 in use".into() };

    let je: serde_json::Value = serde_json::to_value(&expected).unwrap();
    let ja: serde_json::Value = serde_json::to_value(&actual).unwrap();

    assert_eq!(je["kind"], "wrongState");
    assert_eq!(ja["kind"], "error");
    assert!(ja["code"] == "no-free-port");
    assert_ne!(je["kind"], ja["kind"]);
}

#[test]
fn ack_distinguishes_accepted_from_rejected() {
    let ok = Ack::Accepted { seq: 5 };
    let no = Ack::Rejected { reason: Rejection::NotReady { state: State::Starting } };
    assert!(ok.is_accepted());
    assert!(!no.is_accepted());
    // 受理不等于完成：Accepted 只带游标，不带结果。
    let v: serde_json::Value = serde_json::to_value(&ok).unwrap();
    assert_eq!(v["status"], "accepted");
    assert_eq!(v["seq"], 5);
}
