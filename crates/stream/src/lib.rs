//! 流域为每个设备窗或应用窗建立独立的 scrcpy 5.0.1 视频/控制通道。
//! 生命周期由 VideoSession 所有，窗口持有可克隆的 SessionControl。

use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

mod control;
pub mod decode;
pub mod packet;
mod server;

pub use control::{InputEvent, SessionControl};
pub use decode::Frame;
pub use packet::{Codec, Packet, SessionInfo};

use decode::Decoder;
use packet::invalid;
use server::Server;

/// server 首个参数与其固定发行版本严格匹配。
pub const SERVER_VERSION: &str = "5.0.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayTarget {
    Device,
    /// 创建私有显示，并通过该 server 的 START_APP 定向启动包。
    App {
        package: String,
    },
}

impl DisplayTarget {
    fn validate(&self) -> io::Result<()> {
        if let Self::App { package } = self {
            let valid = package.len() <= 255
                && package.split('.').all(|p| {
                    p.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                        && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                });
            if !valid {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Android 包名无效",
                ));
            }
        }
        Ok(())
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(io::Error::new(io::ErrorKind::TimedOut, "投屏握手超时"))
    } else {
        Ok(left)
    }
}

fn check_cancel(cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::new(io::ErrorKind::Interrupted, "窗口已取消投屏"))
    } else {
        Ok(())
    }
}

struct TimedReader<'a> {
    socket: &'a mut TcpStream,
    deadline: Instant,
    cancelled: &'a dyn Fn() -> bool,
}

impl Read for TimedReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            // read_exact 自动重试 Interrupted。用 ConnectionAborted 退出外层读包，
            // start_cancellable 对外统一返回 Interrupted。
            if (self.cancelled)() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            self.socket.set_read_timeout(Some(
                remaining(self.deadline)?.min(Duration::from_millis(150)),
            ))?;
            match self.socket.read(out) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
}

fn connect_video(
    addr: SocketAddr,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    mut check_server: impl FnMut() -> io::Result<()>,
) -> io::Result<TcpStream> {
    loop {
        check_cancel(cancelled)?;
        check_server()?;
        let wait = remaining(deadline)?;
        match TcpStream::connect_timeout(&addr, wait) {
            Ok(mut socket) => {
                let mut dummy = [0xff];
                let mut reader = TimedReader {
                    socket: &mut socket,
                    deadline,
                    cancelled,
                };
                match reader.read_exact(&mut dummy) {
                    Ok(()) if dummy[0] == 0 => return Ok(socket),
                    Ok(()) => return Err(invalid("scrcpy forward 探测字节无效")),
                    // adb forward 已就绪，但 server 尚未 listen 时，本地 TCP 也能建立。
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                        ) => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {}
            Err(e) => return Err(e),
        }
        thread::sleep(Duration::from_millis(35));
    }
}

/// 握手成功后，一并交付两个通道及其对应的设备、显示信息。
struct Handshake {
    video: TcpStream,
    control: SessionControl,
    size: (u32, u32),
    display_id: u32,
    device_name: String,
}

/// 阻塞的后台会话。Drop 释放本会话的设备目录、server 和端口。
pub struct VideoSession {
    server: Server,
    video: TcpStream,
    control: SessionControl,
    size: (u32, u32),
    display_id: u32,
    pub device_name: String,
}

impl VideoSession {
    pub fn start(
        adb: &Path,
        serial: &str,
        server_jar: &Path,
        target: DisplayTarget,
    ) -> io::Result<Self> {
        Self::start_cancellable(adb, serial, server_jar, target, || false)
    }

    /// 窗口可在连接过程中撤销请求。短命令各有截止时间，发送 START_APP 前再检查一次。
    pub fn start_cancellable(
        adb: &Path,
        serial: &str,
        server_jar: &Path,
        target: DisplayTarget,
        cancelled: impl Fn() -> bool,
    ) -> io::Result<Self> {
        Self::start_on(adb::Device::new(adb, serial), server_jar, target, cancelled)
    }

    pub fn start_on(
        device: adb::Device<'_>,
        server_jar: &Path,
        target: DisplayTarget,
        cancelled: impl Fn() -> bool,
    ) -> io::Result<Self> {
        let serial = device.serial;
        check_cancel(&cancelled)?;
        target.validate()?;
        if serial.is_empty()
            || serial
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "adb serial 无效",
            ));
        }
        let mut server = Server::start(device, server_jar, &target, &cancelled)?;
        let handshake = Self::connect(&mut server, &target, &cancelled).map_err(|e| {
            if cancelled() {
                io::Error::new(io::ErrorKind::Interrupted, "窗口已取消投屏")
            } else {
                server.context(e)
            }
        })?;
        let session = Self {
            server,
            video: handshake.video,
            control: handshake.control,
            size: handshake.size,
            display_id: handshake.display_id,
            device_name: handshake.device_name,
        };
        check_cancel(&cancelled)?;
        if let DisplayTarget::App { package } = target {
            session.control.start_app(&package)?;
        }
        Ok(session)
    }

    fn connect(
        server: &mut Server,
        target: &DisplayTarget,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Handshake> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, server.port()));
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut video = connect_video(addr, deadline, cancelled, || server.check_child())?;
        video.set_nodelay(true)?;
        // server 在 accept 全部启用的 socket 后才发送 device name；顺序见
        // https://github.com/Genymobile/scrcpy/blob/v5.0.1/server/src/main/java/com/genymobile/scrcpy/device/DesktopConnection.java
        let control_socket = TcpStream::connect_timeout(&addr, remaining(deadline)?)?;
        let mut reader = TimedReader {
            socket: &mut video,
            deadline,
            cancelled,
        };
        let mut device_name = [0; 64];
        let mut codec = [0; 4];
        reader.read_exact(&mut device_name)?;
        reader.read_exact(&mut codec)?;
        if !Codec::from_id(u32::from_be_bytes(codec)).is_some_and(Codec::supported) {
            return Err(invalid("设备未提供 H.264 视频"));
        }
        let Packet::Session(SessionInfo { width, height }) = Packet::read(&mut reader)? else {
            return Err(invalid("首包缺少视频会话参数"));
        };
        let virtual_display = matches!(target, DisplayTarget::App { .. });
        let display_id = if virtual_display {
            server.display_id(deadline, cancelled)?
        } else {
            0
        };
        video.set_read_timeout(None)?;
        let control =
            SessionControl::new(&video, control_socket, (width, height), virtual_display)?;
        let device_name = String::from_utf8_lossy(&device_name)
            .trim_end_matches('\0')
            .to_owned();
        Ok(Handshake {
            video,
            control,
            size: (width, height),
            display_id,
            device_name,
        })
    }

    pub fn display_id(&self) -> u32 {
        self.display_id
    }
    pub fn size(&self) -> (u32, u32) {
        self.size
    }
    pub fn control(&self) -> SessionControl {
        self.control.clone()
    }

    /// 视频不活动时阻塞至下一包或 shutdown。回调在调用 run 的后台线程同步执行。
    pub fn run(mut self, mut on_frame: impl FnMut(Frame) + Send + 'static) -> io::Result<()> {
        let result = self.receive(&mut on_frame);
        if self.control.intentional() {
            Ok(())
        } else {
            result.map_err(|e| self.server.context(e))
        }
    }

    fn receive(&mut self, on_frame: &mut impl FnMut(Frame)) -> io::Result<()> {
        let mut decoder = Decoder::new(self.size)?;
        let mut first_frame = Some(Instant::now() + Duration::from_secs(15));
        loop {
            // 新显示禁用了桌面装饰。包已卸载或启动失败，可能完全没有输出。
            let packet = if let Some(deadline) = first_frame {
                Packet::read(&mut TimedReader {
                    socket: &mut self.video,
                    deadline,
                    cancelled: &|| false,
                })
                .map_err(|e| {
                    if e.kind() == io::ErrorKind::TimedOut {
                        io::Error::new(e.kind(), "等待投屏首帧超时")
                    } else {
                        e
                    }
                })?
            } else {
                Packet::read(&mut self.video)?
            };
            match packet {
                Packet::Session(info) => {
                    self.size = (info.width, info.height);
                    self.control.set_size(info.width, info.height);
                    // 编码器可在同尺寸下重启，后续 SPS/PPS 与旧会话无关。
                    decoder = Decoder::new(self.size)?;
                }
                Packet::Media { data, .. } => {
                    if let Some(frame) = decoder.feed(&data)? {
                        if first_frame.take().is_some() {
                            self.video.set_read_timeout(None)?;
                        }
                        on_frame(frame);
                    }
                }
            }
        }
    }
}

impl Drop for VideoSession {
    fn drop(&mut self) {
        self.control.disconnect();
    }
}
