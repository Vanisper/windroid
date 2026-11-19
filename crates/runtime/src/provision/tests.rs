//! 流程测试的目录、归档、网络服务都在本机临时空间，不请求发布服务器。
use super::*;
use catalog::{Digest, Kind};
use sha2::{Digest as Hasher, Sha256};
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::atomic::AtomicU64;
use std::thread;
use std::time::{Duration, Instant};
use zip::write::SimpleFileOptions;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "windroid-provision-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture(bytes: &[u8], url: String) -> Artifact {
    Artifact {
        kind: Kind::Server,
        id: "server",
        name: "fixture",
        image_spec: None,
        version: "1",
        url: Box::leak(url.into_boxed_str()),
        bytes: bytes.len() as u64,
        digest: Digest::Sha256(Box::leak(
            format!("{:x}", Sha256::digest(bytes)).into_boxed_str(),
        )),
        prefix: None,
        unpacked: bytes.len() as u64,
        essentials: &[],
    }
}

fn controller(home: &Path, pkg: Artifact) -> Provisioner {
    Provisioner::configured(
        home.into(),
        RuntimePaths::managed(home),
        Box::leak(Box::new([pkg])),
        true,
    )
}

fn wait_for(f: impl Fn() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "等待测试前置条件超时"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn settled(p: &Provisioner) -> Preparation {
    wait_for(|| !p.owner.worker.state.lock().unwrap().active);
    p.snapshot()
}

/// 有效回包发送 Content-Length 和 Content-Range。挂起的首包一直等到暂停关闭连接，
/// 测试无需用外部网络速度猜测暂停时机。
struct Http {
    url: String,
    ranges: Arc<Mutex<Vec<usize>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Http {
    fn new(body: Vec<u8>, stall_first: bool, ignore_range: bool) -> Self {
        let socket = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = socket.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let reqs = Arc::clone(&ranges);
        let handle = thread::spawn(move || {
            while let Ok((mut stream, _)) = socket.accept() {
                if thread_stop.load(Ordering::Acquire) {
                    break;
                }
                let mut lines = BufReader::new(&mut stream);
                let mut start = 0;
                let mut line = String::new();
                loop {
                    line.clear();
                    if lines.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line
                        .to_ascii_lowercase()
                        .trim()
                        .strip_prefix("range: bytes=")
                    {
                        start = value.trim_end_matches('-').parse().unwrap();
                    }
                }
                reqs.lock().unwrap().push(start);
                if start != 0 && !ignore_range {
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\n",
                        start,
                        body.len() - 1,
                        body.len()
                    )
                    .unwrap();
                } else {
                    start = 0;
                    write!(stream, "HTTP/1.1 200 OK\r\n").unwrap();
                }
                write!(
                    stream,
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len() - start
                )
                .unwrap();
                if stall_first && reqs.lock().unwrap().len() == 1 {
                    stream.write_all(&body[start..start + 32_768]).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(8)))
                        .unwrap();
                    let _ = stream.read(&mut [0; 1]);
                } else {
                    // 忽略 Range 时 curl 可在收到头部后关闭连接。
                    let _ = stream.write_all(&body[start..]);
                }
            }
        });
        Self {
            url: format!("http://{addr}/artifact"),
            ranges,
            stop,
            handle: Some(handle),
        }
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let addr = self
            .url
            .strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        let _ = TcpStream::connect(addr);
        self.handle.take().unwrap().join().unwrap();
    }
}

#[test]
fn empty_overrides_use_private_paths_and_explicit_binaries_are_read_only() {
    use std::ffi::OsString;
    let t = Temp::new();
    let home = t.0.join("home");
    let paths = RuntimePaths::resolve(&home, |_| Some(OsString::new()));
    assert_eq!(paths.adb, home.join("runtime/sdk/platform-tools/adb"));
    assert_eq!(paths.server_jar, home.join("runtime/scrcpy-server.jar"));
    assert!(!paths.borrowed && !paths.read_only(Kind::Adb) && !paths.read_only(Kind::Server));

    let adb = t.0.join("custom-adb");
    fs::write(&adb, b"development binary").unwrap();
    fs::set_permissions(&adb, fs::Permissions::from_mode(0o755)).unwrap();
    let other_jar = t.0.join("custom.jar");
    fs::write(&other_jar, b"incompatible version").unwrap();
    let paths = RuntimePaths::resolve(&home, |key| match key {
        "WINDROID_ADB" => Some(OsString::from("custom-adb")),
        "WINDROID_SERVER_JAR" => Some(other_jar.as_os_str().into()),
        "PATH" => Some(t.0.as_os_str().into()),
        _ => None,
    });
    assert_eq!(paths.adb, adb);
    assert_eq!(paths.server_jar, other_jar);
    assert!(
        paths.read_only(Kind::Adb)
            && paths.read_only(Kind::Server)
            && !paths.read_only(Kind::Emulator)
    );
    let p = Provisioner::configured(home.clone(), paths, &CATALOG, true);
    let s = p.snapshot();
    assert!(
        s.components
            .iter()
            .any(|c| c.id == "platform-tools" && c.ready && c.borrowed)
    );
    assert!(
        s.components
            .iter()
            .any(|c| c.id == "scrcpy-server" && !c.ready && c.borrowed)
    );
    assert!(p.start().unwrap_err().contains("只读"));
    assert!(!home.exists());
    fs::set_permissions(&adb, fs::Permissions::from_mode(0o644)).unwrap();
    p.recheck().unwrap();
    assert!(
        p.snapshot()
            .components
            .iter()
            .any(|c| c.id == "platform-tools" && !c.ready)
    );
    assert_eq!(fs::read(other_jar).unwrap(), b"incompatible version");
}

#[test]
fn pause_restart_and_resume_from_exact_prefix() {
    let temp = Temp::new();
    let bytes: Vec<_> = (0..131_072).map(|i| (i % 251) as u8).collect();
    let http = Http::new(bytes.clone(), true, false);
    let pkg = fixture(&bytes, http.url.clone());
    let p = controller(&temp.0, pkg);
    let part = partial(&p.owner.worker.stage, pkg);
    assert_eq!(p.snapshot().phase, "missing");
    assert!(http.ranges.lock().unwrap().is_empty());
    p.start().unwrap();
    p.start().unwrap();
    wait_for(|| fs::metadata(&part).is_ok_and(|m| m.len() > 0));
    assert!(p.recheck().is_err());
    let second = controller(&temp.0, pkg);
    second.start().unwrap();
    assert!(settled(&second).error.unwrap().contains("独占"));
    p.pause().unwrap();
    assert_eq!(settled(&p).phase, "paused");
    assert!(!p.paths().server_jar.exists());
    let retained = fs::metadata(&part).unwrap().len();
    assert!((1..=32_768).contains(&retained));
    drop(p);
    let p = controller(&temp.0, pkg);
    assert_eq!(p.snapshot().phase, "paused");
    assert_eq!(p.snapshot().downloaded_bytes, retained);
    p.start().unwrap();
    let snap = settled(&p);
    assert_eq!(snap.phase, "ready", "{:?}", snap.error);
    assert_eq!(fs::read(&p.paths().server_jar).unwrap(), bytes);
    assert_eq!(*http.ranges.lock().unwrap(), [0, retained as usize]);
    assert!(!part.exists());
}

/// 显式在线冒烟：下载较小的真实 ZIP 和 JAR，验证私有 SDK 布局与协议组件。
#[test]
#[ignore = "downloads 16.8 MB from official release servers"]
fn official_platform_tools_and_server_install_into_private_home() {
    let t = Temp::new();
    let selected: Vec<_> = CATALOG
        .iter()
        .copied()
        .filter(|p| matches!(p.kind, Kind::Adb | Kind::Server))
        .collect();
    let p = Provisioner::configured(
        t.0.clone(),
        RuntimePaths::managed(&t.0),
        Box::leak(selected.into_boxed_slice()),
        SUPPORTED,
    );
    p.start().unwrap();
    let start = Instant::now();
    while p.owner.worker.state.lock().unwrap().active {
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "官方组件传输超时"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let snap = p.snapshot();
    assert_eq!(snap.phase, "ready", "{:?}", snap.error);
    assert_eq!(snap.downloaded_bytes, 16_844_484);
    assert!(p.paths().adb.is_file() && p.paths().server_jar.is_file());
}

fn stage(home: &Path, pkg: Artifact, bytes: &[u8]) -> PathBuf {
    let stage = home.join("staging/provision-v1");
    fs::create_dir_all(&stage).unwrap();
    let file = partial(&stage, pkg);
    fs::write(&file, bytes).unwrap();
    file
}

#[test]
fn ignored_range_restarts_once_without_concatenating_bodies() {
    let t = Temp::new();
    let bytes = vec![17; 65536];
    let http = Http::new(bytes.clone(), false, true);
    let pkg = fixture(&bytes, http.url.clone());
    stage(&t.0, pkg, &bytes[..8192]);
    let p = controller(&t.0, pkg);
    p.start().unwrap();
    let snap = settled(&p);
    assert_eq!(snap.phase, "ready", "{:?}", snap.error);
    assert_eq!(*http.ranges.lock().unwrap(), [8192, 0]);
}

#[test]
fn checksum_mismatch_discards_the_partial_and_preserves_live_data() {
    let t = Temp::new();
    let pkg = fixture(b"correct", "https://example.invalid".into());
    let input = stage(&t.0, pkg, b"corrupt");
    let p = controller(&t.0, pkg);
    fs::create_dir(t.0.join("runtime")).unwrap();
    fs::write(&p.paths().server_jar, b"old").unwrap();
    p.start().unwrap();
    let snap = settled(&p);
    assert_eq!(snap.phase, "failed");
    assert!(snap.error.unwrap().contains("校验和"));
    assert!(!input.exists());
    assert_eq!(fs::read(&p.paths().server_jar).unwrap(), b"old");
}

fn zip_fixture(entries: &[(&str, &[u8], u32)]) -> (Artifact, Vec<u8>) {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for &(name, body, mode) in entries {
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(mode);
        if mode == 0o120777 {
            writer
                .add_symlink(name, std::str::from_utf8(body).unwrap(), options)
                .unwrap();
        } else {
            writer.start_file(name, options).unwrap();
            writer.write_all(body).unwrap();
        }
    }
    let bytes = writer.finish().unwrap().into_inner();
    let pkg = Artifact {
        kind: Kind::Adb,
        id: "platform-tools",
        prefix: Some("platform-tools"),
        essentials: &[("adb", 7)],
        unpacked: entries.iter().map(|(_, b, _)| b.len() as u64).sum(),
        ..fixture(&bytes, "https://example.invalid".into())
    };
    (pkg, bytes)
}

#[test]
fn image_selfcheck_rejects_a_newer_or_unknown_emulator_dependency() {
    let t = Temp::new();
    let image = Artifact {
        essentials: &[],
        ..*CATALOG.iter().find(|p| p.kind == Kind::Image).unwrap()
    };
    for (dep, good) in [
        ("emulator#34.2.16", true),
        ("emulator#100.0", false),
        ("unknown", false),
    ] {
        let properties = format!(
            "Pkg.Revision=14\nAndroidVersion.ApiLevel=34\nSystemImage.TagId=google_apis_playstore\nSystemImage.Abi=arm64-v8a\nAddon.VendorId=google\nPkg.Dependencies={dep}\n"
        );
        fs::write(t.0.join("source.properties"), properties).unwrap();
        assert_eq!(paths::validate_payload(&t.0, image).is_ok(), good);
    }
}

#[test]
fn staged_sdk_payload_is_checked_before_publish_and_on_each_scan() {
    let t = Temp::new();
    let (pkg, bytes) = zip_fixture(&[
        (
            "platform-tools/source.properties",
            b"Pkg.Revision=1\n",
            0o644,
        ),
        ("platform-tools/adb", b"fixture", 0o755),
    ]);
    stage(&t.0, pkg, &bytes);
    let p = controller(&t.0, pkg);
    p.start().unwrap();
    let snap = settled(&p);
    assert_eq!(snap.phase, "ready", "{:?}", snap.error);
    assert_eq!(fs::read(&p.paths().adb).unwrap(), b"fixture");
    fs::set_permissions(&p.paths().adb, fs::Permissions::from_mode(0o644)).unwrap();
    p.recheck().unwrap();
    assert_eq!(p.snapshot().phase, "missing");
    assert!(
        p.snapshot().components[0]
            .issue
            .as_ref()
            .unwrap()
            .contains("不可执行")
    );
    fs::set_permissions(&p.paths().adb, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(p.paths().sdk.join("platform-tools").join(paths::RECEIPT)).unwrap();
    p.recheck().unwrap();
    assert!(
        p.snapshot().components[0]
            .issue
            .as_ref()
            .unwrap()
            .contains("安装不完整")
    );
}

#[test]
fn invalid_archive_or_version_never_replaces_the_installed_directory() {
    let adb: (&str, &[u8], u32) = ("platform-tools/adb", b"fixture", 0o755);
    let props: (&str, &[u8], u32) = (
        "platform-tools/source.properties",
        b"Pkg.Revision=1\n",
        0o644,
    );
    let (mut small, bytes) = zip_fixture(&[adb, props]);
    small.unpacked = 2;
    let cases = [
        ((small, bytes), "展开体积"),
        (
            zip_fixture(&[props, adb, ("platform-tools/../bad", b"x", 0o644)]),
            "文件名",
        ),
        (
            zip_fixture(&[props, adb, ("wrong/extra", b"x", 0o644)]),
            "顶层目录",
        ),
        (
            zip_fixture(&[props, adb, ("platform-tools/link", b"../bad", 0o120777)]),
            "条目类型",
        ),
        (
            zip_fixture(&[
                adb,
                ("platform-tools/source.properties", b"Pkg.Revision=2", 0o644),
            ]),
            "需要版本",
        ),
    ];
    for ((pkg, bytes), diagnostic) in cases {
        let t = Temp::new();
        let dest = t.0.join("runtime/sdk/platform-tools");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("existing"), b"preserve").unwrap();
        stage(&t.0, pkg, &bytes);
        let p = controller(&t.0, pkg);
        p.start().unwrap();
        let snap = settled(&p);
        assert_eq!(snap.phase, "failed");
        assert!(
            snap.error
                .as_deref()
                .is_some_and(|e| e.contains(diagnostic)),
            "{:?}",
            snap.error
        );
        assert!(!partial(&p.owner.worker.stage, pkg).exists());
        assert_eq!(fs::read(dest.join("existing")).unwrap(), b"preserve");
        assert!(!dest.join(paths::RECEIPT).exists());
        assert!(!p.owner.worker.stage.join("bad").exists());
    }
}

#[test]
fn staging_links_and_borrowed_mode_cannot_write_external_files() {
    let t = Temp::new();
    let outside = Temp::new();
    let pkg = fixture(b"sample", "https://example.invalid".into());
    let stage = t.0.join("staging/provision-v1");
    fs::create_dir_all(&stage).unwrap();
    let file = outside.0.join("keep");
    fs::write(&file, b"keep").unwrap();
    symlink(&file, partial(&stage, pkg)).unwrap();
    let p = controller(&t.0, pkg);
    p.start().unwrap();
    assert_eq!(settled(&p).phase, "failed");
    assert_eq!(fs::read(&file).unwrap(), b"keep");
    let part = partial(&stage, pkg);
    fs::remove_file(&part).unwrap();
    fs::hard_link(&file, &part).unwrap();
    p.start().unwrap();
    assert!(settled(&p).error.unwrap().contains("硬链接"));
    assert_eq!(fs::read(&file).unwrap(), b"keep");
    let borrowed = RuntimePaths::from_sdk(outside.0.clone(), file.clone(), true);
    let home = outside.0.join("absent-home");
    let p = Provisioner::configured(home.clone(), borrowed, Box::leak(Box::new([pkg])), true);
    assert!(p.start().is_err());
    assert!(!home.exists());
    assert_eq!(fs::read(file).unwrap(), b"keep");
}

#[test]
fn preparing_components_leaves_image_consent_and_partial_untouched() {
    let temp = Temp::new();
    let core = fixture(b"core", "https://example.invalid".into());
    let image = *CATALOG.iter().find(|p| p.image_spec.is_some()).unwrap();
    let p = Provisioner::configured(
        temp.0.clone(),
        RuntimePaths::managed(&temp.0),
        Box::leak(Box::new([core, image])),
        true,
    );
    stage(&temp.0, core, b"core");
    let image_part = stage(&temp.0, image, b"paused");
    p.start().unwrap();
    let snap = settled(&p);
    assert!(snap.components_ready && !snap.images[0].ready);
    assert_eq!(snap.images[0].downloaded_bytes, 6);
    assert_eq!(fs::read(image_part).unwrap(), b"paused");
    assert!(p.start_image("unlisted").is_err());
}
