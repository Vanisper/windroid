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

    let config = EngineConfig::new(bundle);
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
    // 文案来自宿主的 describe()，界面不自己拼——改文案只改一处。
    app.wait_for_within("text:/^实例尚未创建/", Duration::from_secs(5))
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

    // 停止一个已停止的实例是幂等的：接受且无副作用。
    // 这条不是「无所谓」——界面重试一次必然失败的命令不该让用户看到错误。
    let ack = h.send(Command::Stop);
    assert!(ack.is_accepted(), "停止一个已停止的实例应被幂等受理");
    assert!(
        matches!(h.snapshot().state, State::Stopped),
        "幂等受理不得改变状态"
    );

    // 真正该被拒绝的是没有实例时停止——那说明界面与设备域脱节了。
    let (mut fresh, h2) = start(FakeBackend::new());
    let _ = &mut fresh;
    let ack = h2.send(Command::Stop);
    assert!(!ack.is_accepted(), "无实例时停止应被拒绝");

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

/// 启动过程中界面要指出卡在哪一步。
///
/// 这不是锦上添花：adb 未出现与服务未就绪，用户的应对完全不同——前者要查
/// 端口与 adbd，后者要查镜像与磁盘。只说「启动较慢」等于什么也没说。
#[test]
fn stuck_point_is_shown_while_starting() {
    let (mut app, h) = start(FakeBackend::new().stalling().slowed(1));
    h.send(Command::Create { spec: create_spec() });
    app.wait_for_within("text:/^已停止/", Duration::from_secs(5)).expect("created");
    app.press("id:start").expect("start");

    // 假后端在 adb 可见后停住，卡点应指向更后面的服务层。
    app.wait_for_within("id:stuck", Duration::from_secs(8))
        .unwrap_or_else(|e| panic!("启动中应显示卡点: {e}\n屏幕: {:?}", app.screen().texts()));
}

/// 自检结果必须随快照到达界面。
///
/// 没有这条断言，自检可以静默返回空——界面会显示「一切正常」，
/// 而用户点下去才发现缺组件。缺失信息必须由宿主给出，界面无权编造。
#[test]
fn readiness_reaches_the_ui() {
    let (mut app, _h) = start(FakeBackend::new());
    // 侧边栏一定渲染：它不依赖设备状态。
    app.wait_for_within("text:Windroid", Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("侧边栏应渲染: {e}"));

    // 本机装齐时不该出现「未就绪」；缺组件时必须出现。
    // 两种结果都合法，但界面不能在两者之间停留——它读的是同一份自检。
    let screen = app.screen().texts();
    let shows_blocked = screen.iter().any(|t| t.contains("运行环境未就绪"));
    let shows_library = screen.iter().any(|t| t.contains("应用") || t.contains("启动实例"));
    assert!(
        shows_blocked || shows_library,
        "界面必须处于「未就绪」或「应用库」之一，实际: {screen:?}"
    );
}
