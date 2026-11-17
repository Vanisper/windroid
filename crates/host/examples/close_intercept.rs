//! 验证窗口所有权与关闭拦截。
//!
//! 这是本产品的核心约束：窗口必须归我们所有，否则无法拦截关闭。
//!
//! 判据三条同时成立：
//!   a) 代理被调用（拦得住）
//!   b) 返回 Keep 时窗口仍可见（真的没关）
//!   c) 返回 Close 时窗口关闭（也放得掉——只会拒绝的拦截器是坏的）
//!
//! 用 example 而非 test：AppKit 要求窗口操作在主线程，而 cargo test 的
//! 测试函数跑在工作线程上，拿不到 MainThreadMarker。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use host::surface::{CloseIntent, Window};
use nsh_core::prelude::*;
use nsh_platform_macos::objc2::MainThreadMarker;
use nsh_platform_macos::objc2::rc::Retained;
use nsh_platform_macos::objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask,
};
use nsh_platform_macos::objc2_foundation::{NSDate, NSDefaultRunLoopMode};

#[component]
fn Root() -> impl IntoElement {
    view().size_full().bg(rgb(0x0e0e11)).child(
        text("关闭拦截验证")
            .text_size(px(16.0))
            .text_color(rgb(0xf0f0f4)),
    )
}

/// 手动泵事件循环若干毫秒。
///
/// 比 DispatchQueue 简单：整个验证都在主线程上跑，不存在跨线程传递
/// AppKit 对象的问题。AppKit 的输出事件需要事件循环转起来才会真正绘制，
/// 所以这里不能只是 sleep。
fn pump(app: &NSApplication, ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        let until = NSDate::dateWithTimeIntervalSinceNow(0.01);
        let ev: Option<Retained<NSEvent>> = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&until),
                NSDefaultRunLoopMode,
                true,
            )
        };
        if let Some(ev) = ev {
            app.sendEvent(&ev);
        }
    }
}

fn main() {
    let mtm = MainThreadMarker::new().expect("main");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.finishLaunching();

    let hits: Rc<RefCell<Vec<u32>>> = Rc::new(RefCell::new(Vec::new()));
    let h = Rc::clone(&hits);

    // 前两次拒绝，第三次放行。同时验证「拦得住」与「放得掉」。
    let window = Window::new(
        mtm,
        "Windroid Spike",
        (520.0, 200.0),
        Box::new(move || {
            let mut v = h.borrow_mut();
            let n = v.len() + 1;
            v.push(n as u32);
            println!("[intercept] 第 {n} 次 windowShouldClose 被调用");
            if n < 3 { CloseIntent::Keep } else { CloseIntent::Close }
        }),
    );

    window.load_rust(nsh_platform_macos::Root::of::<Root>());
    window.show(mtm);
    pump(&app, 800);
    println!("[1] 窗口已显示 visible={} title={}", window.is_visible(), window.title());

    assert!(window.is_visible(), "窗口应该可见");

    window.request_close();
    pump(&app, 120);
    let after1 = window.is_visible();
    println!("[2] 第一次关闭后 visible={after1}  (期望 true)");

    window.request_close();
    pump(&app, 120);
    let after2 = window.is_visible();
    println!("[3] 第二次关闭后 visible={after2}  (期望 true)");

    window.request_close();
    pump(&app, 200);
    let after3 = window.is_visible();
    println!("[4] 第三次关闭后 visible={after3}  (期望 false)");

    let n = hits.borrow().len();
    println!("[5] 拦截次数={n} 记录={:?}", hits.borrow());

    let pass = n == 3 && after1 && after2 && !after3;
    println!("RESULT: {}", if pass { "PASS" } else { "FAIL" });
    std::process::exit(if pass { 0 } else { 1 });
}
