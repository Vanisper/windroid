//! 流域：scrcpy 协议。
//!
//! 分工严格：本 crate 只把 socket 字节变成「带时间戳的编码帧」，再变成
//! 「原始像素」。它不认识窗口、不认识 NSView、不关心谁在显示。
//! 表面与输入属于呈现域。
//!
//! 协议事实见 docs/实测/03-流域.md。有两处与直觉不同，都在注释里标了。

use std::io::{self, Read};
use std::net::TcpStream;
use std::time::Duration;

pub mod decode;
pub mod packet;

pub use decode::{Decoder, Frame};
pub use packet::{Codec, Packet, SessionInfo};

/// 协议常量。
pub mod consts {
    /// server 的握手字段长度。定长 + \0 填充，不是长度前缀。
    pub const DEVICE_NAME_LEN: usize = 64;
    /// 包头发送长度。
    pub const HEADER_LEN: usize = 12;
    /// bit63：会话包。
    pub const FLAG_SESSION: u64 = 1 << 63;
    /// bit62：配置帧（SPS/PPS）。
    pub const FLAG_CONFIG: u64 = 1 << 62;
    /// bit61：关键帧。
    pub const FLAG_KEY: u64 = 1 << 61;
    /// PTS 占低 61 位。
    pub const PTS_MASK: u64 = (1 << 61) - 1;
}

/// 一条已建立的视频连接。
pub struct VideoSocket {
    stream: TcpStream,
    /// 视频尺寸。来自会话包，不是启动参数——实际尺寸受 `max_size` 与设备方向影响。
    pub size: (u32, u32),
    pub codec: Codec,
    pub device_name: String,
}

impl VideoSocket {
    /// 连接并完成握手。
    ///
    /// `forward` 决定是否期待 dummy byte：只有 adb forward（client 连、
    /// device 接）模式才有，reverse 模式没有。写错就会把 device name 的
    /// 首字节吃掉，表现为「设备名少一个字符，之后全部错位」。
    pub fn connect(port: u16, forward: bool) -> io::Result<Self> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(15)))?;

        if forward {
            let mut dummy = [0u8; 1];
            stream.read_exact(&mut dummy)?;
        }

        let mut name = [0u8; consts::DEVICE_NAME_LEN];
        stream.read_exact(&mut name)?;
        let device_name = String::from_utf8_lossy(&name)
            .trim_end_matches('\0')
            .to_owned();

        let mut codec_raw = [0u8; 4];
        stream.read_exact(&mut codec_raw)?;
        let codec = Codec::from_id(u32::from_be_bytes(codec_raw))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "未知 codec id"))?;

        let mut me = Self { stream, size: (0, 0), codec, device_name };
        me.wait_session()?;
        Ok(me)
    }

    /// 读第一个会话包，取得视频尺寸。
    ///
    /// 会话包只有 12 字节 header，没有 payload，但它**不是空的**：
    /// 12 字节里编码了宽高。把它当 media packet 去读 payload 会让整个
    /// 后续数据错位——实测表现是读到荒谬的包长然后卡死。
    fn wait_session(&mut self) -> io::Result<()> {
        loop {
            let mut header = [0u8; consts::HEADER_LEN];
            self.stream.read_exact(&mut header)?;
            let flags = u64::from_be_bytes(header[0..8].try_into().unwrap());
            if flags & consts::FLAG_SESSION != 0 {
                let width = (flags & consts::PTS_MASK) as u32;
                let height = u32::from_be_bytes(header[8..12].try_into().unwrap());
                if width > 0 && height > 0 {
                    self.size = (width, height);
                }
                return Ok(());
            }
            // 尺寸未知前的媒体帧无法使用，丢掉。
        }
    }

    /// 读下一个包。
    ///
    /// 返回 `Ok(None)` 表示这段时间内没有新数据——**这不是错误**。
    /// scrcpy 只在画面变化时发帧，静止的屏幕可以让连接长时间静默。
    /// 把它当错误处理会导致「界面停留几秒就断开」。
    pub fn next_packet(&mut self) -> io::Result<Option<Packet>> {
        let mut header = [0u8; consts::HEADER_LEN];
        match self.stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let flags = u64::from_be_bytes(header[0..8].try_into().unwrap());

        if flags & consts::FLAG_SESSION != 0 {
            let width = (flags & consts::PTS_MASK) as u32;
            let height = u32::from_be_bytes(header[8..12].try_into().unwrap());
            if width > 0 && height > 0 {
                self.size = (width, height);
            }
            return Ok(Some(Packet::Session(SessionInfo { width, height })));
        }

        let size = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
        let mut body = vec![0u8; size];
        self.stream.read_exact(&mut body)?;
        Ok(Some(Packet::Media {
            pts_us: flags & consts::PTS_MASK,
            key: flags & consts::FLAG_KEY != 0,
            config: flags & consts::FLAG_CONFIG != 0,
            data: body,
        }))
    }

    /// 关闭连接。server 会随之退出。
    pub fn shutdown(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// 把 scrcpy-server 推上设备并启动，返回本地端口。
pub fn launch_server(serial: &str, local_port: u16, server_jar: &str) -> io::Result<()> {
    let remote = "/data/local/tmp/scrcpy-server.jar";
    adb::push(serial, server_jar, remote)?;
    adb::forward(serial, local_port, "localabstract:scrcpy")?;

    // 版本号必须与 client 严格一致，否则 server 直接抛异常退出。
    let cmd = format!(
        "CLASSPATH={remote} app_process / com.genymobile.scrcpy.Server {} \
         log_level=error audio=false control=false tunnel_forward=true max_size=0 max_fps=0",
        crate::SERVER_VERSION
    );
    // 后台启动：server 会一路推流直到连接断开，不能等它返回。
    std::process::Command::new(
        std::env::var("WINDROID_ADB").unwrap_or_else(|_| "adb".to_owned()),
    )
    .args(["-s", serial, "shell", &cmd])
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()?;
    Ok(())
}

/// server jar 的版本，必须与 client 严格相等。
pub const SERVER_VERSION: &str = "5.0.1";
