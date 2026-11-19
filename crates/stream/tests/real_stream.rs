//! 在显式指定的测试实例上同时建立设备与应用会话。由主线协调执行。
//! WINDROID_SERIAL 必填，WINDROID_TEST_ADB 与 WINDROID_TEST_SERVER_JAR 指向显式工具与准备好的 5.0.1 JAR。
//! 可设置 WINDROID_STREAM_PACKAGE 选择测试 APK，默认 com.android.settings。

#![cfg(target_os = "macos")]

use std::env;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use stream::{DisplayTarget, Frame, InputEvent, VideoSession};

#[test]
#[ignore = "需要显式指定的私有测试模拟器与已安装应用"]
fn our_client_decodes_real_frames() {
    let serial = env::var("WINDROID_SERIAL").expect("指定主线协调的测试 serial");
    let adb = PathBuf::from(env::var_os("WINDROID_TEST_ADB").expect("指定测试用 adb"));
    let jar =
        PathBuf::from(env::var_os("WINDROID_TEST_SERVER_JAR").expect("指定测试用 scrcpy-server"));
    let package =
        env::var("WINDROID_STREAM_PACKAGE").unwrap_or_else(|_| "com.android.settings".into());
    let device = VideoSession::start(&adb, &serial, &jar, DisplayTarget::Device).unwrap();
    let app = VideoSession::start(&adb, &serial, &jar, DisplayTarget::App { package }).unwrap();
    assert_eq!(device.display_id(), 0);
    assert_ne!(app.display_id(), 0);
    println!(
        "{} · 设备屏 {:?}，应用屏 {} {:?}",
        device.device_name,
        device.size(),
        app.display_id(),
        app.size()
    );
    let run = |session: VideoSession| {
        let control = session.control();
        let (tx, rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            session.run(move |frame| {
                let _ = tx.try_send(frame);
            })
        });
        (control, rx, worker)
    };
    let (dc, device_frames, dw) = run(device);
    let (ac, app_frames, aw) = run(app);
    let frame = |rx: &mpsc::Receiver<Frame>| {
        let frame = rx.recv_timeout(Duration::from_secs(15)).ok()?;
        let (pixels, _) = frame.bytes.as_chunks::<4>();
        Some(
            frame.bytes.len() == (frame.width * frame.height * 4) as usize
                && pixels.iter().any(|p| p[..3] != [0, 0, 0]),
        )
    };
    let device_ok = frame(&device_frames);
    let app_ok = frame(&app_frames);
    dc.shutdown();
    dw.join().unwrap().expect("结束设备会话");
    // 关闭设备会话后，另一会话仍可独立接收键码并输出后续帧。
    let _ = app_frames.try_recv();
    for down in [true, false] {
        ac.send(InputEvent::Key {
            down,
            keycode: 20,
            repeat: 0,
            meta_state: 0,
        })
        .unwrap();
    }
    let still_live = frame(&app_frames);
    ac.shutdown();
    aw.join().unwrap().expect("结束应用会话");
    assert_eq!(
        (device_ok, app_ok, still_live),
        (Some(true), Some(true), Some(true))
    );
}
