//! 真实流的端到端测试。
//!
//! 需要一台已就绪的模拟器。默认忽略。
//!
//!   WINDROID_SERIAL=emulator-5556 cargo test -p stream -- --ignored --nocapture
//!
//! 这个测试证明的是「我们自己写的 client 能从 scrcpy-server 拿到可解码的
//! 视频」。官方 client 能跑不代表我们的能跑——协议细节（会话包承载尺寸、
//! config 帧需要前置拼接）任何一处理解错都会表现为「连上了但没有画面」。

use std::time::{Duration, Instant};

use stream::{Decoder, Packet};

fn serial() -> String {
    std::env::var("WINDROID_SERIAL").unwrap_or_else(|_| "emulator-5554".to_owned())
}

fn server_jar() -> String {
    std::env::var("WINDROID_SERVER_JAR").unwrap_or_else(|_| {
        "/opt/homebrew/Cellar/scrcpy/5.0.1/share/scrcpy/scrcpy-server".to_owned()
    })
}

#[test]
#[ignore = "需要一台已就绪的模拟器"]
fn our_client_decodes_real_frames() {
    let serial = serial();
    let port = 27184;
    stream::launch_server(&serial, port, &server_jar()).expect("启动 server");
    std::thread::sleep(Duration::from_secs(2));

    let mut socket = stream::VideoSocket::connect(port, true).expect("连接");
    println!("[握手] device={} codec={:?} size={:?}", socket.device_name, socket.codec, socket.size);
    assert!(socket.codec.supported(), "M1a 只支持 H.264");
    assert!(socket.size.0 > 0 && socket.size.1 > 0, "会话包必须给出尺寸");

    let mut decoder = Decoder::spawn(socket.size.0, socket.size.1).expect("解码器");

    // config 帧必须前置拼接到下一个媒体帧：单独喂给解码器没有 SPS/PPS 可解。
    let mut pending_config: Option<Vec<u8>> = None;

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut frames = 0;
    let mut packets = 0;
    let mut last_nudge = Instant::now();
    while Instant::now() < deadline && frames < 30 {
        // 静止画面时 scrcpy 不发帧。主动制造变化，否则测的是「空闲」而不是「解码」。
        if last_nudge.elapsed() > Duration::from_millis(300) {
            let _ = std::process::Command::new(
                std::env::var("WINDROID_ADB").unwrap_or_else(|_| "adb".to_owned()),
            )
            .args(["-s", &serial, "shell", "input", "keyevent", "KEYCODE_DPAD_DOWN"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
            last_nudge = Instant::now();
        }
        let Some(pkt) = socket.next_packet().expect("读包") else { continue };
        match pkt {
            Packet::Session(info) => println!("[会话] {:?}", info),
            Packet::Media { data, config, key, .. } => {
                packets += 1;
                if config {
                    pending_config = Some(data);
                    continue;
                }
                if let Some(cfg) = pending_config.take() {
                    decoder.feed(&cfg).expect("喂 config");
                }
                if key {
                    println!("[关键帧] {packets} 号包, {} 字节", data.len());
                }
                decoder.feed(&data).expect("喂帧");
                while let Some(frame) = decoder.try_next() {
                    frames += 1;
                    if frames == 1 {
                        // 首帧必须不是全零：全零说明 SPS/PPS 没接上或解码未启动。
                        let non_zero = frame.bytes.iter().any(|&b| b != 0);
                        println!("[首帧] {} 字节, 非全零={}", frame.bytes.len(), non_zero);
                        assert!(non_zero, "首帧全零：解码未真正产出像素");
                    }
                }
            }
        }
    }

    println!("[结果] packets={packets} frames={frames}");
    assert!(frames >= 30, "20 秒内应解出至少 30 帧, 实际 {frames}");
    socket.shutdown();
}
