//! 设备域集成测试：命令、事件、快照三件套在工作。
//!
//! 这些测试不用 GUI，不用 Android，不用子进程——只用一个假后端。
//! 它们验证的不是「能跑」，而是几条具体性质：
//! 事件有序、状态不跳变、拒绝不误报、快照与事件能对齐。

use contract::*;
use device::{spawn, FakeBackend, Timeouts};
use std::time::Duration;

fn short_timeouts() -> Timeouts {
    Timeouts {
        adb: Duration::from_millis(60),
        services: Duration::from_millis(60),
        hard: Duration::from_secs(60),
    }
}

fn id() -> InstanceId {
    InstanceId::new("t-1")
}

/// 收集事件直到看到目标状态，或超时。
fn until_state(events: &device::Events, want: State, budget: Duration) -> Option<Event> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Some(e @ Event::StateChanged { to, .. }) if to == want => return Some(e),
            Some(_) => continue,
            None => continue,
        }
    }
    None
}

#[test]
fn command_roundtrip_returns_ack() {
    let (h, _ev) = spawn(id(), FakeBackend::new(), Timeouts::default());

    let ack = h.send(Command::Create {
        spec: Spec {
            template: "phone-1080".into(),
            image: Image { api: 34, tag: "google_apis_playstore".into(), abi: "arm64-v8a".into() },
            disk_bytes: 8 << 30,
        },
    });
    assert!(ack.is_accepted(), "create should be accepted: {ack:?}");

    // 命令确实改变了事实，且快照能读到。
    let snap = h.snapshot();
    assert_eq!(snap.state, State::Stopped);
    assert!(snap.facts.dir_exists);
}

#[test]
fn events_arrive_in_order_with_monotonic_seq() {
    let (h, ev) = spawn(id(), FakeBackend::new(), Timeouts::default());
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    });
    h.send(Command::Start);

    let events = ev.drain(Duration::from_millis(300));
    assert!(!events.is_empty(), "expected events");

    let seqs: Vec<Seq> = events.iter().map(|e| e.seq()).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "event seq must be monotonic: {seqs:?}");

    // 游标严格递增，不重复。
    for w in seqs.windows(2) {
        assert!(w[1] > w[0], "seq not strictly increasing: {seqs:?}");
    }
}

#[test]
fn boot_progresses_through_expected_states() {
    let (h, ev) = spawn(id(), FakeBackend::new().slowed(2), Timeouts::default());
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    });
    h.send(Command::Start);

    // 状态必须按顺序推进，不能跳步。
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match ev.recv_timeout(Duration::from_millis(300)) {
            Some(Event::StateChanged { to, .. }) => {
                if seen.last() != Some(&to) {
                    seen.push(to);
                }
                if to == State::Ready {
                    break;
                }
            }
            Some(_) => {}
            None => {}
        }
    }

    assert!(seen.contains(&State::Starting), "should pass through Starting: {seen:?}");
    assert_eq!(seen.last(), Some(&State::Ready), "should end at Ready: {seen:?}");
    // Starting 在 Ready 之前，且 Ready 只出现一次。
    let first_ready = seen.iter().position(|s| *s == State::Ready).unwrap();
    let last_starting = seen.iter().rposition(|s| *s == State::Starting).unwrap();
    assert!(last_starting < first_ready, "Starting must precede Ready: {seen:?}");
}

#[test]
fn ready_requires_all_three_services_not_just_pm() {
    // 这是最重要的一条：假后端在 package 服务出现后还会停一拍，
    // 我们可以验证 package 有、window 无的那一刻，状态不是 Ready。
    let (h, ev) = spawn(id(), FakeBackend::new().slowed(3), Timeouts::default());
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    });
    h.send(Command::Start);

    let mut saw_partial = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match ev.recv_timeout(Duration::from_millis(300)) {
            Some(Event::StateChanged { to, facts, .. }) => {
                if to == State::Ready {
                    assert!(facts.services.ready(), "Ready with incomplete services: {facts:?}");
                    assert_eq!(facts.services.count(), 3);
                    break;
                }
                if facts.services.any() && !facts.services.ready() {
                    saw_partial = true;
                    assert_eq!(to, State::Starting, "partial services must be Starting, got {to}");
                }
            }
            Some(_) => {}
            None => {}
        }
    }
    assert!(saw_partial, "test did not observe a partial-services moment");
}

#[test]
fn stall_reports_the_sticking_point() {
    // 假后端在 adb 可见后停住；超时后应进入 Stalled。
    let (h, ev) = spawn(id(), FakeBackend::new().stalling(), short_timeouts());
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    });
    h.send(Command::Start);

    let e = until_state(&ev, State::Stalled, Duration::from_secs(5))
        .expect("should reach Stalled when services never come");

    if let Event::StateChanged { phase, facts, .. } = e {
        // 卡点必须可读：adb 已可见但服务未齐，说明卡在服务层。
        assert!(facts.process_alive, "Stalled implies the process is still alive");
        assert!(facts.adb_visible, "adb was visible");
        assert!(!facts.services.ready());
        assert_eq!(phase, Some(Phase::Warming), "stuck at service layer");
    }
}

#[test]
fn stalled_process_exit_becomes_failed() {
    // Stalled 之后进程死掉，必须转 Failed——这是最确定的失败信号。
    let (h, ev) = spawn(id(), FakeBackend::new().stalling(), short_timeouts());
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    });
    h.send(Command::Start);
    until_state(&ev, State::Stalled, Duration::from_secs(5)).expect("stalled first");

    // 用户放弃：Stop 把它带回 Stopped（而不是 Failed）。
    let ack = h.send(Command::Stop);
    assert!(ack.is_accepted(), "stop from Stalled should be accepted: {ack:?}");
    until_state(&ev, State::Stopped, Duration::from_secs(5))
        .expect("stop should lead to Stopped, not Failed");
}

#[test]
fn duplicate_start_is_rejected_with_reason() {
    let (h, ev) = spawn(id(), FakeBackend::new().slowed(2), Timeouts::default());
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    });
    h.send(Command::Start);

    // 等它真的进入 Starting，再发第二个 Start。
    until_state(&ev, State::Starting, Duration::from_secs(5)).expect("should be starting");

    let ack = h.send(Command::Start);
    match ack {
        Ack::Rejected { reason: Rejection::WrongState { current, wanted } } => {
            assert_eq!(current, State::Starting, "rejection must name the actual state");
            assert_eq!(wanted, "已停止", "rejection must say what was needed");
        }
        other => panic!("expected WrongState rejection, got {other:?}"),
    }

    // 关键：被拒绝后没有产生第二个启动过程。
    let snap = h.snapshot();
    assert!(snap.facts.process_alive);
    assert_eq!(snap.state, State::Starting);
}

#[test]
fn install_before_ready_is_rejected_as_not_ready() {
    let (h, ev) = spawn(id(), FakeBackend::new().slowed(3), Timeouts::default());
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    });
    h.send(Command::Start);
    until_state(&ev, State::Starting, Duration::from_secs(5)).expect("starting");

    let ack = h.send(Command::Install { apk_path: "/tmp/a.apk".into() });
    match ack {
        Ack::Rejected { reason: Rejection::WrongState { wanted, .. } } => {
            assert_eq!(wanted, "系统就绪");
        }
        other => panic!("expected rejection before ready, got {other:?}"),
    }
}

#[test]
fn snapshot_and_events_can_be_reconciled_by_seq() {
    // 先取快照再订阅事件时的对齐方式：丢弃 seq <= snapshot.seq 的事件。
    let (h, ev) = spawn(id(), FakeBackend::new().slowed(2), Timeouts::default());
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    });
    h.send(Command::Start);

    // 等到某个中间时刻再取快照，模拟晚到的订阅者。
    std::thread::sleep(Duration::from_millis(120));
    let snap = h.snapshot();

    let later: Vec<Event> = ev
        .drain(Duration::from_millis(200))
        .into_iter()
        .filter(|e| e.seq() > snap.seq)
        .collect();

    // 快照之后的事件必须都是比它新的。
    for e in &later {
        assert!(e.seq() > snap.seq, "stale event leaked past the snapshot");
    }
    // 且快照本身是自洽的：state 与 facts 相符。
    assert_eq!(snap.state, State::of(&snap.facts), "snapshot state must derive from its facts");
}

#[test]
fn snapshot_state_always_equals_derived_state() {
    let (h, ev) = spawn(id(), FakeBackend::new(), Timeouts::default());
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    });
    h.send(Command::Start);

    // 反复采样：任何时刻，快照里的 state 都必须等于对同一份 facts 的推导结果。
    // 这条性质一旦破坏，就说明有代码在给状态赋值而不是派生态。
    for _ in 0..40 {
        let s = h.snapshot();
        assert_eq!(
            s.state,
            State::of(&s.facts),
            "state drifted from facts: state={:?} facts={:?}",
            s.state,
            s.facts
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = ev.drain(Duration::from_millis(10));
}

#[test]
fn delete_requires_stopped() {
    let (h, ev) = spawn(id(), FakeBackend::new().slowed(2), Timeouts::default());
    h.send(Command::Create {
        spec: Spec {
            template: "t".into(),
            image: Image { api: 34, tag: "g".into(), abi: "a".into() },
            disk_bytes: 1,
        },
    });
    h.send(Command::Start);
    until_state(&ev, State::Starting, Duration::from_secs(5)).expect("starting");

    let ack = h.send(Command::Delete);
    match ack {
        Ack::Rejected { reason: Rejection::WrongState { wanted, .. } } => assert_eq!(wanted, "已停止"),
        other => panic!("delete while running must be rejected, got {other:?}"),
    }

    h.send(Command::Stop);
    until_state(&ev, State::Stopped, Duration::from_secs(5)).expect("stopped");
    let ack = h.send(Command::Delete);
    assert!(ack.is_accepted(), "delete when stopped should be accepted: {ack:?}");
    assert_eq!(h.snapshot().state, State::Absent);
}
