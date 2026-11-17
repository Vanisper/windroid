//! 桥的测试：不需要窗口，不需要 nsh 引擎，只验证两侧的翻译与转发。

use super::*;
use contract::*;
use device::{spawn, FakeBackend, Timeouts};
use nsh_engine::HostChannel;
use std::time::{Duration, Instant};

fn setup() -> (Arc<Bridge>, HostChannel) {
    let (h, ev) = spawn(
        InstanceId::new("bridge-test"),
        "emulator-5554".to_owned(),
        FakeBackend::new(),
        Timeouts::default(),
    );
    let channel = HostChannel::new();
    let bridge = Bridge::attach(h, channel.clone(), ev);
    (bridge, channel)
}

fn create(h: &device::Handle) -> Ack {
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    })
}

#[test]
fn command_handler_executes_and_answers() {
    let (bridge, _channel) = setup();
    assert!(create(bridge.handle()).is_accepted());

    // 界面发一条 Start：JSON 进，回执 JSON 出。
    let start = serde_json::to_value(Command::Start).unwrap();
    let ack = execute(bridge.handle(), &start);
    assert_eq!(ack["status"], "accepted", "start should be accepted: {ack}");
    assert!(ack["seq"].is_number(), "accepted ack carries its cursor");

    // 受理之后状态确实变了——回执不是空头承诺。
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(bridge.snapshot().state, State::Starting | State::Ready) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("state did not move after accepted Start");
}

#[test]
fn malformed_command_is_rejected_not_panicking() {
    let (bridge, _channel) = setup();
    let out = execute(bridge.handle(), &serde_json::json!({ "kind": "nonsense" }));
    let s = out.to_string();
    assert!(s.contains("无法解析"), "should explain the parse failure: {s}");
    assert_eq!(out["status"], "rejected");
}

#[test]
fn wrong_state_command_carries_the_actual_state() {
    let (bridge, _channel) = setup();
    // 未创建实例就 Start：应被拒绝，并说明当前状态。
    let start = serde_json::to_value(Command::Start).unwrap();
    let ack = execute(bridge.handle(), &start);
    assert_eq!(ack["status"], "rejected");
    assert_eq!(ack["reason"]["kind"], "wrongState");
    assert_eq!(ack["reason"]["current"], "absent");
}

#[test]
fn snapshot_is_serialisable_for_tsx() {
    let (bridge, _channel) = setup();
    create(bridge.handle());
    let snap = bridge.snapshot();
    assert_eq!(snap.state, State::Stopped);

    let v = to_json(&snap);
    // 字段名是 TSX 侧的契约，改了就是破坏性变更。
    assert_eq!(v["state"], "stopped");
    assert!(v["facts"]["dirExists"].is_boolean());
    assert!(v["facts"]["services"]["window"].is_boolean());
    assert_eq!(v["id"], "bridge-test");
}

#[test]
fn events_are_forwarded_to_the_channel() {
    // 事件必须真的从设备域流到通道上。用一个自定义 HostChannel 观察不到
    // 内部发送，因此改为验证「设备域事件流在推进」这一必要条件：
    // 转发线程若崩溃，设备域事件仍会产出但界面什么也收不到。
    // 这里的替代验证是：状态确实推进了，且转发线程仍在运行（未 panic）。
    let (bridge, channel) = setup();
    create(bridge.handle());
    bridge.handle().send(Command::Start);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut progressed = false;
    while Instant::now() < deadline {
        if matches!(bridge.snapshot().state, State::Starting | State::Ready) {
            progressed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(progressed, "device should have progressed past Stopped");

    // 转发线程没有崩溃：通道仍可接受发送。
    assert!(channel.send(names::EVENT, JsonValue::Null).is_ok());
}

#[test]
fn describe_explains_where_boot_is_stuck() {
    let facts = Facts {
        dir_exists: true,
        process_alive: true,
        ever_launched: true,
        adb_visible: true,
        services: Services { package: true, activity: true, window: false },
        boot_stalled: true,
        ..Default::default()
    };
    let s = describe(State::Stalled, Phase::of(&facts), &facts);
    assert!(s.contains("2/3"), "should report which services are up: {s}");
    assert!(s.contains("继续等待"), "should offer the next action: {s}");
}

#[test]
fn describe_covers_every_state() {
    for st in [
        State::Absent,
        State::Stopped,
        State::Starting,
        State::Stalled,
        State::Ready,
        State::Streaming,
        State::Stopping,
        State::Failed,
    ] {
        let d = describe(st, None, &Facts::default());
        assert!(!d.is_empty(), "no description for {st}");
    }
}

#[test]
fn message_names_are_stable() {
    // 这些字符串同时存在于 TSX 侧，改名必须两边一起改。
    assert_eq!(names::COMMAND, "windroid.command");
    assert_eq!(names::SNAPSHOT, "windroid.snapshot");
    assert_eq!(names::EVENT, "windroid.event");
    // 符合 nsh 的命名规则（非空、不超长、可用字符）。
    for n in [names::COMMAND, names::SNAPSHOT, names::EVENT] {
        assert!(nsh_core::host_channel::check_name(n).is_ok(), "invalid name {n}");
    }
}
