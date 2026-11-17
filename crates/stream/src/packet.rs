//! 视频包与编码格式。
//!
//! 只描述协议本身，不含解析之外的逻辑。

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
    /// M1a 只做 H.264：它是 Android 模拟器的默认输出，也是兼容性最好的选择。
    /// 其余格式留待需要时实现，不在没有需求时预先铺开。
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
