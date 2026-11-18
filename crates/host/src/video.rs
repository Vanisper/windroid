//! 视频表面：把解码后的帧画进我们自己的窗口。
//!
//! 这是整个项目最关键的接缝：画面不是由第三方进程画在它自己的窗口里，
//! 而是我们接收像素、上传到自己的层、放进自己的视图树。正因如此，
//! 窗口关闭才能被拦截（见 [crate::surface]）。
//!
//! **光有像素不足以显示**：必须有一层把它画到屏幕上。缺了这一步的表现是
//! 「解码在跑、帧在上传、窗口全黑」——这个坑值得写在这里。
//!
//! 显示走 `CALayer.contents` + `CGImage`，不是 Metal 渲染管线。理由是 M1a
//! 要验证的是「帧能进我们的窗口」，而 Metal 显示需要编译着色器、建管线，
//! 那是另一件独立的、与窗口所有权无关的风险。先证明前者成立。
//! 代价是每帧一次 CPU 位图构造（1080p 约 1-2ms），够用但不理想；
//! 换成 Metal 是 M2 的性能工作。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use nsh_core::JsonValue;
use nsh_platform_macos::objc2::rc::Retained;
use nsh_platform_macos::objc2::{MainThreadMarker, MainThreadOnly};
use nsh_platform_macos::objc2_app_kit::{NSView, NSViewLayerContentsRedrawPolicy};
use nsh_platform_macos::objc2_foundation::{NSPoint, NSRect, NSSize};
use nsh_platform_macos::{ComponentContext, NativeComponent};
use objc2_core_foundation::CFData;
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
};

/// 帧的生产端与消费端共享的槽位。
///
/// 用 Mutex 而不是无锁结构：帧的写入频率是每秒几十次，锁竞争的代价远小于
/// 无锁实现带来的一整类正确性问题。
pub type FrameSlot = Arc<Mutex<Option<stream::Frame>>>;

/// 创建一个帧槽位。解码侧写入，表面侧读取。
pub fn frame_slot() -> FrameSlot {
    Arc::new(Mutex::new(None))
}

thread_local! {
    /// 当前活着的表面。帧的节奏只有这一个来源——多个定时器会互相错拍，
    /// 表现为画面抖动。
    static SURFACES: RefCell<std::collections::BTreeMap<u64, Rc<RefCell<SurfaceState>>>> =
        const { RefCell::new(std::collections::BTreeMap::new()) };
    static NEXT_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// 一个表面的可驱动部分。
struct SurfaceState {
    slot: FrameSlot,
    view: Retained<NSView>,
    uploaded: u64,
}

impl SurfaceState {
    /// 把最新一帧画到视图上。
    fn upload_latest(&mut self) {
        let Some(frame) = self.slot.lock().ok().and_then(|mut s| s.take()) else {
            return;
        };
        let (w, h) = (frame.width, frame.height);
        if w == 0 || h == 0 {
            return;
        }

        let Some(image) = make_image(&frame.bytes, w, h) else {
            return;
        };

        unsafe {
            if let Some(layer) = self.view.layer() {
                // CGImage 是 CoreFoundation 对象，转成 AnyObject 交给 CALayer。
                // SAFETY: CGImage 是 CoreFoundation 对象，CALayer.contents 接受它；
                // 引用在 setContents 返回前保持有效（image 活得比调用久）。
                let obj: &nsh_platform_macos::objc2::runtime::AnyObject =
                    &*objc2_core_foundation::CFRetained::as_ptr(&image)
                        .cast::<nsh_platform_macos::objc2::runtime::AnyObject>()
                        .as_ptr();
                layer.setContents(Some(obj));
            }
        }

        self.uploaded += 1;
        if self.uploaded == 1 || self.uploaded.is_multiple_of(60) {
            log::info!("视频表面已显示 {} 帧 ({}x{})", self.uploaded, w, h);
        }
    }
}

/// 把 BGRA 字节构造成 CGImage。
///
/// 每次分配新图而不是复用缓冲：CALayer 会持有旧图直到下次替换，
/// 复用同一块内存会让正在显示的帧被改写，表现为撕裂或花屏。
fn make_image(bytes: &[u8], width: u32, height: u32) -> Option<objc2_core_foundation::CFRetained<CGImage>> {
    let data = CFData::from_bytes(bytes);
    let provider = CGDataProvider::with_cf_data(Some(&data))?;

    // BGRA 在内存里是 B,G,R,A。小端机上按 u32 读就是 A,R,G,B，
    // 因此用 32Little + NoneSkipFirst：跳过最高字节，剩下 R,G,B。
    // 写错的表现是红蓝互换——画面看起来"能显示但有病"。
    let bitmap_info = CGBitmapInfo(
        objc2_core_graphics::CGImageByteOrderInfo::Order32Little.0
            | CGImageAlphaInfo::NoneSkipFirst.0,
    );
    // SAFETY: kCGColorSpaceSRGB 是 CoreGraphics 提供的常量字符串，进程期恒有效。
    let space = unsafe { CGColorSpace::with_name(Some(objc2_core_graphics::kCGColorSpaceSRGB)) }?;

    // SAFETY: decode 传空指针表示不裁剪；bytes_per_row 与 provider 的数据
    // 长度一致（width*4 <= bytes.len()），provider 持有数据的强引用。
    unsafe {
        CGImage::new(
            width as usize,
            height as usize,
            8,
            32,
            (width as usize) * 4,
            Some(&space),
            bitmap_info,
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent(0),
        )
    }
}

/// 承载视频的 NSView。
struct VideoSurface {
    view: Retained<NSView>,
    _context: ComponentContext,
    id: u64,
}

impl NativeComponent for VideoSurface {
    fn view(&self) -> Retained<NSView> {
        self.view.clone()
    }

    fn set_prop(&mut self, _name: &str, _value: Option<&JsonValue>) {}

    fn command(&mut self, name: &str, _args: &JsonValue) {
        if name == "frame" {
            let state = SURFACES.with(|m| m.borrow().get(&self.id).cloned());
            if let Some(state) = state
                && let Ok(mut s) = state.try_borrow_mut() {
                    s.upload_latest();
                }
        }
    }
}

/// 定时器是否在推帧。
///
/// 必须是进程级的：写入方是流线程（断开时），读取方是主线程（定时器回调）
/// 与引擎线程（组装快照）。用 thread_local 会让三方各持一份副本，
/// 表现为「流断了但定时器还在跑」「状态设了但界面看不到」。
static PUMPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 界面上显示的流状态。空串表示正常。
static STREAM_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// 启动主线程定时器。可重复调用，只创建一次。
///
/// 必须在主线程调用：AppKit 的视图与 layer 操作都有主线程约束。
pub fn start_pump() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        // 16ms ≈ 60Hz。比帧率略快，保证不因定时器粒度丢帧。
        pump_with_interval(0.016);
        log::info!("视频帧定时器已启动");
    });
    set_pumping(true);
}

/// 开关推帧。流断开时关，重连时开。
pub fn set_pumping(on: bool) {
    PUMPING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// 当前是否在推帧。
pub fn is_pumping() -> bool {
    PUMPING.load(std::sync::atomic::Ordering::Relaxed)
}

/// 设置界面上显示的流状态。None 表示正常。
///
/// 流断开必须让用户看见：画面停住而窗口照开，是最容易被误认为「软件卡了」
/// 的状态。
pub fn set_stream_note(note: Option<String>) {
    if let Ok(mut n) = STREAM_NOTE.lock() {
        *n = note;
    }
}

/// 读取当前的流状态。快照组装时调用。
pub fn stream_note() -> Option<String> {
    STREAM_NOTE.lock().ok().and_then(|n| n.clone())
}

fn pump_with_interval(interval: f64) {
    use std::os::raw::c_void;

    // 回调签名是 (timer, info) 两参。写成单参在多数 ABI 上"能跑"，
    // 但那是未定义行为——多传的参数被静默丢弃，换个平台就崩。
    extern "C" fn tick(_timer: *mut c_void, _info: *mut c_void) {
        pump_frame();
    }

    #[repr(C)]
    struct CFRunLoopTimerContext {
        version: isize,
        info: *mut c_void,
        retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
        release: Option<extern "C" fn(*const c_void)>,
        copy_description: Option<extern "C" fn(*const c_void) -> *const c_void>,
    }

    unsafe extern "C" {
        fn CFRunLoopGetMain() -> *mut c_void;
        fn CFRunLoopTimerCreate(
            allocator: *const c_void,
            fire_date: f64,
            interval: f64,
            flags: usize,
            order: isize,
            callout: extern "C" fn(*mut c_void, *mut c_void),
            context: *mut CFRunLoopTimerContext,
        ) -> *mut c_void;
        fn CFAbsoluteTimeGetCurrent() -> f64;
        fn CFRunLoopAddTimer(rl: *mut c_void, timer: *mut c_void, mode: *const c_void);
        static kCFRunLoopCommonModes: *const c_void;
    }

    let mut ctx = CFRunLoopTimerContext {
        version: 0,
        info: std::ptr::null_mut(),
        retain: None,
        release: None,
        copy_description: None,
    };
    unsafe {
        let timer = CFRunLoopTimerCreate(
            std::ptr::null(),
            CFAbsoluteTimeGetCurrent(),
            interval,
            0,
            0,
            tick,
            &mut ctx,
        );
        CFRunLoopAddTimer(CFRunLoopGetMain(), timer, kCFRunLoopCommonModes);
    }
}

/// 驱动所有表面各显示一次最新帧。
pub fn pump_frame() {
    // 没有流在跑时不做事。检查一个 bool 比遍历表面便宜得多。
    if !is_pumping() {
        return;
    }
    let ids: Vec<u64> = SURFACES.with(|m| m.borrow().keys().copied().collect());
    for id in ids {
        let state = SURFACES.with(|m| m.borrow().get(&id).cloned());
        if let Some(state) = state
            && let Ok(mut s) = state.try_borrow_mut() {
                s.upload_latest();
            }
    }
}

/// 注册名为 `windroid.video` 的原生组件。
pub fn register(app_view: &nsh_platform_macos::AppView, mtm: MainThreadMarker, slot: FrameSlot) {
    app_view.register_component(
        "windroid.video",
        Rc::new(move |ctx: ComponentContext| create(mtm, &slot, ctx)),
    );
}

fn create(mtm: MainThreadMarker, slot: &FrameSlot, ctx: ComponentContext) -> Box<dyn NativeComponent> {
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(640.0, 360.0)),
    );
    // wantsLayer 让 AppKit 创建 backing layer；不做这一步，setContents
    // 无处可放，画面不会出现。
    view.setWantsLayer(true);
    // 不去重绘：内容由我们主动推送，让 AppKit 自己再画一遍是浪费。
    view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);

    let id = NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    });
    let state = Rc::new(RefCell::new(SurfaceState {
        slot: Arc::clone(slot),
        view: view.clone(),
        uploaded: 0,
    }));
    SURFACES.with(|m| m.borrow_mut().insert(id, state));
    log::info!("视频表面已注册 id={id}");

    Box::new(VideoSurface { view, _context: ctx, id })
}
