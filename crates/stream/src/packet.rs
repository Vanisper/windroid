//! 视频包与编码格式。
//!
//! 包布局见 <https://github.com/Genymobile/scrcpy/blob/v5.0.1/server/src/main/java/com/genymobile/scrcpy/device/Streamer.java>。

use std::io::{self, Read};

const SESSION: u64 = 1 << 63;
const CONFIG: u64 = 1 << 62;
const KEY: u64 = 1 << 61;
const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn frame_len(width: u32, height: u32) -> io::Result<usize> {
    let pixels = u64::from(width) * u64::from(height);
    if width > u16::MAX.into() || height > u16::MAX.into() || !(1..=16_777_216).contains(&pixels) {
        return Err(invalid("视频帧尺寸超出范围"));
    }
    Ok(pixels as usize * 4)
}

/// 编码格式。id 是 ASCII 四字节按大端读成的 u32。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
    Vp8,
    Vp9,
}

impl Codec {
    pub fn from_id(id: u32) -> Option<Self> {
        Some(match id {
            0x6832_3634 => Self::H264,
            0x6832_3635 => Self::H265,
            0x0061_7631 => Self::Av1,
            0x0076_7038 => Self::Vp8,
            0x0076_7039 => Self::Vp9,
            // 0 = 显式禁用，1 = 配置错误：两者都不是可用编码。
            _ => return None,
        })
    }

    /// 是否已有解码实现。
    ///
    /// 会话启动时显式协商 H.264。
    pub fn supported(self) -> bool {
        matches!(self, Self::H264)
    }
}

/// 会话信息。尺寸随会话包更新。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionInfo {
    pub width: u32,
    pub height: u32,
}

/// 从 socket 读到的一个包。
#[derive(Debug)]
pub enum Packet {
    /// 会话参数。不含视频数据。
    Session(SessionInfo),
    /// 视频数据。
    Media {
        /// 显示时间，微秒。
        pts_us: u64,
        key: bool,
        /// SPS/PPS 等配置数据。需要拼接到下一个媒体帧前面才能解码。
        config: bool,
        data: Vec<u8>,
    },
}

impl Packet {
    /// 连接建立后阻塞读完整包。画面静止时保持等待，关闭 socket 可随时唤醒。
    pub(crate) fn read(reader: &mut impl Read) -> io::Result<Self> {
        let mut header = [0; 12];
        reader.read_exact(&mut header)?;
        let flags = u64::from_be_bytes(header[..8].try_into().unwrap());
        let last = u32::from_be_bytes(header[8..12].try_into().unwrap());
        if flags & SESSION != 0 {
            // 高 32 位存 flags（含 client resize），中间 32 位才是 width。
            let width = u32::from_be_bytes(header[4..8].try_into().unwrap());
            frame_len(width, last)?;
            return Ok(Self::Session(SessionInfo {
                width,
                height: last,
            }));
        }
        let size = last as usize;
        if !(1..=MAX_PACKET_BYTES).contains(&size) {
            return Err(invalid("视频包长度超出范围"));
        }
        let mut data = vec![0; size];
        reader.read_exact(&mut data)?;
        Ok(Self::Media {
            pts_us: flags & (KEY - 1),
            key: flags & KEY != 0,
            config: flags & CONFIG != 0,
            data,
        })
    }
}
