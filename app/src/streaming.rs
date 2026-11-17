//! 把真实视频流接到窗口上。
//!
//! 这条链路跨三个执行上下文：socket 读取与解码在其中两个后台线程，
//! 帧上传在主线程。它们之间只通过一个「最新帧」槽位相连——不排队、
//! 不等待，因为视频只关心现在该显示什么。
//!
//! 用真实后端还是假后端由环境变量决定，见 [backend]。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use host::video::FrameSlot;
use stream::{Decoder, Packet};

/// 一条正在运行的流。Drop 时停掉所有线程并断开连接。
pub struct Stream {
    stop: Arc<AtomicBool>,
    pub url: String,
}

impl Stream {
    /// 启动流：推 server、连 socket、起解码与转发的后台线程。
    pub fn start(serial: String, slot: FrameSlot, local_port: u16) -> Result<Self, String> {
        let jar = server_jar();
        stream::launch_server(&serial, local_port, &jar)
            .map_err(|e| format!("启动 scrcpy-server 失败: {e}"))?;

        // server 需要一点时间开始监听。轮询比固定 sleep 更可靠：
        // 冷启动与热启动的差别可以是几秒。
        let mut socket = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut last_err = String::new();
        while std::time::Instant::now() < deadline {
            match stream::VideoSocket::connect(local_port, true) {
                Ok(s) => {
                    socket = Some(s);
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
        let mut socket = socket.ok_or_else(|| format!("连接视频端口失败: {last_err}"))?;

        let (w, h) = socket.size;
        let decoder = Decoder::spawn(w, h).map_err(|e| format!("启动解码器失败: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let url = format!("{serial}/{w}x{h}");

        std::thread::Builder::new()
            .name("stream-reader".to_owned())
            .spawn(move || {
                let mut decoder = decoder;
                // config 帧必须前置拼接到下一个媒体帧：单独送给解码器没有
                // SPS/PPS 可解，表现为「连上了但永远黑屏」。
                let mut pending_config: Option<Vec<u8>> = None;


                while !stop_for_thread.load(Ordering::Relaxed) {
                    match socket.next_packet() {
                        Ok(Some(Packet::Session(_))) => continue,
                        Ok(Some(Packet::Media { data, config, .. })) => {
                            if config {
                                pending_config = Some(data);
                                continue;
                            }
                            // 不合并成 let-chain：这是一个有独立含义的步骤
                            // （把暂存的 SPS/PPS 先送进去），嵌套读起来更清楚
                            // 它和下面的「送媒体帧」是两件事。
                            #[allow(clippy::collapsible_if)]
                            if let Some(cfg) = pending_config.take() {
                                if decoder.feed(&cfg).is_err() {
                                    break;
                                }
                            }
                            if decoder.feed(&data).is_err() {
                                break;
                            }
                        }
                        // 画面静止时 scrcpy 不发帧。这不是错误。
                        Ok(None) => continue,
                        Err(e) => {
                            log::warn!("视频流中断: {e}");
                            break;
                        }
                    }

                    // 把已解出的帧放进槽位。只保留最新一帧：旧帧没有价值，
                    // 堆积只会让画面越来越滞后。
                    while let Some(frame) = decoder.try_next() {
                        if let Ok(mut s) = slot.lock() {
                            *s = Some(frame);
                        }
                    }
                }
                socket.shutdown();
            })
            .map_err(|e| format!("启动流线程失败: {e}"))?;

        Ok(Self { stop, url })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// scrcpy-server jar 的位置。
///
/// M1a 先从系统安装的 scrcpy 取。生产版本应随 app 分发固定版本的 jar，
/// 因为 server 与 client 的版本必须严格相等（协议没有版本协商）。
fn server_jar() -> String {
    if let Ok(p) = std::env::var("WINDROID_SERVER_JAR") {
        return p;
    }
    "/opt/homebrew/Cellar/scrcpy/5.0.1/share/scrcpy/scrcpy-server".to_owned()
}
