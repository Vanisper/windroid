//! 真实模拟器的集成测试。
//!
//! 与其它测试不同，这里**真的启动模拟器**。因此默认忽略：它需要 SDK、AVD、
//! 一分钟以上的启动时间，不适合每次 cargo test 都跑。
//!
//! 跑法：
//!   WINDROID_AVD=tv14 cargo test -p runtime -- --ignored --nocapture
//!
//! 这个测试的价值在于它是唯一能证明「事实来自真实 adb」的地方。假后端能
//! 证明状态机自洽，证明不了状态机与真实世界对得上。

use std::time::{Duration, Instant};

use contract::{Backend, Command, Facts, InstanceId, Spec, State};
use runtime::{EmulatorBackend, Layout};

fn spec() -> Spec {
    Spec {
        template: "tv-1080".into(),
        image: contract::Image { api: 34, tag: "google_apis_playstream".into(), abi: "arm64".into() },
        disk_bytes: 8 << 30,
    }
}

#[test]
#[ignore = "启动真实模拟器，需要 SDK 与 AVD"]
fn real_adb_facts_drive_the_state_machine() {
    let layout = Layout::default_root();
    let mut backend = EmulatorBackend::new(InstanceId::new("it-1"), layout, 5556);

    let mut facts = Facts::default();
    backend.exec(&Command::Create { spec: spec() }, &mut facts).expect("create");
    assert!(facts.dir_exists, "创建后目录必须存在");

    backend.exec(&Command::Start, &mut facts).expect("start");
    assert!(facts.process_alive, "启动后进程必须活着");
    assert_eq!(State::of(&facts), State::Starting);

    // 等就绪。真实启动通常 20-60 秒。
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut last = State::Starting;
    while Instant::now() < deadline {
        backend.observe(&mut facts);
        let now = State::of(&facts);
        if now != last {
            println!("[state] {:?} -> {:?} facts={:?}", last, now, facts);
            last = now;
        }
        if now == State::Ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    assert_eq!(State::of(&facts), State::Ready, "真实模拟器应达到 Ready: {facts:?}");
    assert!(facts.adb_visible, "Ready 时 adb 必须可见");
    assert!(facts.services.ready(), "Ready 时三个服务必须齐备");

    backend.exec(&Command::Stop, &mut facts).expect("stop");
    backend.observe(&mut facts);
    assert!(!facts.process_alive, "停止后进程必须退出");
    assert_eq!(State::of(&facts), State::Stopped);
}
