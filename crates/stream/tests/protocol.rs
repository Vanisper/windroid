//! 假 adb 只分配资源。loopback 对端按 server v5.0.1 的先 video 后 control 顺序握手。
#![cfg(unix)]

use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use stream::{DisplayTarget, InputEvent, VideoSession};

const SERIAL: &str = "test-serial";
const PACKAGE: &str = "com.example.editor";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "windroid-stream-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let adb = dir.join("fake adb");
        fs::write(&adb, include_bytes!("fixtures/fake-adb.sh")).unwrap();
        fs::set_permissions(&adb, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(dir.join("server.jar"), b"fixture").unwrap();
        Self(dir)
    }

    fn start(&self, target: DisplayTarget) -> io::Result<VideoSession> {
        VideoSession::start_on(
            adb::Device::injected(&self.0.join("fake adb"), SERIAL),
            &self.0.join("server.jar"),
            target,
            || false,
        )
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.0.join("calls")).unwrap()
    }

    fn listen(&self) -> TcpListener {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        fs::write(
            self.0.join("next-port"),
            format!("{}\n", listener.local_addr().unwrap().port()),
        )
        .unwrap();
        listener
    }

    fn connect(&self, target: DisplayTarget) -> (VideoSession, Peer) {
        let listener = self.listen();
        let server = thread::spawn(move || {
            let port = listener.local_addr().unwrap().port();
            let (mut video, _) = listener.accept().unwrap();
            video.write_all(&[0]).unwrap();
            let (control, _) = listener.accept().unwrap();
            control
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut name = [0; 64];
            name[..4].copy_from_slice(b"Test");
            video.write_all(&name).unwrap();
            video.write_all(b"h264").unwrap();
            let mut peer = Peer {
                video,
                control,
                port,
            };
            peer.size(32, 32, false);
            peer
        });
        let session = self.start(target).unwrap();
        assert_eq!(session.device_name, "Test");
        assert_eq!(session.size(), (32, 32));
        (session, server.join().unwrap())
    }

    fn map(&self, port: u16) -> PathBuf {
        self.0.join(format!("map-{port}"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

struct Peer {
    video: TcpStream,
    control: TcpStream,
    port: u16,
}

impl Peer {
    fn size(&mut self, width: u32, height: u32, client_resize: bool) {
        self.video
            .write_all(&(0x8000_0000u32 | u32::from(client_resize)).to_be_bytes())
            .unwrap();
        self.video.write_all(&width.to_be_bytes()).unwrap();
        self.video.write_all(&height.to_be_bytes()).unwrap();
    }

    fn next(&mut self, expected: &[u8]) {
        let mut actual = vec![0; expected.len()];
        self.control.read_exact(&mut actual).unwrap();
        assert_eq!(actual, expected);
    }

    #[cfg(target_os = "macos")]
    fn packet(&mut self, flags: u64, bytes: &[u8]) {
        self.video.write_all(&flags.to_be_bytes()).unwrap();
        self.video
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .unwrap();
        // 故意分两次写。短读后继续读同一媒体包。
        let (head, tail) = bytes.split_at(bytes.len() / 2);
        self.video.write_all(head).unwrap();
        self.video.write_all(tail).unwrap();
    }
}

fn hex(text: &str) -> Vec<u8> {
    let s: String = text.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn app() -> DisplayTarget {
    DisplayTarget::App {
        package: PACKAGE.to_owned(),
    }
}
fn key() -> InputEvent {
    InputEvent::Key {
        down: true,
        keycode: 66,
        repeat: 2,
        meta_state: 1,
    }
}
fn touch() -> InputEvent {
    InputEvent::Touch {
        action: 0,
        x: 17,
        y: 7,
        width: 32,
        height: 32,
        action_button: 1,
        buttons: 1,
    }
}

#[test]
fn parallel_displays_route_input_start_and_teardown_to_their_own_socket() {
    let f = Fixture::new();
    let (device, mut device_peer) = f.connect(DisplayTarget::Device);
    let (app, mut app_peer) = f.connect(app());
    assert_eq!(device.display_id(), 0);
    assert_eq!(app.display_id(), 57);
    assert_ne!(device_peer.port, app_peer.port);
    let local_map = |path: &Path| {
        fs::read_to_string(path)
            .unwrap()
            .split_whitespace()
            .last()
            .unwrap()
            .to_owned()
    };
    let device_socket = local_map(&f.map(device_peer.port));
    let app_socket = local_map(&f.map(app_peer.port));
    assert_ne!(device_socket, app_socket);
    let calls = f.calls();
    for socket in [&device_socket, &app_socket] {
        let id = socket.strip_prefix("localabstract:scrcpy_").unwrap();
        assert_eq!(id.len(), 8);
        assert!(u32::from_str_radix(id, 16).unwrap() <= 0x7fff_ffff);
        assert!(calls.contains(&format!("scid={id} ")));
        assert!(calls.contains(&format!(
            "CLASSPATH=/data/local/tmp/windroid-scrcpy-{id}/server.jar"
        )));
    }
    assert!(calls.contains("display_id=0"));
    assert!(calls.contains("new_display="));
    assert!(calls.contains("vd_destroy_content=false"));
    assert!(!calls.contains(PACKAGE));
    app_peer.next(&[16, PACKAGE.len() as u8]);
    app_peer.next(PACKAGE.as_bytes());
    let dc = device.control();
    let ac = app.control();
    dc.send(key()).unwrap();
    device_peer.next(&hex("00 00 00000042 00000002 00000001"));
    ac.send(touch()).unwrap();
    app_peer.next(&hex(
        "02 00 ffffffffffffffff 00000011 00000007 0020 0020 ffff 00000001 00000001",
    ));
    ac.send(InputEvent::Scroll {
        x: 5,
        y: 18,
        width: 32,
        height: 32,
        h: -1.,
        v: 16.,
        buttons: 0,
    })
    .unwrap();
    app_peer.next(&hex("03 00000005 00000012 0020 0020 f800 7fff 00000000"));
    dc.send(InputEvent::Text(format!("{}界", "a".repeat(299))))
        .unwrap();
    device_peer.next(&hex("01 0000012b"));
    device_peer.next(&[b'a'; 299]);
    device_peer.next(&hex("01 00000003 e7958c"));
    assert_eq!(
        dc.resize(64, 32).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    drop(device);
    assert!(!f.map(device_peer.port).exists());
    assert!(f.map(app_peer.port).exists());
    assert_eq!(
        dc.send(key()).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    ac.resize(64, 32).unwrap();
    app_peer.next(&hex("15 0040 0020"));
    drop(app);
    assert!(!f.map(app_peer.port).exists());
    assert!(!f.calls().contains("--remove-all"));
}

#[test]
#[cfg(target_os = "macos")]
fn decoder_delivers_known_pixels_and_reconfigures_after_client_resize() {
    // libx264 的两张单色 IDR，均用 yuv420p/zerolatency 生成。保留混合长度起始码。
    let sps1 = "6742c00ada25b011000003000100000300028f1226a0";
    let sps2 = "6742c00ada116c0440000003004000000300a3c489a8";
    let f = Fixture::new();
    let (session, mut peer) = f.connect(DisplayTarget::Device);
    let control = session.control();
    let (tx, rx) = mpsc::sync_channel(2);
    let worker = thread::spawn(move || session.run(move |f| tx.try_send(f).unwrap()));
    let config = |sps: &str| hex(&format!("00000001{sps}00000168ce0fc8"));
    peer.packet(1 << 62, &config(sps1));
    peer.packet(
        1 << 61,
        &hex("0000016588843a118a000218f1c00040f63800087949d75e"),
    );
    let red = rx.recv_timeout(Duration::from_secs(4)).unwrap();
    assert_eq!(
        (red.width, red.height, red.bytes.len()),
        (32, 32, 32 * 32 * 4)
    );
    let (pixels, _) = red.bytes.as_chunks::<4>();
    assert!(
        pixels
            .iter()
            .all(|&[b, g, r, a]| r > 180 && g < 60 && b < 60 && a == 255)
    );
    peer.size(64, 32, true);
    peer.packet(1 << 62, &config(sps2));
    peer.packet(
        (1 << 61) | 476,
        &hex("0000016588843a118a00023171c00043ca38000805c9c9c9d75d75e0"),
    );
    let blue = rx.recv_timeout(Duration::from_secs(4)).unwrap();
    assert_eq!(
        (blue.width, blue.height, blue.bytes.len()),
        (64, 32, 64 * 32 * 4)
    );
    let (pixels, _) = blue.bytes.as_chunks::<4>();
    assert!(
        pixels
            .iter()
            .all(|&[b, g, r, a]| b > 180 && g < 60 && r < 60 && a == 255)
    );
    control.send(touch()).unwrap(); // 属于已失效的 32×32 帧
    control.send(key()).unwrap();
    peer.next(&hex("00 00 00000042 00000002 00000001"));
    control.shutdown();
    worker.join().unwrap().expect("主动关闭成功");
}

#[test]
fn control_loss_wakes_idle_video_and_rebound_forward_is_preserved() {
    let f = Fixture::new();
    let (session, peer) = f.connect(DisplayTarget::Device);
    let mapping = f.map(peer.port);
    let replacement = format!(
        "test-serial tcp:{} localabstract:scrcpy_7fffffff\n",
        peer.port
    );
    fs::write(&mapping, &replacement).unwrap();
    peer.control.shutdown(Shutdown::Write).unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || tx.send(session.run(|_| {})).unwrap());
    assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap().is_err());
    worker.join().unwrap();
    assert_eq!(fs::read_to_string(&mapping).unwrap(), replacement);
    assert!(!f.calls().contains("--remove tcp:"));
}

#[test]
fn invalid_packages_push_and_startup_failures_are_not_success() {
    let f = Fixture::new();
    for name in ["", "+term", "?Settings", "a..b", "a;b", &"a".repeat(256)] {
        assert_eq!(
            f.start(DisplayTarget::App {
                package: name.into()
            })
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert!(!f.0.join("calls").exists());
    fs::write(f.0.join("fail-push"), "").unwrap();
    let e = f.start(DisplayTarget::Device).err().unwrap();
    assert!(e.to_string().contains("push refused"));
    assert!(f.calls().contains("rmdir /data/local/tmp/windroid-scrcpy-"));
    assert!(!f.calls().contains("--no-rebind"));
    fs::remove_file(f.0.join("fail-push")).unwrap();
    fs::write(f.0.join("exit-server"), "").unwrap();
    let listener = f.listen();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let e = f.start(DisplayTarget::Device).err().unwrap();
    assert!(e.to_string().contains("server 已退出"), "{e}");
    assert!(!f.map(port).exists());
}

#[test]
fn cancelling_an_incomplete_handshake_never_sends_start_app() {
    use std::sync::{Arc, atomic::AtomicBool};
    let f = Fixture::new();
    let listener = f.listen();
    let port = listener.local_addr().unwrap().port();
    let stopped = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::clone(&stopped);
    let (accepted, rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        let (mut video, _) = listener.accept().unwrap();
        video.write_all(&[0]).unwrap();
        let (mut control, _) = listener.accept().unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        video.write_all(b"par").unwrap();
        accepted.send(()).unwrap();
        let mut bytes = Vec::new();
        control.read_to_end(&mut bytes).unwrap();
        bytes
    });
    thread::scope(|scope| {
        let f = &f;
        let start = scope.spawn(move || {
            VideoSession::start_on(
                adb::Device::injected(&f.0.join("fake adb"), SERIAL),
                &f.0.join("server.jar"),
                app(),
                || cancelled.load(Ordering::Acquire),
            )
        });
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        stopped.store(true, Ordering::Release);
        assert_eq!(
            start.join().unwrap().err().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
    });
    assert!(peer.join().unwrap().is_empty());
    assert!(!f.map(port).exists());
}

#[test]
fn unbounded_packet_length_and_invalid_display_size_abort_the_session() {
    let f = Fixture::new();
    for header in [
        hex("0000000000000000 ffffffff"),
        hex("80000000 ffffffff ffffffff"),
    ] {
        let (session, mut peer) = f.connect(DisplayTarget::Device);
        peer.video.write_all(&header).unwrap();
        assert_eq!(
            session.run(|_| {}).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(!f.map(peer.port).exists());
    }
}

/// 在同一个进程中先启动会话，再让服务在 forward 与长连接之间换成旧协议。
#[test]
fn daemon_changed_before_stream_launch_preserves_the_replacement() {
    const HELPER: &str = "WINDROID_STREAM_CHECK_REPLACEMENT";
    if std::env::var_os(HELPER).is_some() {
        let fixture = Fixture::new();
        let program = fixture.0.join("fake adb");
        fs::write(fixture.0.join("next-port"), "19000\n").unwrap();
        adb::ensure_server(&program).unwrap();
        let result = VideoSession::start(
            &program,
            SERIAL,
            &fixture.0.join("server.jar"),
            DisplayTarget::Device,
        );
        let issue = result.err().unwrap();
        assert!(issue.to_string().contains("不兼容"), "{issue}");
        let calls = fixture.calls();
        let commands: Vec<_> = calls
            .lines()
            .map(|line| line.split_whitespace().take(2).collect::<Vec<_>>())
            .collect();
        assert_eq!(commands.len(), 3, "{calls}");
        assert_eq!(commands[0], ["shell", "mkdir"]);
        assert_eq!(commands[1][0], "push");
        assert_eq!(commands[2], ["forward", "--no-rebind"]);
        return;
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    thread::scope(|threads| {
        let service = threads.spawn(|| {
            let mut count = 0;
            for connection in listener.incoming() {
                let mut socket = connection.unwrap();
                let mut query = [0; 16];
                if socket.read_exact(&mut query).is_err() {
                    break;
                }
                assert_eq!(&query, b"000chost:version");
                count += 1;
                socket
                    .write_all(if count <= 4 {
                        b"OKAY00040029"
                    } else {
                        b"OKAY00040028"
                    })
                    .unwrap();
            }
            count
        });
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon_changed_before_stream_launch_preserves_the_replacement",
            ])
            .env(HELPER, "1")
            .env_remove("ADB_SERVER_SOCKET")
            .env_remove("ANDROID_ADB_SERVER_ADDRESS")
            .env("ANDROID_ADB_SERVER_PORT", address.port().to_string())
            .status()
            .unwrap();
        drop(TcpStream::connect(address).unwrap());
        let probes = service.join().unwrap();
        assert!(result.success());
        assert_eq!(
            probes, 7,
            "启动前、mkdir、push、forward、长连接及两次清理都须握手"
        );
    });
}
