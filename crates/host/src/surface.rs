//! 窗口所有权与关闭拦截。
//!
//! 本模块是「窗口归我们」这条约束的落地处。只在 macOS 上有实现——平台顺序是
//! macOS → Linux → Windows，先跑通一个再谈其余。
//!
//! 关键点：窗口由标准 AppKit API 创建并持有，nsh 的 [AppView] 作为 content view
//! 嵌进来。因此关闭拦截用标准 NSWindowDelegate，不依赖框架提供 hook——
//! 这正是选 nsh 的原因：它不持有窗口，我们持有。

use std::cell::RefCell;
use std::rc::Rc;

use nsh_platform_macos::objc2::rc::Retained;
use nsh_platform_macos::objc2::runtime::ProtocolObject;
use nsh_platform_macos::objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use nsh_platform_macos::objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use nsh_platform_macos::objc2_foundation::{
    NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use nsh_platform_macos::{AppOptions, AppView};

/// 关闭决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseIntent {
    /// 允许关闭，进程可以退出。
    Close,
    /// 拒绝关闭，交给产品决定后续（询问用户、隐藏、保留实例）。
    Keep,
}

/// 关闭拦截的决策点。
///
/// 用回调而不是固定策略：第一版会弹选择，第二版可能记住用户偏好。
/// 本模块只负责「拦得住」，不替产品决定「拦下来做什么」。
pub type CloseFn = Box<dyn FnMut() -> CloseIntent + 'static>;

define_class!(
    /// 关闭拦截代理。
    ///
    /// 必须用 define_class! 而非手写 impl：手写会缺少宏生成的 ObjC 元数据，
    /// 编译期报 NSObjectProtocol not satisfied。
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = CloseIvars]
    #[name = "WindroidCloseDelegate"]
    pub struct CloseDelegate;

    unsafe impl NSObjectProtocol for CloseDelegate {}

    unsafe impl NSWindowDelegate for CloseDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _window: &NSWindow) -> bool {
            let mut guard = self.ivars().on_close.borrow_mut();
            match guard.as_mut().map(|f| f()) {
                Some(CloseIntent::Keep) => false,
                Some(CloseIntent::Close) => true,
                // 没装回调时按标准行为关闭：不制造「点了没反应」。
                None => true,
            }
        }
    }
);

/// 代理的附加数据。objc2 的宏要求它放在 ivars 里，不能作为结构体字段。
#[derive(Default)]
pub struct CloseIvars {
    on_close: RefCell<Option<CloseFn>>,
}

impl CloseDelegate {
    fn make(mtm: MainThreadMarker, on_close: CloseFn) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(CloseIvars {
            on_close: RefCell::new(Some(on_close)),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// 一个归我们所有的窗口，连同它的界面视图与宿主通道。
pub struct Window {
    window: Retained<NSWindow>,
    /// 必须持有：NSWindow.detach 后代理会被释放，delegate 变悬垂。
    _delegate: Retained<CloseDelegate>,
    view: Rc<AppView>,
}

impl Window {
    /// 创建窗口。返回时窗口尚未显示。
    ///
    /// 先建窗口、装好代理、再显示——避免出现「已显示但关闭拦不住」的时间窗。
    pub fn new(
        mtm: MainThreadMarker,
        title: &str,
        size: (f64, f64),
        on_close: CloseFn,
    ) -> Self {
        let rect = NSRect::new(NSPoint::new(140.0, 140.0), NSSize::new(size.0, size.1));

        let window: Retained<NSWindow> = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str(title));

        let delegate = CloseDelegate::make(mtm, on_close);
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

        // AppView 是普通 NSView，作为 content view 挂进来。
        // 界面只是窗口里的一个视图，窗口本身仍是我们的。
        let view = AppView::new(mtm, AppOptions::default());
        window.setContentView(Some(&view.view()));

        Self { window, _delegate: delegate, view }
    }

    /// 宿主通道：命令与事件的接入点。在 load 之前也可用——
    /// 通道由 AppView 持有，先注册 handler 再载入界面可以避免漏掉启动事件。
    pub fn channel(&self) -> nsh_engine::HostChannel {
        self.view.host()
    }

    /// 载入 Rust 根组件。
    pub fn load_rust(&self, root: nsh_platform_macos::Root) {
        self.view.load(nsh_engine::App::Rust(root));
    }

    /// 从 nsh build 的产物目录载入界面。
    ///
    /// 用 [nsh_engine::Bundle::read] 而非读字符串进 Bundle::source：它会一并
    /// 读取同目录的 source map、capabilities.json 与 app.json。手工拼 Bundle
    /// 会漏掉 capabilities，而 capabilities.host 默认什么都不授予——漏掉的
    /// 结果是界面上每个 host.call 都被拒，且错误出现在运行期而非构建期。
    pub fn load_bundle_dir(&self, bundle_js: &std::path::Path) -> Result<(), String> {
        let bundle = nsh_engine::Bundle::read(bundle_js).map_err(|e| e.to_string())?;
        self.view.load(nsh_engine::App::Js(bundle));
        Ok(())
    }

    /// 从内存中的 JS 源载入，并显式给出能力声明。
    ///
    /// 供内嵌与测试使用。capabilities_json 不可省略：不传等于「什么都不授予」。
    /// 格式与 nsh build 产出的 capabilities.json 一致。
    pub fn load_bundle_source(&self, code: String, capabilities_json: &str) -> Result<(), String> {
        let bundle = nsh_engine::Bundle::source(code)
            .with_capabilities_json(Some(capabilities_json))?;
        self.view.load(nsh_engine::App::Js(bundle));
        Ok(())
    }

    /// 显示并激活。
    pub fn show(&self, mtm: MainThreadMarker) {
        self.window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activate();
    }

    /// 请求关闭。走标准关闭流程，因此会经过代理——这正是可验证之处。
    pub fn request_close(&self) {
        self.window.performClose(None);
    }

    /// 窗口是否仍可见。用于验证拦截确实生效。
    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    /// 窗口标题，供断言。
    pub fn title(&self) -> String {
        self.window.title().to_string()
    }
}
