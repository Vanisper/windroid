//! 使用系统 VideoToolbox 解码 H.264。解码状态与目标显示的编码会话同寿命。

use crate::packet::{frame_len, invalid};
use std::io;

/// 连续 BGRA，每行 width × 4 字节。
pub struct Frame {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Android MediaCodec 的 Annex-B 可以混用 3/4 字节起始码。
fn annex_b(data: &[u8], mut visit: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
    let marker = |bytes: &[u8]| bytes.windows(3).position(|w| w == [0, 0, 1]);
    let first = marker(data)
        .filter(|&i| data[..i].iter().all(|b| *b == 0))
        .ok_or_else(|| invalid("H.264 缺少 Annex-B 起始码"))?;
    let mut tail = &data[first + 3..];
    loop {
        let next = marker(tail);
        let unit = &tail[..next.unwrap_or(tail.len())];
        let end = unit
            .iter()
            .rposition(|b| *b != 0)
            .ok_or_else(|| invalid("H.264 NAL 为空"))?;
        if unit[0] & 0x80 != 0 {
            return Err(invalid("H.264 NAL 头无效"));
        }
        visit(&unit[..=end])?;
        let Some(next) = next else { return Ok(()) };
        tail = &tail[next + 3..];
    }
}

pub(crate) struct Decoder {
    size: (u32, u32),
    sps: Vec<u8>,
    pps: Vec<u8>,
    native: Option<native::Session>,
}

impl Decoder {
    pub(crate) fn new(size: (u32, u32)) -> io::Result<Self> {
        frame_len(size.0, size.1)?;
        Ok(Self {
            size,
            sps: Vec::new(),
            pps: Vec::new(),
            native: None,
        })
    }

    pub(crate) fn feed(&mut self, data: &[u8]) -> io::Result<Option<Frame>> {
        let mut sample = Vec::new();
        let mut key = false;
        let mut picture = false;
        annex_b(data, |unit| {
            let kind = unit[0] & 31;
            if kind == 7 || kind == 8 {
                if unit.len() > u16::MAX.into() {
                    return Err(invalid("H.264 参数集过大"));
                }
                let dest = if kind == 7 {
                    &mut self.sps
                } else {
                    &mut self.pps
                };
                if dest != unit {
                    *dest = unit.to_vec();
                    self.native = None;
                }
            } else {
                // VideoToolbox 接受 4 字节长度前缀的 AVCC 样本。
                sample.extend((unit.len() as u32).to_be_bytes());
                sample.extend(unit);
                picture |= (1..=5).contains(&kind);
                key |= kind == 5;
            }
            Ok(())
        })?;
        if !picture {
            return Ok(None);
        }
        if self.native.is_none() {
            if !key {
                return Ok(None);
            }
            if self.sps.is_empty() || self.pps.is_empty() {
                return Err(invalid("H.264 关键帧缺少 SPS/PPS"));
            }
            self.native = Some(native::Session::new(&self.sps, &self.pps, self.size)?);
        }
        self.native.as_mut().unwrap().decode(&sample, self.size)
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::ffi::c_void;
    use std::ptr::{self, NonNull};

    unsafe extern "C" {
        fn windroid_vt_new(
            sps: *const u8,
            sps_len: usize,
            pps: *const u8,
            pps_len: usize,
            w: u32,
            h: u32,
            out: *mut *mut c_void,
        ) -> i32;
        fn windroid_vt_decode(
            decoder: *mut c_void,
            data: *const u8,
            len: usize,
            w: u32,
            h: u32,
            out: *mut u8,
            copied: *mut i32,
        ) -> i32;
        fn windroid_vt_free(decoder: *mut c_void);
    }

    fn check(status: i32) -> io::Result<()> {
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "VideoToolbox 解码失败 (OSStatus {status})"
            )))
        }
    }

    pub(super) struct Session(NonNull<c_void>);

    impl Session {
        pub(super) fn new(sps: &[u8], pps: &[u8], (width, height): (u32, u32)) -> io::Result<Self> {
            let mut out = ptr::null_mut();
            // 参数集在本次调用内复制成格式描述。成功时 out 转移一个有效 session。
            check(unsafe {
                windroid_vt_new(
                    sps.as_ptr(),
                    sps.len(),
                    pps.as_ptr(),
                    pps.len(),
                    width,
                    height,
                    &mut out,
                )
            })?;
            Ok(Self(
                NonNull::new(out).ok_or_else(|| invalid("VideoToolbox 未提供解码会话"))?,
            ))
        }

        pub(super) fn decode(
            &mut self,
            sample: &[u8],
            (width, height): (u32, u32),
        ) -> io::Result<Option<Frame>> {
            let len = frame_len(width, height)?;
            let mut bytes = Vec::with_capacity(len);
            let mut copied = 0;
            // 同步回调只写入该容量内的 BGRA 行。copied=1 表示所有行都已初始化。
            check(unsafe {
                windroid_vt_decode(
                    self.0.as_ptr(),
                    sample.as_ptr(),
                    sample.len(),
                    width,
                    height,
                    bytes.as_mut_ptr(),
                    &mut copied,
                )
            })?;
            if copied == 0 {
                return Ok(None);
            }
            unsafe {
                bytes.set_len(len);
            }
            Ok(Some(Frame {
                bytes,
                width,
                height,
            }))
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            unsafe {
                windroid_vt_free(self.0.as_ptr());
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::*;
    pub(super) struct Session;
    impl Session {
        pub(super) fn new(_: &[u8], _: &[u8], _: (u32, u32)) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "当前视频解码需要 macOS VideoToolbox",
            ))
        }
        pub(super) fn decode(&mut self, _: &[u8], _: (u32, u32)) -> io::Result<Option<Frame>> {
            unreachable!()
        }
    }
}
