//! H.264 解码。
//!
//! 用 ffmpeg 作为独立进程，而不是链接 libavcodec：解码是本项目唯一的重型
//! 外部依赖，进程隔离让它的崩溃、内存泄漏、线程模型都不影响主进程。
//!
//! 这个选择是 M1a 的临时方案。生产实现应换成 VideoToolbox（macOS 硬解），
//! 理由不是性能而是功耗与授权：软解 1080p 长期占用一个核。

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

/// 一帧原始像素，BGRA。
pub struct Frame {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// 解码器：持续进程，输入 Annex-B 码流，输出 BGRA 帧。
///
/// 读帧在独立线程上进行。单线程「喂一包、读一帧」会死锁：解码器需要攒够
/// 输入才产出第一帧，而调用方正阻塞在读上，谁也无法推进。实测表现是
/// 「握手成功、关键帧已收到，然后永远没画面」。
pub struct Decoder {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<Frame>,
    /// 最近一帧。用来实现「取最新」而不必等下一帧。
    latest: Arc<Mutex<Option<Frame>>>,
}

impl Decoder {
    /// 启动解码器。
    ///
    /// **不要加 `-fflags nobuffer` 或 `-probesize`**：实测这两个参数会让
    /// ffmpeg 从管道读 h264 时解不出任何帧（输出 0 字节），且不报错。
    /// 看起来像低延迟优化，实际是静默失效。
    pub fn spawn(width: u32, height: u32) -> io::Result<Self> {
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel", "error",
                "-f", "h264",
                "-i", "pipe:0",
                "-f", "rawvideo",
                "-pix_fmt", "bgra",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdin = child.stdin.take().expect("stdin");
        let mut stdout = child.stdout.take().expect("stdout");
        let frame_bytes = (width as usize) * (height as usize) * 4;
        let (tx, frames) = mpsc::channel::<Frame>();
        let latest = Arc::new(Mutex::new(None));
        let latest_for_thread = Arc::clone(&latest);

        thread::Builder::new()
            .name("decode-out".to_owned())
            .spawn(move || {
                let mut buf = vec![0u8; frame_bytes];
                // read_exact 而不是 read：短读会把半个帧当整帧交出去，
                // 表现为画面撕裂。读到 Err 说明 ffmpeg 退出，线程随之结束。
                while stdout.read_exact(&mut buf).is_ok() {
                    if let Ok(mut slot) = latest_for_thread.lock() {
                        *slot = Some(Frame { bytes: buf.clone(), width, height });
                    }
                    // 无人消费时丢弃：视频只关心最新，阻塞会让延迟无限增长。
                    if tx.send(Frame { bytes: buf.clone(), width, height }).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn decode reader");

        Ok(Self { child, stdin, frames, latest })
    }

    /// 送入一个编码帧。
    pub fn feed(&mut self, data: &[u8]) -> io::Result<()> {
        self.stdin.write_all(data)?;
        self.stdin.flush()
    }

    /// 阻塞等下一帧。
    pub fn next_frame(&self) -> Option<Frame> {
        self.frames.recv().ok()
    }

    /// 取最近一帧，不阻塞。渲染线程用这个：它只关心现在该显示什么。
    pub fn latest_frame(&self) -> Option<Frame> {
        self.latest.lock().ok().and_then(|slot| {
            slot.as_ref().map(|f| Frame { bytes: f.bytes.clone(), width: f.width, height: f.height })
        })
    }

    /// 尝试非阻塞取一帧。
    pub fn try_next(&self) -> Option<Frame> {
        self.frames.try_recv().ok()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
