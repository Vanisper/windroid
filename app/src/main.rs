//! Windroid 应用入口。
//!
//! 这一层只做组装：建窗口、接设备域、载入界面。业务逻辑不在这里。
//!
//! 窗口在显示之前必须已经装好关闭拦截代理——否则存在「已显示但关闭拦不住」
//! 的时间窗，而那正是本产品的核心约束之一。

use std::cell::RefCell;
use std::rc::Rc;

use contract::{Command, Image, InstanceId, Spec};
use device::{spawn, FakeBackend, Timeouts};
use host::surface::{CloseIntent, Window};
use nsh_core::prelude::*;
use nsh_platform_macos::objc2::MainThreadMarker;
use nsh_platform_macos::objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

fn main() {
    env_logger_init();

    let mtm = MainThreadMarker::new().expect("必须在主线程启动");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // 关闭策略：M0 阶段先拦截并打印，验证「拦得住」。
    // 真正的策略选择（停止应用 / 隐藏 / 保留实例）在 M1c 接入。
    let close_count = Rc::new(RefCell::new(0u32));
    let cc = Rc::clone(&close_count);
    let window = Window::new(mtm, "Windroid", (1080.0, 720.0), Box::new(move || {
        *cc.borrow_mut() += 1;
        let n = *cc.borrow();
        eprintln!("[close] 拦截第 {n} 次关闭请求（M0 阶段直接放行）");
        CloseIntent::Close
    }));

    // 设备域：M0 用假后端。真后端接模拟器是 M1 的事。
    let (handle, events) = spawn(
        InstanceId::new("dev-1"),
        FakeBackend::new().slowed(6),
        Timeouts::default(),
    );

    // 通道在载入界面之前接好，避免漏掉启动事件。
    let channel = window.channel();
    let bridge = host::Bridge::attach(handle.clone(), channel.clone(), events);

    // M0 的最小流程：建实例、启动，然后把状态打到日志里。
    // 界面里能看到同样的事实——两者读的是同一份派生状态。
    let _ = handle.send(Command::Create {
        spec: Spec {
            template: "phone-1080".into(),
            image: Image { api: 34, tag: "google_apis_playstore".into(), abi: "arm64-v8a".into() },
            disk_bytes: 8 << 30,
        },
    });
    let ack = handle.send(Command::Start);
    eprintln!("[boot] Start 回执: {ack:?}");

    window.load_rust(nsh_platform_macos::Root::of::<Root>());
    window.show(mtm);

    // 事件转发线程与 actor 都在后台跑；主线程交给 AppKit 事件循环。
    app.run();

    // 退出后打一份最终快照，便于从终端确认状态确实推进过。
    let snap = bridge.snapshot();
    eprintln!(
        "[exit] 最终状态 {} (seq {}, 服务 {}/3)",
        snap.state,
        snap.seq,
        snap.facts.services.count()
    );
}

/// M0 的占位界面。
///
/// 下一步会替换成 TSX。现在先用 Rust 根组件，是为了把「窗口 + 通道 + 设备域」
/// 这条链路先跑通——框架层的问题不该和界面语言的编译问题混在一起排查。
#[component]
fn Root() -> impl IntoElement {
    let facts = Rc::new(RefCell::new(contract::Facts::default()));

    view()
        .flex_col()
        .gap(px(12.0))
        .p(px(24.0))
        .bg(rgb(0x0e0e11))
        .child(
            text("Windroid")
                .text_size(px(20.0))
                .text_color(rgb(0xf0f0f4)),
        )
        .child(
            text("M0 骨架：窗口归我们所有，命令/事件通道已接通")
                .text_size(px(13.0))
                .text_color(rgb(0x9a9aa8)),
        )
        .child(
            text(format!("服务状态: {}/3", facts.borrow().services.count()))
                .text_size(px(13.0))
                .text_color(rgb(0x6a6a78)),
        )
}

fn env_logger_init() {
    // 无 logger 时 log 宏是空操作，不影响功能；这里只是便于排查。
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();
}
