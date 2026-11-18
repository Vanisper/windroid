//! scrcpy 控制消息。窗口提供 Android 键码及已呈现帧的绝对像素。
//! 字节布局见 <https://github.com/Genymobile/scrcpy/blob/v5.0.1/app/src/control_msg.c>。

use std::io::{self, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key {
        down: bool,
        keycode: u32,
        repeat: u32,
        meta_state: u32,
    },
    Text(String),
    /// Android MotionEvent action、actionButton 与 buttonState，固定使用鼠标指针。
    Touch {
        action: u8,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
        action_button: u32,
        buttons: u32,
    },
    /// 滚动量以刻度计，上游每轴接受 [-16, 16]。
    Scroll {
        x: i32,
        y: i32,
        width: u16,
        height: u16,
        h: f32,
        v: f32,
        buttons: u32,
    },
}

fn geometry(width: u16, height: u16) -> u32 {
    u32::from(width) << 16 | u32::from(height)
}

fn position(x: i32, y: i32, width: u16, height: u16, bytes: &mut Vec<u8>) -> io::Result<()> {
    if !(0..i32::from(width)).contains(&x) || !(0..i32::from(height)).contains(&y) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "输入坐标超出帧边界",
        ));
    }
    bytes.extend(x.to_be_bytes());
    bytes.extend(y.to_be_bytes());
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    Ok(())
}

impl InputEvent {
    fn frame_size(&self) -> Option<u32> {
        match *self {
            Self::Touch { width, height, .. } | Self::Scroll { width, height, .. } => {
                Some(geometry(width, height))
            }
            _ => None,
        }
    }

    fn write(self, mut out: impl Write) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(32);
        match self {
            Self::Key {
                down,
                keycode,
                repeat,
                meta_state,
            } => {
                bytes.extend([0, u8::from(!down)]);
                bytes.extend(keycode.to_be_bytes());
                bytes.extend(repeat.to_be_bytes());
                bytes.extend(meta_state.to_be_bytes());
            }
            Self::Text(text) => {
                // 单条消息最多 300 个 UTF-8 字节。一次持有写锁，整段文本保持顺序。
                let mut rest = text.as_str();
                while !rest.is_empty() {
                    let n = rest.floor_char_boundary(300.min(rest.len()));
                    let (chunk, next) = rest.split_at(n);
                    bytes.clear();
                    bytes.push(1);
                    bytes.extend((n as u32).to_be_bytes());
                    bytes.extend(chunk.as_bytes());
                    out.write_all(&bytes)?;
                    rest = next;
                }
                return Ok(());
            }
            Self::Touch {
                action,
                x,
                y,
                width,
                height,
                action_button,
                buttons,
            } => {
                bytes.extend([2, action]);
                bytes.extend(u64::MAX.to_be_bytes());
                position(x, y, width, height, &mut bytes)?;
                let pressure: u16 = if buttons != 0 || action == 0 {
                    u16::MAX
                } else {
                    0
                };
                bytes.extend(pressure.to_be_bytes());
                bytes.extend(action_button.to_be_bytes());
                bytes.extend(buttons.to_be_bytes());
            }
            Self::Scroll {
                x,
                y,
                width,
                height,
                h,
                v,
                buttons,
            } => {
                bytes.push(3);
                position(x, y, width, height, &mut bytes)?;
                for steps in [h, v] {
                    if !steps.is_finite() {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, "滚动量无效"));
                    }
                    let fixed = (steps.clamp(-16., 16.) * 2048.) as i16;
                    bytes.extend(fixed.to_be_bytes());
                }
                bytes.extend(buttons.to_be_bytes());
            }
        }
        out.write_all(&bytes)
    }
}

struct Sockets {
    video: TcpStream,
    control: TcpStream,
    writer: Mutex<()>,
    intentional: AtomicBool,
    closed: AtomicBool,
    size: AtomicU32,
    virtual_display: bool,
}

/// 可跨线程持有。所有输入和关闭操作只针对建立它的两条 socket。
#[derive(Clone)]
pub struct SessionControl(Arc<Sockets>);

impl SessionControl {
    pub(crate) fn new(
        video: &TcpStream,
        control: TcpStream,
        size: (u32, u32),
        virtual_display: bool,
    ) -> io::Result<Self> {
        control.set_nodelay(true)?;
        control.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut inbound = control.try_clone()?;
        let me = Self(Arc::new(Sockets {
            video: video.try_clone()?,
            control,
            writer: Mutex::new(()),
            intentional: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            virtual_display,
            size: AtomicU32::new(geometry(size.0 as u16, size.1 as u16)),
        }));
        // 当前不订阅剪贴板。独立检测控制通道 EOF，静止的视频读取也能及时返回。
        let sockets = Arc::downgrade(&me.0);
        std::thread::Builder::new()
            .name("stream-control".into())
            .spawn(move || {
                let _ = io::copy(&mut inbound, &mut io::sink());
                if let Some(inner) = sockets.upgrade() {
                    Self(inner).disconnect();
                }
            })?;
        Ok(me)
    }

    fn write(&self, write: impl FnOnce(&TcpStream) -> io::Result<()>) -> io::Result<()> {
        let _lock = self
            .0
            .writer
            .lock()
            .map_err(|_| io::Error::other("控制通道写锁中断"))?;
        if self.0.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "投屏控制通道已关闭",
            ));
        }
        let result = write(&self.0.control);
        if let Err(e) = &result
            && e.kind() != io::ErrorKind::InvalidInput
        {
            // write_all 可能只送出半条消息。该字节流已无法继续复用。
            self.disconnect();
        }
        result
    }

    pub fn send(&self, event: InputEvent) -> io::Result<()> {
        self.write(|out| {
            // 旋转时窗口可能还在画上一帧，直接丢弃旧坐标。
            if event
                .frame_size()
                .is_some_and(|s| s != self.0.size.load(Ordering::Acquire))
            {
                return Ok(());
            }
            event.write(out)
        })
    }

    /// 请求 Android 调整本会话的虚拟显示。实际尺寸由后续会话包/帧给出。
    pub fn resize(&self, width: u16, height: u16) -> io::Result<()> {
        if !self.0.virtual_display || width == 0 || height == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "仅可调整虚拟显示到正尺寸",
            ));
        }
        let [w1, w2] = width.to_be_bytes();
        let [h1, h2] = height.to_be_bytes();
        self.write(|mut out| out.write_all(&[21, w1, w2, h1, h2]))
    }

    pub(crate) fn start_app(&self, package: &str) -> io::Result<()> {
        // START_APP 为单字节长度；调用方已验证包名与长度。服务器在本会话显示上启动。
        let mut msg = vec![16, package.len() as u8];
        msg.extend(package.as_bytes());
        self.write(|mut out| out.write_all(&msg))
    }

    pub(crate) fn set_size(&self, width: u32, height: u32) {
        self.0
            .size
            .store(geometry(width as u16, height as u16), Ordering::Release);
    }

    /// 唤醒读写线程。进程等待与 adb 回收由会话所有者执行。
    pub fn shutdown(&self) {
        self.0.intentional.store(true, Ordering::Release);
        self.disconnect();
    }

    pub(crate) fn intentional(&self) -> bool {
        self.0.intentional.load(Ordering::Acquire)
    }

    pub(crate) fn disconnect(&self) {
        if !self.0.closed.swap(true, Ordering::AcqRel) {
            let _ = self.0.video.shutdown(Shutdown::Both);
            let _ = self.0.control.shutdown(Shutdown::Both);
        }
    }
}
