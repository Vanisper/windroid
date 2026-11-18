//! 单窗视频表面。图层按原比例居中绘制，输入的参照系始终是最后一次成功显示的帧。

use std::cell::Cell;

use nsh_platform_macos::objc2::rc::Retained;
use nsh_platform_macos::objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
};
use nsh_platform_macos::objc2_app_kit::{
    NSEvent, NSEventModifierFlags as Mod, NSView, NSViewLayerContentsPlacement,
    NSViewLayerContentsRedrawPolicy,
};
use nsh_platform_macos::objc2_foundation::{
    NSData, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
};
use objc2_quartz_core::CATransaction;
use stream::{Frame, InputEvent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pixel {
    x: i32,
    y: i32,
    width: u16,
    height: u16,
}

/// AppKit 报告窗口点，scrcpy 要求左上角原点的编码像素。两侧都按比例换算，
/// Retina backing scale 因而相消。拖出画面后的 move/up 夹在边缘以结束原手势。
fn map_pixel(view: (f64, f64), point: (f64, f64), size: (u16, u16), clamp: bool) -> Option<Pixel> {
    let (vw, vh) = view;
    let (width, height) = size;
    let (x, y) = point;
    if width == 0
        || height == 0
        || vw <= 0.0
        || vh <= 0.0
        || ![vw, vh, x, y].iter().all(|v| v.is_finite())
    {
        return None;
    }
    let (w, h) = (f64::from(width), f64::from(height));
    let scale = (vw / w).min(vh / h);
    let px = (x - (vw - w * scale) * 0.5) / scale;
    let py = ((vh - y) - (vh - h * scale) * 0.5) / scale;
    if !clamp && (px < 0.0 || px >= w || py < 0.0 || py >= h) {
        return None;
    }
    Some(Pixel {
        x: px.clamp(0.0, w - 1.0) as i32,
        y: py.clamp(0.0, h - 1.0) as i32,
        width,
        height,
    })
}

pub struct CanvasState {
    input: Box<dyn Fn(InputEvent)>,
    resize: Option<Box<dyn Fn(u16, u16)>>,
    display_size: DisplayResize,
    size: Cell<(u16, u16)>,
    buttons: Cell<u32>,
    last_pointer: Cell<Option<Pixel>>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = CanvasState]
    #[name = "WindroidVideoCanvas"]
    pub struct Canvas;
    unsafe impl NSObjectProtocol for Canvas {}
    impl Canvas {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool { true }
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool { true }
        #[unsafe(method(mouseDown:))]
        fn left_down(&self, event: &NSEvent) { self.pointer(event, 0, 1); }
        #[unsafe(method(mouseUp:))]
        fn left_up(&self, event: &NSEvent) { self.pointer(event, 1, 1); }
        #[unsafe(method(mouseDragged:))]
        fn left_drag(&self, event: &NSEvent) { self.pointer(event, 2, 1); }
        #[unsafe(method(rightMouseDown:))]
        fn right_down(&self, event: &NSEvent) { self.pointer(event, 0, 2); }
        #[unsafe(method(rightMouseUp:))]
        fn right_up(&self, event: &NSEvent) { self.pointer(event, 1, 2); }
        #[unsafe(method(rightMouseDragged:))]
        fn right_drag(&self, event: &NSEvent) { self.pointer(event, 2, 2); }
        #[unsafe(method(otherMouseDown:))]
        fn other_down(&self, event: &NSEvent) { if event.buttonNumber() == 2 { self.pointer(event, 0, 4); } }
        #[unsafe(method(otherMouseUp:))]
        fn other_up(&self, event: &NSEvent) { if event.buttonNumber() == 2 { self.pointer(event, 1, 4); } }
        #[unsafe(method(otherMouseDragged:))]
        fn other_drag(&self, event: &NSEvent) { if event.buttonNumber() == 2 { self.pointer(event, 2, 4); } }
        #[unsafe(method(scrollWheel:))]
        fn scroll(&self, event: &NSEvent) {
            let Some(p) = self.pixel(event, false) else { return; };
            let unit = if event.hasPreciseScrollingDeltas() { 10.0 } else { 1.0 };
            (self.ivars().input)(InputEvent::Scroll { x:p.x, y:p.y, width:p.width, height:p.height,
                h:(-event.scrollingDeltaX()/unit) as f32, v:(event.scrollingDeltaY()/unit) as f32,
                buttons:self.ivars().buttons.get() });
        }
        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.sync_display_size(self.inLiveResize());
        }
        #[unsafe(method(viewDidChangeBackingProperties))]
        fn backing_changed(&self) {
            let _: () = unsafe { msg_send![super(self), viewDidChangeBackingProperties] };
            self.sync_display_size(self.inLiveResize());
        }
        #[unsafe(method(viewDidEndLiveResize))]
        fn end_live_resize(&self) {
            let _: () = unsafe { msg_send![super(self), viewDidEndLiveResize] };
            self.sync_display_size(false);
        }
        #[unsafe(method(performKeyEquivalent:))]
        fn key_equivalent(&self, event: &NSEvent) -> bool {
            if event.modifierFlags().contains(Mod::Command) && event.charactersIgnoringModifiers().is_some_and(|s|s.to_string().eq_ignore_ascii_case("w")) {
                if let Some(window) = self.window() { window.performClose(None); }
                true
            } else { unsafe { msg_send![super(self), performKeyEquivalent:event] } }
        }
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) { self.keyboard(event, true); }
        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) { self.keyboard(event, false); }
    }
);

impl Canvas {
    pub(crate) fn make(
        mtm: MainThreadMarker,
        input: Box<dyn Fn(InputEvent)>,
        resize: Option<Box<dyn Fn(u16, u16)>>,
    ) -> Retained<Self> {
        let allocated = mtm.alloc::<Self>().set_ivars(CanvasState {
            input,
            resize,
            display_size: DisplayResize::default(),
            size: Cell::new((0, 0)),
            buttons: Cell::new(0),
            last_pointer: Cell::new(None),
        });
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(640.0, 360.0));
        let view: Retained<Self> = unsafe { msg_send![super(allocated), initWithFrame: rect] };
        view.setWantsLayer(true);
        view.setToolTip(Some(&NSString::from_str("Esc 返回，F2 打开 Android 菜单")));
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);
        view.setLayerContentsPlacement(NSViewLayerContentsPlacement::ScaleProportionallyToFit);
        view
    }

    pub(crate) fn draw_frame(&self, frame: Frame) -> Result<(), String> {
        let width = u16::try_from(frame.width).map_err(|_| "视频宽度超出控制协议范围")?;
        let height = u16::try_from(frame.height).map_err(|_| "视频高度超出控制协议范围")?;
        let image = make_image(frame.bytes, width, height).ok_or("无法建立视频帧图像")?;
        let layer = self.layer().ok_or("视频图层未挂载")?;
        // 逐帧替换，不给 contents 附加隐式过渡动画。CALayer 持有该帧直到下一次提交。
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        unsafe {
            let ptr =
                CFRetained::as_ptr(&image).cast::<nsh_platform_macos::objc2::runtime::AnyObject>();
            layer.setContents(Some(ptr.as_ref()));
        }
        CATransaction::commit();
        if self.ivars().size.replace((width, height)) == (0, 0) {
            // 在首帧到达前，AppKit 可能已把画布从初始尺寸挂载、平铺或缩放到最终位置。
            self.sync_display_size(self.inLiveResize());
        }
        Ok(())
    }

    fn sync_display_size(&self, live: bool) {
        let state = self.ivars();
        let Some(resize) = &state.resize else { return };
        if self.window().is_none() {
            return;
        }
        let backing = self.convertSizeToBacking(self.bounds().size);
        if let Some(target) = video_size(backing.width, backing.height)
            && state
                .display_size
                .should_send(target, state.size.get(), live)
        {
            resize(target.0, target.1);
        }
    }

    fn pixel(&self, event: &NSEvent, clamp: bool) -> Option<Pixel> {
        let bounds = self.bounds();
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        map_pixel(
            (bounds.size.width, bounds.size.height),
            (p.x - bounds.origin.x, p.y - bounds.origin.y),
            self.ivars().size.get(),
            clamp,
        )
    }

    fn pointer(&self, event: &NSEvent, action: u8, button: u32) {
        let state = self.ivars();
        let pressed = state.buttons.get();
        if action != 0 && pressed & button == 0 {
            return;
        }
        let p = self.pixel(event, action != 0).or_else(|| {
            if action == 1 {
                state.last_pointer.get()
            } else {
                None
            }
        });
        let Some(p) = p else {
            return;
        };
        let buttons = match action {
            0 => pressed | button,
            1 => pressed & !button,
            _ => pressed,
        };
        state.buttons.set(buttons);
        state.last_pointer.set(Some(p));
        (state.input)(InputEvent::Touch {
            action,
            x: p.x,
            y: p.y,
            width: p.width,
            height: p.height,
            action_button: if action == 2 { 0 } else { button },
            buttons,
        });
    }

    fn keyboard(&self, event: &NSEvent, down: bool) {
        let flags = event.modifierFlags();
        let bare = event
            .charactersIgnoringModifiers()
            .map(|s| s.to_string())
            .unwrap_or_default();
        if flags.contains(Mod::Command) && bare.eq_ignore_ascii_case("w") {
            if down
                && !event.isARepeat()
                && let Some(window) = self.window()
            {
                window.performClose(None);
            }
            return;
        }
        if self.ivars().size.get() == (0, 0) {
            return;
        }
        let raw = event
            .characters()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let meta_state = if flags.contains(Mod::Shift) { 1 } else { 0 }
            | if flags.contains(Mod::Option) { 2 } else { 0 }
            | if flags.contains(Mod::Control) {
                0x1000
            } else {
                0
            }
            | if flags.contains(Mod::Command) {
                0x10000
            } else {
                0
            }
            | if flags.contains(Mod::CapsLock) {
                0x100000
            } else {
                0
            };
        let text = flags.contains(Mod::Option)
            && !flags.intersects(Mod::Command | Mod::Control)
            && !raw.is_ascii();
        let key = android_key(event.keyCode(), &bare).filter(|_| !text);
        if let Some(keycode) = key {
            (self.ivars().input)(InputEvent::Key {
                down,
                keycode,
                repeat: u32::from(down && event.isARepeat()),
                meta_state,
            });
        } else if down
            && !raw.is_empty()
            && raw
                .chars()
                .all(|c| !c.is_control() && !(''..='').contains(&c))
        {
            (self.ivars().input)(InputEvent::Text(raw));
        }
    }
}

/// 方向键等特殊键由硬件码识别，字母与数字按当前键盘布局的字符识别。
/// Android 键码见 https://developer.android.com/reference/android/view/KeyEvent 。
fn android_key(hardware: u16, characters: &str) -> Option<u32> {
    match hardware {
        36 | 76 => return Some(66),
        48 => return Some(61),
        51 => return Some(67),
        53 => return Some(4),   // Esc → Back
        120 => return Some(82), // F2 → Menu
        115 => return Some(122),
        116 => return Some(92),
        117 => return Some(112),
        119 => return Some(123),
        121 => return Some(93),
        123 => return Some(21),
        124 => return Some(22),
        125 => return Some(20),
        126 => return Some(19),
        _ => {}
    }
    let [b] = *characters.as_bytes() else {
        return None;
    };
    let b = b.to_ascii_lowercase();
    Some(match b {
        b'a'..=b'z' => u32::from(b - b'a') + 29,
        b'0'..=b'9' => u32::from(b - b'0') + 7,
        b' ' => 62,
        b',' => 55,
        b'.' => 56,
        b'/' => 76,
        b';' => 74,
        b'-' => 69,
        b'=' => 70,
        b'[' => 71,
        b']' => 72,
        b'\\' => 73,
        b'\'' => 75,
        _ => return None,
    })
}

#[derive(Default)]
struct DisplayResize {
    last_target: Cell<Option<(u16, u16)>>,
}

impl DisplayResize {
    fn should_send(&self, target: (u16, u16), wire: (u16, u16), live: bool) -> bool {
        if live || wire.0 == 0 || wire.1 == 0 {
            return false;
        }
        // 首次对照视频尺寸，以后对照最近的目标。新帧与编码对齐可能滞后于用户连续调窗。
        self.last_target.replace(Some(target)).unwrap_or(wire) != target
    }
}

/// 在 backing 像素空间保持内容比例，限制最长边以控制单会话显存与解码开销。
fn video_size(width: f64, height: f64) -> Option<(u16, u16)> {
    if !width.is_finite() || !height.is_finite() || width < 2.0 || height < 2.0 {
        return None;
    }
    let scale = (1920.0 / width.max(height)).min(1.0);
    let even = |length: f64| ((length * scale).round() as u16 & !1).max(2);
    Some((even(width), even(height)))
}

fn make_image(bytes: Vec<u8>, width: u16, height: u16) -> Option<CFRetained<CGImage>> {
    if width == 0
        || height == 0
        || bytes.len()
            != usize::from(width)
                .checked_mul(usize::from(height))?
                .checked_mul(4)?
    {
        return None;
    }
    // NSData 接管解码器产出的 Vec，由 provider / CGImage 持有到图层替换完成。
    let data = NSData::from_vec(bytes);
    let provider = CGDataProvider::with_cf_data(Some((*data).as_ref()))?;
    let bitmap_info = CGBitmapInfo(
        objc2_core_graphics::CGImageByteOrderInfo::Order32Little.0
            | CGImageAlphaInfo::NoneSkipFirst.0,
    );
    // sRGB 常量与 CoreGraphics 同寿命，provider 持有完整的一帧 BGRA 数据。
    unsafe {
        let space = CGColorSpace::with_name(Some(objc2_core_graphics::kCGColorSpaceSRGB))?;
        CGImage::new(
            width as usize,
            height as usize,
            8,
            32,
            usize::from(width) * 4,
            Some(&space),
            bitmap_info,
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_maps_displayed_pixels_and_ignores_letterbox() {
        let view = (400.0, 400.0);
        assert_eq!(
            map_pixel(view, (200.0, 200.0), (1920, 1080), false),
            Some(Pixel {
                x: 960,
                y: 540,
                width: 1920,
                height: 1080
            })
        );
        assert_eq!(map_pixel(view, (200.0, 390.0), (1920, 1080), false), None);
        assert_eq!(
            map_pixel(view, (-100.0, 0.0), (1920, 1080), true),
            Some(Pixel {
                x: 0,
                y: 1079,
                width: 1920,
                height: 1080
            })
        );
        assert_eq!(map_pixel(view, (10.0, 200.0), (1080, 1920), false), None);
        assert_eq!(
            map_pixel(view, (200.0, 300.0), (1080, 1920), false),
            Some(Pixel {
                x: 540,
                y: 480,
                width: 1080,
                height: 1920
            })
        );
        assert_eq!(map_pixel(view, (200.0, 100.0), (0, 0), false), None);
    }
    #[test]
    fn keyboard_preserves_layout_and_android_navigation() {
        assert_eq!(android_key(0, "Q"), Some(45));
        assert_eq!(android_key(53, ""), Some(4));
        assert_eq!(android_key(120, ""), Some(82));
        assert_eq!(android_key(123, ""), Some(21));
        assert_eq!(android_key(0, "é"), None);
    }
    #[test]
    fn virtual_display_resize_is_even_and_preserves_retina_aspect() {
        assert_eq!(video_size(3840.0, 2160.0), Some((1920, 1080)));
        assert_eq!(video_size(721.0, 1280.0), Some((720, 1280)));
        assert_eq!(video_size(0.0, 100.0), None);
    }

    #[test]
    fn virtual_display_follows_non_live_layout_and_coalesces_live_drag() {
        let display = DisplayResize::default();
        let original = (720, 1280);
        let tiled = (1090, 996);
        assert!(!display.should_send(tiled, (0, 0), false));
        // 首帧到达时补发程序调整的最终尺寸。重复布局与仍在途的帧都不产生额外请求。
        assert!(display.should_send(tiled, original, false));
        assert!(!display.should_send(tiled, original, false));
        assert!(!display.should_send((1000, 1100), tiled, true));
        assert!(!display.should_send(original, tiled, true));
        assert!(display.should_send(original, original, false));
        assert!(!display.should_send(original, tiled, false));

        let initial = DisplayResize::default();
        assert!(!initial.should_send(original, original, false));
        assert!(initial.should_send(tiled, original, false));
    }

    #[test]
    fn core_graphics_roundtrip_preserves_top_and_bottom_rgb_corners() {
        use objc2_core_foundation::{CGPoint, CGRect, CGSize};
        use objc2_core_graphics::{
            CGBitmapContextCreate, CGContext, CGImageByteOrderInfo, kCGColorSpaceSRGB,
        };
        // 解码器 BGRA，左上红、右上绿、左下蓝、右下白。读回用大端 RGBA。
        let image = make_image(
            vec![
                0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
            ],
            2,
            2,
        )
        .unwrap();
        let mut rgba = [0u8; 16];
        // context 仅借用本地数组，完成绘制后先释放 context 再核对像素。
        let space = unsafe { CGColorSpace::with_name(Some(kCGColorSpaceSRGB)) }.unwrap();
        let ctx = unsafe {
            CGBitmapContextCreate(
                rgba.as_mut_ptr().cast(),
                2,
                2,
                8,
                8,
                Some(&space),
                CGImageByteOrderInfo::Order32Big.0 | CGImageAlphaInfo::PremultipliedLast.0,
            )
        }
        .unwrap();
        CGContext::draw_image(
            Some(&ctx),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(2.0, 2.0)),
            Some(&image),
        );
        drop(ctx);
        assert_eq!(
            rgba,
            [
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255
            ]
        );
    }

    #[test]
    fn malformed_frame_is_never_installed() {
        assert!(make_image(Vec::new(), 0, 1).is_none());
        assert!(make_image(vec![0; 4], 1920, 1080).is_none());
    }
}
