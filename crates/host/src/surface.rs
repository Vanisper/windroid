//! 应用库与视频窗口都由 AppKit 主线程持有。宿主通过可跨线程的命令和事件接线。

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use nsh_platform_macos::objc2::rc::Retained;
use nsh_platform_macos::objc2::runtime::ProtocolObject;
use nsh_platform_macos::objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send,
};
use nsh_platform_macos::objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication,
    NSApplicationDelegate, NSApplicationTerminateReply, NSBackingStoreType, NSControlStateValueOn,
    NSEvent, NSEventModifierFlags, NSEventType, NSModalResponseOK, NSOpenPanel, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use nsh_platform_macos::objc2_foundation::{
    NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use nsh_platform_macos::{AppOptions, AppView};

/// package 为 None 时是该实例的设备视图。窗口从创建到回收始终携带同一把键。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct WindowKey {
    pub instance_id: String,
    pub package: Option<String>,
}

#[derive(Debug)]
pub enum WindowCommand {
    /// 宿主已核对实例归属与状态。重复打开聚焦已有窗口。
    Open {
        key: WindowKey,
        title: String,
        serial: String,
        adb: PathBuf,
        jar: PathBuf,
    },
    Background {
        key: WindowKey,
    },
    /// 宿主已确认 Android 退出，直接回收其窗口。
    Dismiss {
        key: WindowKey,
    },
    /// 完全退出执行失败，窗口保留，可再次操作。
    QuitFailed {
        key: WindowKey,
    },
    CloseInstance {
        instance_id: String,
    },
    PickApk {
        instance_id: String,
        instance_name: String,
    },
    /// 最近一次成功读取的完整活动任务集合。查询失败时应保留上次事实。
    Reconcile {
        instance_id: String,
        tasks: Vec<(String, u32)>,
    },
}

#[derive(Debug)]
pub enum WindowEvent {
    Opened {
        key: WindowKey,
    },
    FirstFrame {
        key: WindowKey,
    },
    Closed {
        key: WindowKey,
    },
    InstanceClosed {
        instance_id: String,
    },
    QuitRequested {
        key: WindowKey,
    },
    ApkPicked {
        instance_id: String,
        path: PathBuf,
    },
    Error {
        instance_id: String,
        message: String,
    },
}

/// 无轮询定时器。命令、首帧与流结束均可从工作线程投递到 AppKit。
pub fn on_main(action: impl FnOnce(MainThreadMarker) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        action(MainThreadMarker::new().expect("AppKit 主队列"));
    });
}

/// 安排一次性的启动超时检查，无需常驻定时器。
pub fn on_main_after(
    delay: std::time::Duration,
    action: impl FnOnce(MainThreadMarker) + Send + 'static,
) {
    let when = dispatch2::DispatchTime::try_from(delay).expect("窗口超时在有效范围内");
    DispatchQueue::main()
        .after(when, move || {
            action(MainThreadMarker::new().expect("AppKit 主队列"))
        })
        .expect("主队列允许延时回调");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseIntent {
    Close,
    Keep,
}
pub type CloseFn = Box<dyn FnMut() -> CloseIntent + 'static>;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WindroidAppTermination"]
    pub struct TerminationDelegate;
    unsafe impl NSObjectProtocol for TerminationDelegate {}
    unsafe impl NSApplicationDelegate for TerminationDelegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn terminate(&self, app: &NSApplication) -> NSApplicationTerminateReply {
            // 让 main 的 run 返回后统一清理窗口、连接和实例进程。唤醒等待事件的 run loop。
            app.stop(None);
            if let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                NSEventType::ApplicationDefined, NSPoint::new(0.0,0.0), NSEventModifierFlags::empty(), 0.0, 0, None, 0, 0, 0,
            ) { app.postEvent_atStart(&event, true); }
            NSApplicationTerminateReply::TerminateCancel
        }
    }
);

impl TerminationDelegate {
    pub fn install(mtm: MainThreadMarker) -> Option<Retained<Self>> {
        let app = NSApplication::sharedApplication(mtm);
        if app.delegate().is_some() {
            return None;
        }
        let delegate: Retained<Self> = unsafe { msg_send![mtm.alloc::<Self>(), init] };
        app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        Some(delegate)
    }
}

pub struct CloseIvars {
    on_close: RefCell<CloseFn>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = CloseIvars]
    #[name = "WindroidCloseDelegate"]
    pub struct CloseDelegate;
    unsafe impl NSObjectProtocol for CloseDelegate {}
    unsafe impl NSWindowDelegate for CloseDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _window: &NSWindow) -> bool {
            self.ivars()
                .on_close
                .try_borrow_mut()
                .is_ok_and(|mut f| f() == CloseIntent::Close)
        }
    }
);

struct OwnedWindow {
    window: Retained<NSWindow>,
    // NSWindow 只弱持有 delegate。
    _delegate: Retained<CloseDelegate>,
}

impl OwnedWindow {
    fn new(mtm: MainThreadMarker, title: &str, size: (f64, f64), on_close: CloseFn) -> Self {
        let rect = NSRect::new(NSPoint::new(140.0, 140.0), NSSize::new(size.0, size.1));
        let window = unsafe {
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
        // Rust 持有分配时的 +1。默认的 releasedWhenClosed 会在 close 时另作一次 release。
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(title));
        let this = mtm.alloc::<CloseDelegate>().set_ivars(CloseIvars {
            on_close: RefCell::new(on_close),
        });
        let delegate: Retained<CloseDelegate> = unsafe { msg_send![super(this), init] };
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        Self {
            window,
            _delegate: delegate,
        }
    }

    fn show(&self, mtm: MainThreadMarker) {
        if self.window.isMiniaturized() {
            self.window.deminiaturize(None);
        }
        self.window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activate();
    }
}

/// 应用库的 nsh 宿主窗口。
pub struct Window {
    owned: OwnedWindow,
    view: Rc<AppView>,
}

impl Window {
    pub fn new(mtm: MainThreadMarker, title: &str, size: (f64, f64), on_close: CloseFn) -> Self {
        let owned = OwnedWindow::new(mtm, title, size, on_close);
        let view = AppView::new(mtm, AppOptions::default());
        owned.window.setContentView(Some(&view.view()));
        Self { owned, view }
    }

    pub fn channel(&self) -> nsh_engine::HostChannel {
        self.view.host()
    }

    pub fn load_rust(&self, root: nsh_platform_macos::Root) {
        self.view.load(nsh_engine::App::Rust(root));
    }

    /// 同时载入构建产物中的 source map、能力声明与 app.json。
    pub fn load_bundle_dir(&self, bundle_js: &std::path::Path) -> Result<(), String> {
        let bundle = nsh_engine::Bundle::read(bundle_js).map_err(|e| e.to_string())?;
        self.view.load(nsh_engine::App::Js(bundle));
        Ok(())
    }

    pub fn load_bundle_source(&self, code: String, capabilities_json: &str) -> Result<(), String> {
        let bundle =
            nsh_engine::Bundle::source(code).with_capabilities_json(Some(capabilities_json))?;
        self.view.load(nsh_engine::App::Js(bundle));
        Ok(())
    }

    pub fn show(&self, mtm: MainThreadMarker) {
        self.owned.show(mtm);
    }
    pub fn request_close(&self) {
        self.owned.window.performClose(None);
    }
    pub fn is_visible(&self) -> bool {
        self.owned.window.isVisible()
    }
    pub fn title(&self) -> String {
        self.owned.window.title().to_string()
    }
}

/// 视频窗口直接承载响应鼠键的 NSView。每个实例独占一层像素和一条输入通道。
pub struct VideoWindow {
    owned: OwnedWindow,
    canvas: Retained<crate::video::Canvas>,
}

impl VideoWindow {
    pub fn new(
        mtm: MainThreadMarker,
        title: &str,
        size: (f64, f64),
        on_close: CloseFn,
        input: Box<dyn Fn(stream::InputEvent)>,
        resize: Option<Box<dyn Fn(u16, u16)>>,
    ) -> Self {
        let owned = OwnedWindow::new(mtm, title, size, on_close);
        let canvas = crate::video::Canvas::make(mtm, input, resize);
        owned.window.setBackgroundColor(Some(
            &nsh_platform_macos::objc2_app_kit::NSColor::blackColor(),
        ));
        owned.window.setContentMinSize(NSSize::new(240.0, 180.0));
        owned.window.setContentView(Some(&canvas));
        owned.window.setInitialFirstResponder(Some(&canvas));
        owned.window.makeFirstResponder(Some(&canvas));
        // AppKit 逐窗错开顶部坐标，防止新显示恰好盖住旧窗口。
        thread_local! { static NEXT: std::cell::Cell<NSPoint> = const { std::cell::Cell::new(NSPoint::new(0.0, 0.0)) }; }
        NEXT.with(|p| p.set(owned.window.cascadeTopLeftFromPoint(p.get())));
        Self { owned, canvas }
    }

    pub fn show(&self, mtm: MainThreadMarker) {
        self.owned.show(mtm);
    }
    pub fn set_title(&self, title: &str) {
        self.owned.window.setTitle(&NSString::from_str(title));
    }
    pub fn draw(&self, frame: stream::Frame) -> Result<(), String> {
        self.canvas.draw_frame(frame)
    }

    /// 异步 sheet 只锁定所属窗口。关闭窗口时取消尚未答复的 sheet。
    pub fn ask_close(
        &self,
        mtm: MainThreadMarker,
        label: &str,
        reply: impl Fn(Option<CloseChoice>, bool) + 'static,
    ) {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&format!("关闭「{label}」？")));
        alert.setInformativeText(&NSString::from_str(
            "保留后台后，可从应用库的“运行中”重新打开。完全退出将结束应用并移除其任务。",
        ));
        alert.addButtonWithTitle(&NSString::from_str("保留后台"));
        alert.addButtonWithTitle(&NSString::from_str("完全退出"));
        alert
            .addButtonWithTitle(&NSString::from_str("取消"))
            .setKeyEquivalent(&NSString::from_str("\x1b"));
        alert.setShowsSuppressionButton(true);
        let checkbox = alert.suppressionButton();
        if let Some(check) = &checkbox {
            check.setTitle(&NSString::from_str("对所有应用窗口记住我的选择"));
        }
        let done = RcBlock::new(move |code| {
            let choice = if code == NSAlertFirstButtonReturn {
                Some(CloseChoice::Background)
            } else if code == NSAlertSecondButtonReturn {
                Some(CloseChoice::Quit)
            } else {
                None
            };
            reply(
                choice,
                choice.is_some()
                    && checkbox
                        .as_ref()
                        .is_some_and(|c| c.state() == NSControlStateValueOn),
            );
        });
        alert.beginSheetModalForWindow_completionHandler(&self.owned.window, Some(&done));
    }
}

impl Drop for VideoWindow {
    fn drop(&mut self) {
        let window = &self.owned.window;
        if let Some(sheet) = window.attachedSheet() {
            window.endSheet(&sheet);
        }
        window.setDelegate(None);
        // 管理器已完成策略判断，close 执行回收；performClose 则会重入询问。
        window.close();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseChoice {
    Background,
    Quit,
}

/// 非模态选择器允许操作主窗口；目标实例在显示面板前确定。取消只产生 None。
pub fn pick_apk(
    mtm: MainThreadMarker,
    instance_name: &str,
    chosen: impl Fn(Option<PathBuf>) + 'static,
) -> Retained<NSOpenPanel> {
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    panel.setTitle(Some(&NSString::from_str(&format!(
        "安装 APK · {instance_name}"
    ))));
    panel.setMessage(Some(&NSString::from_str(&format!(
        "安装至「{instance_name}」"
    ))));
    panel.setPrompt(Some(&NSString::from_str("安装 APK")));
    #[allow(deprecated)]
    panel.setAllowedFileTypes(Some(&NSArray::from_retained_slice(&[NSString::from_str(
        "apk",
    )])));
    begin_picker(&panel, chosen);
    panel
}

/// 资源选择只返回路径。档案校验、预览和用户确认在独立步骤中进行。
pub enum ResourcePicker {
    ImageZip,
    RuntimeDirectory,
    ImageDirectory,
}

pub fn pick_resource(
    mtm: MainThreadMarker,
    kind: ResourcePicker,
    chosen: impl FnOnce(Option<PathBuf>) + 'static,
) {
    let panel = NSOpenPanel::openPanel(mtm);
    let directory = !matches!(kind, ResourcePicker::ImageZip);
    let title = match kind {
        ResourcePicker::ImageZip => "选择官方系统镜像 ZIP",
        ResourcePicker::RuntimeDirectory => "选择运行组件目录",
        ResourcePicker::ImageDirectory => "选择系统镜像目录",
    };
    panel.setTitle(Some(&NSString::from_str(title)));
    panel.setCanChooseDirectories(directory);
    panel.setCanCreateDirectories(directory);
    panel.setCanChooseFiles(!directory);
    panel.setAllowsMultipleSelection(false);
    if !directory {
        #[allow(deprecated)]
        panel.setAllowedFileTypes(Some(&NSArray::from_retained_slice(&[NSString::from_str(
            "zip",
        )])));
    }
    begin_picker(&panel, chosen);
}

fn begin_picker(panel: &NSOpenPanel, chosen: impl FnOnce(Option<PathBuf>) + 'static) {
    let selection = panel.retain();
    let chosen = RefCell::new(Some(chosen));
    let reply = RcBlock::new(move |code| {
        if let Some(chosen) = chosen.take() {
            chosen(
                (code == NSModalResponseOK)
                    .then(|| selection.URL()?.to_file_path())
                    .flatten(),
            );
        }
    });
    panel.beginWithCompletionHandler(&reply);

/// 工作区拒绝与预放资源确认先于应用库及文件日志。提示只使用 AppKit。
pub fn workspace_prompt(mtm: MainThreadMarker, title: &str, detail: &str, confirm: bool) -> bool {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.addButtonWithTitle(&NSString::from_str(if confirm {
        "在此新建工作区"
    } else {
        "退出"
    }));
    if confirm {
        alert.addButtonWithTitle(&NSString::from_str("取消"));
    }
    alert.runModal() == NSAlertFirstButtonReturn
}
}
