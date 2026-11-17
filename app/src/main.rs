//! Windroid 应用入口。
//!
//! 这一层只做组装：建窗口、接设备域、载入界面。业务逻辑不在这里。
//!
//! 窗口在显示之前必须已经装好关闭拦截代理——否则存在「已显示但关闭拦不住」
//! 的时间窗，而那正是本产品的核心约束之一。

use std::path::PathBuf;

mod streaming;

use contract::{Backend, InstanceId};
use device::spawn;
use host::surface::{CloseIntent, Window};
use nsh_platform_macos::objc2::MainThreadMarker;
use nsh_platform_macos::objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

fn main() {
    env_logger_init();

    let mtm = MainThreadMarker::new().expect("必须在主线程启动");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // 关闭策略：M0 阶段先拦截并打印，验证「拦得住」。
    // 真正的策略选择（停止应用 / 隐藏 / 保留实例）在 M1c 接入。
    let window = Window::new(mtm, "Windroid", (900.0, 640.0), Box::new(move || {
        eprintln!("[close] 收到关闭请求");
        CloseIntent::Close
    }));

    // 视频表面：帧由后台流线程写入槽位，界面驱动上传。
    let slot = host::video::frame_slot();
    window.register_video(&slot, mtm);

    // 设备域。用真实模拟器还是假后端由环境决定——见 choose_backend。
    let serial = runtime::serial_for(
        std::env::var("WINDROID_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5554),
    );
    let (handle, events) = spawn(
        InstanceId::new("dev-1"),
        serial.clone(),
        choose_backend(),
        device::Timeouts::default(),
    );

    // 通道必须在载入界面之前接好：界面首帧就会取快照，
    // handler 晚一步注册会让那次调用落到「没有 handler」。
    let channel = window.channel();
    let bridge = host::Bridge::attach(handle.clone(), channel.clone(), events);

    match load_ui(&window) {
        Ok(()) => eprintln!("[ui] 界面已载入"),
        Err(e) => {
            eprintln!("[ui] 界面载入失败: {e}");
            std::process::exit(1);
        }
    }

    // 开了 `WINDROID_REAL` 就直接把流接上：M1a 要验证的是「真实画面进我们的
    // 窗口」，不是「点击流程」。UI 驱动流启动属于 M1b。
    let _stream = if std::env::var_os("WINDROID_REAL").is_some() {
        // 必须复用注册给表面的那个槽位。新建一个等于把帧写进没人读的地方，
        // 表现为「流已连接、解码在跑、窗口始终黑屏」。
        match streaming::Stream::start(serial.clone(), std::sync::Arc::clone(&slot), 27186) {
            Ok(s) => {
                eprintln!("[stream] 已连接 {}", s.url);
                Some(s)
            }
            Err(e) => {
                eprintln!("[stream] 连接失败: {e}");
                None
            }
        }
    } else {
        None
    };

    window.show(mtm);

    // 帧上传由主线程定时器驱动，而不是由界面驱动。
    //
    // 界面驱动存在的问题是：界面不知道帧的节奏，只能猜一个间隔，猜大了掉帧、
    // 猜小了空转。用 16ms（约 60Hz）轮询槽位：没有新帧时它只做一次 Option
    // 检查，代价可以忽略；有新帧就上传。
    host::video::start_pump();

    // 事件转发线程与 actor 都在后台跑；主线程交给 AppKit 事件循环。
    app.run();

    let snap = bridge.snapshot();
    eprintln!(
        "[exit] 最终状态 {} (seq {}, 服务 {}/3)",
        snap.state,
        snap.seq,
        snap.facts.services.count()
    );
}

/// 载入界面。
///
/// 优先读 `nsh build` 的产物目录：它一并写出 capabilities.json，
/// 而 capabilities.host 不声明等于全禁。没有产物时退回源码，并显式给出
/// 能力声明——这样开发期与打包后走同一条路径。
fn load_ui(window: &Window) -> Result<(), String> {
    let bundle_js = bundle_dir().join("bundle.js");
    if bundle_js.is_file() {
        return window.load_bundle_dir(&bundle_js);
    }

    let source = ui_dir().join("App.tsx");
    let code = std::fs::read_to_string(&source)
        .map_err(|e| format!("{}: {e}", source.display()))?;
    window.load_bundle_source(code, host::CAPABILITIES_JSON)
}

/// 界面资源目录。
///
/// 开发期是仓库里的 app/；打包后是 .app 内的 Resources。
/// M0 只处理前者。
fn ui_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("WINDROID_UI_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// nsh build 的产物目录。
///
/// 与源码分开：`App.tsx` 是输入，`.nsh/out` 是输出。开发期与打包后都从这里载入。
fn bundle_dir() -> PathBuf {
    ui_dir().join(".nsh/out")
}

/// 选择后端。
///
/// `WINDROID_REAL=1` 时接真实模拟器，否则用假后端。默认假后端不是偷懒：
/// 开发界面时不该每次都等模拟器启动，而两种后端跑同一套 actor，
/// 说明状态机与真实世界的关系是被验证过的，不是两套代码各跑各的。
fn choose_backend() -> Box<dyn Backend> {
    if std::env::var_os("WINDROID_REAL").is_none() {
        return Box::new(device::FakeBackend::new().slowed(6));
    }

    let port = std::env::var("WINDROID_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(5554);
    Box::new(runtime::EmulatorBackend::new(
        InstanceId::new("dev-1"),
        runtime::Layout::default_root(),
        port,
    ))
}

fn env_logger_init() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();
}
