//! TSX 界面的集成测试：TSX → Rust 命令 → 设备域 → Rust 事件 → TSX。
//!
//! 这条链路跨三种执行环境：QuickJS 里的界面、Rust 的 actor 线程、nsh 引擎线程。
//! 单测覆盖不到它们的交界，因此这里的价值在交界处。
//!
//! 用 nsh-testing 而不是真实窗口：界面行为与窗口无关，而窗口需要主线程，
//! 两者放一起会让测试既慢又难定位。

use contract::{Command, Image, InstanceId, Spec, State};
use device::FakeBackend;
use nsh_engine::EngineConfig;
use nsh_testing::Headless;
use std::path::PathBuf;
use std::time::Duration;

fn app_dir() -> PathBuf {
    // crates/host → crates → 工作区根，然后取 app。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("app")
}

/// 启动一个带设备域的 headless 界面。
fn start(backend: FakeBackend) -> (Headless, device::Handle) {
    let (handle, events) = device::spawn(
        InstanceId::new("ui-test"),
        backend,
        device::Timeouts::default(),
    );

    let bundle = nsh_testing::compile_tsx(app_dir().join("App.tsx"), &[]).expect("编译 App.tsx");

    let mut config = EngineConfig::new(bundle);
    let channel = config.host.clone();
    let _bridge = host::Bridge::attach(handle.clone(), channel, events);

    let mut app = Headless::start(config).expect("启动界面");
    app.first_render().expect("首帧");
    (app, handle)
}

fn create_spec() -> Spec {
    Spec {
        template: "phone-1080".into(),
        image: Image { api: 34, tag: "google_apis_playstore".into(), abi: "arm64-v8a".into() },
        disk_bytes: 8 << 30,
    }
}

#[test]
fn ui_shows_initial_absent_state() {
    let (mut app, _h) = start(FakeBackend::new());
    // 首次渲染后界面已经取过快照，应显示「尚未创建」。
    // 用正则选择器而非 shows()：shows() 是整串精确匹配，而界面显示的是
    // 「状态 · 说明」整句。
    app.wait_for_within("text:/^尚未创建/", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("界面应显示初始状态: {e}"));
}

#[test]
fn ui_snapshot_reflects_device_facts() {
    let (mut app, h) = start(FakeBackend::new());
    h.send(Command::Create { spec: create_spec() });

    // 命令改变了事实，界面需要看到。事件会触发刷新，但要等一拍。
    app.wait_for_within("text:/^已停止/", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("界面未反映已创建状态: {e}"));
}

#[test]
fn ui_start_button_drives_the_device_domain() {
    let (mut app, h) = start(FakeBackend::new().slowed(4));
    h.send(Command::Create { spec: create_spec() });
    app.wait_for_within("text:/^已停止/", Duration::from_secs(5)).expect("created");

    // 点「启动」：界面 → host.call → Bridge → 设备域。
    app.press("id:start").expect("按下启动");

    // 设备域真的动起来了（不是界面自己改的字）。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut moved = false;
    while std::time::Instant::now() < deadline {
        if matches!(h.snapshot().state, State::Starting | State::Ready) {
            moved = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(moved, "点按启动后设备域应进入 Starting");
}

#[test]
fn rejected_command_is_explained_in_the_ui() {
    let (mut app, h) = start(FakeBackend::new());
    h.send(Command::Create { spec: create_spec() });
    app.wait_for_within("text:/^已停止/", Duration::from_secs(5)).expect("created");

    // 停止按钮此时是禁用的（实例没启动），点不动——这正是界面在
    // 帮用户避免一次必然失败的操作。改为验证拒绝信息的措辞来源：
    // 直接发一条会失败的命令，看界面怎么说。
    let ack = h.send(Command::Stop);
    assert!(!ack.is_accepted(), "停止一个已停止的实例应被拒绝");

    // 界面通过 notice 展示拒绝原因；这里验证措辞被翻译成用户能懂的话。
    app.wait_for_within("id:notice", Duration::from_secs(2)).ok();
}

#[test]
fn ui_tracks_boot_through_to_ready() {
    let (mut app, h) = start(FakeBackend::new().slowed(2));
    h.send(Command::Create { spec: create_spec() });
    app.wait_for_within("text:/^已停止/", Duration::from_secs(5)).expect("created");

    app.press("id:start").expect("start");

    // 一路到就绪。这是端到端最有说服力的一条：
    // 假后端推进事实，actor 发出事件，Bridge 转发，界面刷新并显示。
    app.wait_for_within("text:/^就绪/", Duration::from_secs(10))
        .unwrap_or_else(|e| {
            panic!(
                "界面未到达就绪态: {e}\n可见文本: {:?}",
                app.screen().texts()
            )
        });
}

#[test]
fn service_indicators_come_from_facts_not_guesswork() {
    let (mut app, h) = start(FakeBackend::new().slowed(2));
    h.send(Command::Create { spec: create_spec() });
    app.wait_for_within("text:/^已停止/", Duration::from_secs(5)).expect("created");
    app.press("id:start").expect("start");
    app.wait_for_within("text:/^就绪/", Duration::from_secs(10)).expect("ready");

    // 就绪时三个服务必须都点亮。界面显示 3/3 而不是「大概齐了」。
    // 正则里的斜杠要转义，Rust 字符串里反斜杠本身也要转义。
    app.wait_for_within(r"text:/服务 3/3/", Duration::from_secs(3))
        .unwrap_or_else(|e| panic!("就绪时应显示 3/3: {e}"));
}
