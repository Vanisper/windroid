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
    let sha = base16ct::lower::encode_string(&Sha256::digest(bytes));
    Artifact {
        kind: Kind::Server,
        id: "scrcpy-server",
        name: "fixture",
        image_spec: None,
        version: "5.0.1",
        url: Box::leak(url.into_boxed_str()),
        bytes: bytes.len() as u64,
        manifest: Box::leak(
            format!("# {sha}\nf\t{}\t-\t{sha}\tscrcpy-server.jar\n", bytes.len()).into_boxed_str(),
        ),
        digest: Digest::Sha256(Box::leak(sha.into_boxed_str())),
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
#[derive(Clone, Copy)]
enum FirstResponse {
    Complete,
    Stall,
    Disconnect,
}

struct Http {
    url: String,
    ranges: Arc<Mutex<Vec<usize>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Http {
    fn new(body: Vec<u8>, first: FirstResponse, ignore_range: bool) -> Self {
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
                if reqs.lock().unwrap().len() == 1 && !matches!(first, FirstResponse::Complete) {
                    stream.write_all(&body[start..start + 32_768]).unwrap();
                    if matches!(first, FirstResponse::Disconnect) {
                        continue;
                    }
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
fn new_roots_are_independent_and_versioned() {
    let t = Temp::new();
    let paths = RuntimePaths::discover(&t.0);
    assert_eq!(paths.adb, t.0.join("runtime/platform-tools/37.0.1/adb"));
    assert_eq!(
        paths.server_jar,
        t.0.join("runtime/scrcpy-server/5.0.1/scrcpy-server.jar")
    );
    assert_eq!(paths.images, t.0.join("system-images"));
    assert_eq!(fs::read_dir(&t.0).unwrap().count(), 0);
}

#[test]
fn pause_restart_and_resume_from_exact_prefix() {
    let temp = Temp::new();
    let bytes: Vec<_> = (0..131_072).map(|i| (i % 251) as u8).collect();
    let http = Http::new(bytes.clone(), FirstResponse::Stall, false);
    let pkg = fixture(&bytes, http.url.clone());
    let p = controller(&temp.0, pkg);
    let stage = archive::stage_path(&p.paths().destination(pkg), pkg);
    let part = partial(&stage, pkg);
    assert_eq!(p.snapshot().phase, "missing");
    assert!(http.ranges.lock().unwrap().is_empty());
    p.start().unwrap();
    p.start().unwrap();
    wait_for(|| fs::metadata(&part).is_ok_and(|m| m.len() > 0));
    assert!(p.recheck().is_err());
    let second = controller(&temp.0, pkg);
    second.start().unwrap();
    assert!(settled(&second).error.is_some());
    p.pause().unwrap();
    assert_eq!(settled(&p).phase, "paused");
    assert_eq!(
        fs::read(stage.join("owner")).unwrap(),
        b"windroid-package-install-v1\n"
    );
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
    let dest = RuntimePaths::discover(home).destination(pkg);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let (stage, _lock) = archive::workspace(&dest, pkg).unwrap();
    let file = partial(&stage, pkg);
    fs::write(&file, bytes).unwrap();
    file
}

#[test]
fn ignored_range_restarts_once_without_concatenating_bodies() {
    let t = Temp::new();
    let bytes = vec![17; 65536];
    let http = Http::new(bytes.clone(), FirstResponse::Complete, true);
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
    fs::create_dir_all(p.paths().server_jar.parent().unwrap()).unwrap();
    fs::write(&p.paths().server_jar, b"old").unwrap();
    p.repair(pkg.id).unwrap();
    let snap = settled(&p);
    assert_eq!(snap.phase, "failed");
    assert!(snap.error.unwrap().contains("校验和"));
    assert!(!input.exists());
    assert_eq!(fs::read(&p.paths().server_jar).unwrap(), b"old");
}

const HOST_ARM64: &[u8] = b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01fixture";
const ARM_EXECUTABLE: &[(&str, u64)] = &[("adb", HOST_ARM64.len() as u64)];
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
    let mut rows = std::collections::BTreeMap::new();
    for &(name, body, mode) in entries {
        let rel = name.split_once('/').unwrap().1;
        rows.insert(
            rel,
            format!(
                "f\t{}\t{}\t{}",
                body.len(),
                if mode & 0o111 != 0 { "x" } else { "-" },
                base16ct::lower::encode_string(&Sha256::digest(body))
            ),
        );
    }
    let mut inventory = "# fixture\n".to_string();
    for (path, row) in rows {
        inventory.push_str(&format!("{row}\t{path}\n"));
    }
    let pkg = Artifact {
        manifest: Box::leak(inventory.into_boxed_str()),
        version: "1",
        kind: Kind::Adb,
        id: "platform-tools",
        prefix: Some("platform-tools"),
        essentials: ARM_EXECUTABLE,
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
        ("platform-tools/adb", HOST_ARM64, 0o755),
    ]);
    stage(&t.0, pkg, &bytes);
    let p = controller(&t.0, pkg);
    p.start().unwrap();
    let snap = settled(&p);
    assert_eq!(snap.phase, "ready", "{:?}", snap.error);
    let adb = p.paths().destination(pkg).join("adb");
    assert_eq!(fs::read(&adb).unwrap(), HOST_ARM64);
    fs::set_permissions(&adb, fs::Permissions::from_mode(0o644)).unwrap();
    p.recheck().unwrap();
    assert_eq!(p.snapshot().components[0].integrity, "damaged");
    assert!(
        p.snapshot().components[0]
            .issue
            .as_ref()
            .unwrap()
            .contains("不可执行")
    );
    fs::set_permissions(&adb, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_file(&adb).unwrap();
    p.recheck().unwrap();
    assert!(
        p.snapshot().components[0]
            .issue
            .as_ref()
            .unwrap()
            .contains("缺少文件")
    );
}

#[test]
fn invalid_archive_or_version_never_replaces_the_installed_directory() {
    let adb: (&str, &[u8], u32) = ("platform-tools/adb", HOST_ARM64, 0o755);
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
        let dest = t.0.join("runtime/unrelated-release");
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
        assert_eq!(fs::read(dest.join("existing")).unwrap(), b"preserve");
        assert!(!p.paths().destination(pkg).join("bad").exists());
    }
}

#[test]
fn staging_links_cannot_write_external_files() {
    let t = Temp::new();
    let outside = Temp::new();
    let pkg = fixture(b"sample", "https://example.invalid".into());
    let part = stage(&t.0, pkg, b"");
    fs::remove_file(&part).unwrap();
    let file = outside.0.join("keep");
    fs::write(&file, b"keep").unwrap();
    symlink(&file, &part).unwrap();
    let p = controller(&t.0, pkg);
    p.start().unwrap();
    assert_eq!(settled(&p).phase, "failed");
    assert_eq!(fs::read(&file).unwrap(), b"keep");
    fs::remove_file(&part).unwrap();
    fs::hard_link(&file, &part).unwrap();
    p.start().unwrap();
    assert!(settled(&p).error.unwrap().contains("硬链接"));
    assert_eq!(fs::read(file).unwrap(), b"keep");
}

#[test]
fn a_second_publisher_reuses_the_verified_destination() {
    use std::os::unix::fs::MetadataExt;
    let home = Temp::new();
    let pkg = fixture(b"server", "https://example.invalid".into());
    stage(&home.0, pkg, b"server");
    let p = controller(&home.0, pkg);
    p.start().unwrap();
    let result = settled(&p);
    assert!(
        result.components_ready,
        "{:?} {:?}",
        result.error, result.components[0].issue
    );
    let dest = p.paths().destination(pkg);
    let first = fs::metadata(&dest).unwrap().ino();
    let second = controller(&home.0, pkg);
    second.start().unwrap();
    assert_eq!(fs::metadata(dest).unwrap().ino(), first);
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

#[test]
fn publishes_verified_archives_in_configured_resource_directory() {
    let home = Temp::new();
    let resources = Temp::new();
    let settings = windroid_locations::StorageLocations {
        runtime_root: resources.0.join("shared"),
        image_root: home.0.join("system-images"),
    };
    fs::write(
        home.0.join(windroid_locations::SETTINGS),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let (pkg, bytes) = zip_fixture(&[
        (
            "platform-tools/source.properties",
            b"Pkg.Revision=1\n",
            0o644,
        ),
        ("platform-tools/adb", HOST_ARM64, 0o755),
    ]);
    stage(&home.0, pkg, &bytes);
    let p = controller(&home.0, pkg);
    p.start().unwrap();
    let snap = settled(&p);
    assert!(snap.components_ready, "{:?}", snap.error);
    assert_eq!(
        p.paths().destination(pkg).join("adb"),
        settings.runtime_root.join("platform-tools/1/adb")
    );
    assert!(!home.0.join("runtime").exists());
}

#[test]
fn import_checks_the_confirmed_copy_and_preserves_the_download_prefix_after_failure() {
    let home = Temp::new();
    let original = Temp::new();
    let zip = original.0.join("user-chosen.zip");
    let image = *CATALOG.iter().find(|p| p.id == "system-image").unwrap();
    let (mut pkg, bytes) = zip_fixture(&[(
        "arm64-v8a/source.properties",
        b"Pkg.Revision=14\nPkg.Dependencies=emulator#34.2.16\nAndroidVersion.ApiLevel=34\nSystemImage.Abi=arm64-v8a\nSystemImage.TagId=google_apis_playstore\nAddon.VendorId=google",
        0o644,
    )]);
    pkg = Artifact {
        essentials: &[],
        kind: Kind::Image,
        id: image.id,
        version: image.version,
        prefix: image.prefix,
        image_spec: image.image_spec,
        ..pkg
    };
    let p = controller(&home.0, pkg);
    fs::write(&zip, &bytes[..50]).unwrap();
    assert!(p.inspect_image_zip(&zip).unwrap_err().contains("大小"));
    fs::write(&zip, &bytes).unwrap();
    let preview = p.inspect_image_zip(&zip).unwrap();
    assert_eq!(preview.image.unwrap().api, 34);
    assert!(!preview.ready && !preview.location.exists());
    assert!(!archive::stage_path(&preview.location, pkg).exists());
    let prefix = stage(&home.0, pkg, &bytes[..50]);

    let mut changed = bytes.clone();
    changed[0] ^= 1;
    fs::write(&zip, &changed).unwrap();
    assert!(p.inspect_image_zip(&zip).unwrap_err().contains("校验和"));
    p.import_image(pkg.id, zip.clone()).unwrap();
    assert!(settled(&p).error.unwrap().contains("校验和"));
    assert!(!preview.location.exists());
    assert_eq!(fs::read(&prefix).unwrap(), bytes[..50]);

    fs::write(&zip, &bytes).unwrap();
    p.import_image(pkg.id, zip.clone()).unwrap();
    let snap = settled(&p);
    assert!(snap.images[0].ready, "{:?}", snap.error);
    assert!(!home.0.join("instances").exists());
    assert_eq!(fs::read(zip).unwrap(), bytes);
    assert!(!prefix.exists());
}

#[test]
fn failed_transfer_reopens_without_network_and_retries_only_on_request() {
    let home = Temp::new();
    let bytes = vec![72; 131_072];
    let http = Http::new(bytes.clone(), FirstResponse::Disconnect, false);
    let pkg = fixture(&bytes, http.url.clone());
    let p = controller(&home.0, pkg);
    p.start().unwrap();
    let failed = settled(&p);
    assert_eq!(failed.phase, "failed");
    assert!(failed.error.unwrap().contains("下载失败"));
    assert_eq!(failed.downloaded_bytes, 32_768);
    drop(p);

    let reopened = controller(&home.0, pkg);
    assert_eq!(reopened.snapshot().phase, "paused");
    assert_eq!(*http.ranges.lock().unwrap(), [0]);
    reopened.start().unwrap();
    assert!(settled(&reopened).components_ready);
    assert_eq!(*http.ranges.lock().unwrap(), [0, 32_768]);
    assert_eq!(fs::read(&reopened.paths().server_jar).unwrap(), bytes);
}

#[test]
fn independent_image_move_preserves_the_active_component_download() {
    let temp = Temp::new();
    let home = temp.0.join("home");
    let bytes = vec![0x74; 131_072];
    let socket = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let pkg = fixture(
        &bytes,
        format!("http://{}/jar", socket.local_addr().unwrap()),
    );
    let (release, wait) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        let payload = &bytes;
        scope.spawn(move || {
            let (mut conn, _) = socket.accept().unwrap();
            let mut req = BufReader::new(&mut conn);
            loop {
                let mut line = String::new();
                req.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )
            .unwrap();
            conn.write_all(&payload[..32_768]).unwrap();
            wait.recv_timeout(Duration::from_secs(10)).unwrap();
            conn.write_all(&payload[32_768..]).unwrap();
        });
        let p = controller(&home, pkg);
        p.start().unwrap();
        let part = partial(&archive::stage_path(&p.paths().destination(pkg), pkg), pkg);
        wait_for(|| fs::metadata(&part).is_ok_and(|m| m.len() > 0));
        let settings = crate::storage::StorageLocations {
            runtime_root: home.join("runtime"),
            image_root: temp.0.join("detached"),
        };
        fs::write(
            home.join(windroid_locations::SETTINGS),
            serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();
        p.refresh_locations();
        assert_eq!(p.snapshot().phase, "downloading");
        release.send(()).unwrap();
        let result = settled(&p);
        assert_eq!(result.phase, "ready", "{:?}", result.error);
        assert_eq!(fs::read(p.paths().server_jar).unwrap(), bytes);
    });
}

#[test]
fn conflicts_are_preserved_and_damaged_jar_needs_explicit_repair() {
    let t = Temp::new();
    let pkg = fixture(b"official", "https://example.invalid".into());
    let p = controller(&t.0, pkg);
    let dest = p.paths().destination(pkg);
    let part = stage(&t.0, pkg, b"official");
    fs::write(&dest, b"foreign file").unwrap();
    p.start().unwrap();
    assert_eq!(settled(&p).components[0].integrity, "conflict");
    assert!(p.repair(pkg.id).is_err());
    assert_eq!(fs::read(&dest).unwrap(), b"foreign file");
    fs::remove_file(&dest).unwrap();
    p.start().unwrap();
    let result = settled(&p);
    assert!(
        result.components_ready,
        "{:?} {:?}",
        result.error, result.components[0].issue
    );
    let jar = dest.join("scrcpy-server.jar");
    let mtime = fs::metadata(&jar).unwrap().modified().unwrap();
    fs::write(&jar, b"Official").unwrap();
    fs::File::options()
        .write(true)
        .open(&jar)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    stage(&t.0, pkg, b"official");
    p.start().unwrap();
    assert_eq!(settled(&p).components[0].integrity, "damaged");
    assert_eq!(fs::read(&jar).unwrap(), b"Official");
    assert!(part.exists());
    let extra = dest.join("user.txt");
    fs::write(&extra, b"keep").unwrap();
    assert!(p.repair(pkg.id).is_err());
    assert_eq!(fs::read(&extra).unwrap(), b"keep");
    fs::remove_file(extra).unwrap();
    p.repair(pkg.id).unwrap();
    let result = settled(&p);
    assert!(
        result.components_ready,
        "{:?} {:?}",
        result.error, result.components[0].issue
    );
    assert_eq!(fs::read(&jar).unwrap(), b"official");
}

#[test]
fn interrupted_publication_restores_or_commits_only_identified_objects() {
    let pkg = fixture(b"official", "https://example.invalid".into());
    for step in 0..5 {
        let t = Temp::new();
        stage(&t.0, pkg, b"official");
        let p = controller(&t.0, pkg);
        let dest = p.paths().destination(pkg);
        let work = archive::stage_path(&dest, pkg);
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("scrcpy-server.jar"), b"old").unwrap();
        let scratch = work.join("unpack");
        fs::create_dir(&scratch).unwrap();
        fs::write(scratch.join("scrcpy-server.jar"), b"official").unwrap();
        let identity = |path: &Path| crate::storage::StableObject::at(path).unwrap();
        let old = (step != 4).then(|| identity(&dest));
        let record =
            serde_json::json!({"stage":identity(&work),"old":old,"new":identity(&scratch)});
        fs::write(
            work.join("publish.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        if step == 4 {
            fs::remove_dir_all(&dest).unwrap();
        } else if step >= 1 {
            fs::rename(&dest, work.join("previous")).unwrap();
        }
        if step >= 2 {
            fs::rename(&scratch, &dest).unwrap();
            if step >= 3 {
                fs::write(dest.join("scrcpy-server.jar"), b"damaged").unwrap();
            }
        }
        p.recheck().unwrap();
        assert_eq!(p.snapshot().components[0].integrity, "damaged");
        p.repair_at(pkg.id, &dest).unwrap();
        let finished = settled(&p);
        assert!(finished.components_ready, "{step} {:?}", finished.error);
        assert!(!work.join("previous").exists() && !work.join("publish.json").exists());
        assert_eq!(
            fs::read(dest.join("scrcpy-server.jar")).unwrap(),
            b"official"
        );
    }
}

#[test]
fn space_and_disconnected_volumes_are_checked_before_transferring() {
    let home = Temp::new();
    let http = Http::new(vec![1; 8], FirstResponse::Complete, false);
    let pkg = Artifact {
        unpacked: u64::MAX,
        ..fixture(&[1; 8], http.url.clone())
    };
    let p = controller(&home.0, pkg);
    p.start().unwrap();
    let failed = settled(&p);
    assert!(failed.error.unwrap().contains("空间不足"));
    assert!(http.ranges.lock().unwrap().is_empty() && !p.paths().runtime_root.exists());
    let offline = PathBuf::from(format!("/Volumes/Windroid-offline-{}", std::process::id()));
    assert!(!offline.exists());
    let locations = StorageLocations {
        runtime_root: offline.clone(),
        ..StorageLocations::defaults(&home.0)
    };
    fs::write(
        home.0.join(windroid_locations::SETTINGS),
        serde_json::to_vec(&locations).unwrap(),
    )
    .unwrap();
    p.recheck().unwrap();
    let row = &p.snapshot().components[0];
    assert!(!row.writable && row.issue.as_deref().unwrap().contains("挂载"));
    p.start().unwrap();
    assert!(http.ranges.lock().unwrap().is_empty() && !offline.exists());
}

#[test]
fn candidate_download_progress_and_prefix_stay_with_the_selected_volume() {
    let home = Temp::new();
    let other = Temp::new();
    let official = CATALOG.iter().find(|p| p.id == "system-image").unwrap();
    let prefix = official.prefix.unwrap();
    let properties = format!(
        "Pkg.Revision=14\nPkg.Dependencies=emulator#34.2.16\nAndroidVersion.ApiLevel=34\nSystemImage.Abi={prefix}\nSystemImage.TagId=google_apis_playstore\nAddon.VendorId=google"
    );
    let (fixture, bytes) = zip_fixture(&[
        (
            &format!("{prefix}/source.properties"),
            properties.as_bytes(),
            0o644,
        ),
        (&format!("{prefix}/system.img"), &vec![0x71; 131_072], 0o644),
    ]);
    let http = Http::new(bytes.clone(), FirstResponse::Stall, false);
    let image = Artifact {
        kind: Kind::Image,
        id: official.id,
        version: official.version,
        prefix: official.prefix,
        image_spec: official.image_spec,
        essentials: &[],
        url: Box::leak(http.url.clone().into_boxed_str()),
        ..fixture
    };
    let p = controller(&home.0, image);
    let settings = StorageLocations {
        image_root: other.0.join("candidate"),
        ..StorageLocations::defaults(&home.0)
    };
    let dest = RuntimePaths::for_locations(&settings).destination(image);
    let part = partial(&archive::stage_path(&dest, image), image);
    p.start_image_at(image.id, &settings).unwrap();
    wait_for(|| fs::metadata(&part).is_ok_and(|m| m.len() > 0));
    assert_eq!(p.snapshot().candidate_target.as_ref(), Some(&dest));
    p.pause().unwrap();
    let paused = settled(&p);
    assert_eq!(paused.phase, "paused");
    assert!(!paused.images[0].ready && paused.images[0].downloaded_bytes == 0);
    let retained = fs::metadata(&part).unwrap().len();
    drop(p);
    let reopened = controller(&home.0, image);
    assert_eq!(reopened.snapshot().images[0].downloaded_bytes, 0);
    reopened.start_image_at(image.id, &settings).unwrap();
    let done = settled(&reopened);
    assert_eq!(done.candidate_target.as_ref(), Some(&dest));
    assert_eq!(done.candidate_downloaded_bytes, Some(bytes.len() as u64));
    assert!(!done.images[0].ready && !done.images[0].location.exists());
    assert_eq!(*http.ranges.lock().unwrap(), [0, retained as usize]);
    assert!(dest.join("system.img").is_file());
    let reused = controller(&home.0, image);
    reused.start_image_at(image.id, &settings).unwrap();
    let ready = reused.snapshot();
    assert_eq!(ready.phase, "ready");
    assert_eq!(ready.candidate_target.as_ref(), Some(&dest));
    assert_eq!(ready.candidate_downloaded_bytes, Some(bytes.len() as u64));
    assert!(!ready.images[0].ready);
    assert_eq!(http.ranges.lock().unwrap().len(), 2);
    assert!(!home.0.join(windroid_locations::SETTINGS).exists());
}

#[test]
fn replacing_a_damaged_version_invalidates_its_original_confirmation() {
    let t = Temp::new();
    let pkg = fixture(b"official", "https://example.invalid".into());
    let p = controller(&t.0, pkg);
    let dest = p.paths().destination(pkg);
    let jar = dest.join("scrcpy-server.jar");
    fs::create_dir_all(&dest).unwrap();
    fs::write(&jar, b"Damaged!").unwrap();
    p.recheck().unwrap();
    let consent = p.snapshot().components[0].repair_token.clone().unwrap();
    let saved = t.0.join("first-version");
    fs::rename(&dest, &saved).unwrap();
    fs::create_dir(&dest).unwrap();
    fs::write(&jar, b"Damaged!").unwrap();
    let part = stage(&t.0, pkg, b"official");
    assert!(p.repair_confirmed(pkg.id, &dest, &consent).is_err());
    assert_eq!(fs::read(&part).unwrap(), b"official");
    assert_eq!(fs::read(&jar).unwrap(), b"Damaged!");
    assert_eq!(
        fs::read(saved.join("scrcpy-server.jar")).unwrap(),
        b"Damaged!"
    );
}

#[test]
fn copied_publication_record_cannot_recover_another_stage() {
    let t = Temp::new();
    let pkg = fixture(b"official", "https://example.invalid".into());
    let p = controller(&t.0, pkg);
    let dest = p.paths().destination(pkg);
    stage(&t.0, pkg, b"official");
    let work = archive::stage_path(&dest, pkg);
    let first = t.0.join("original-stage");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&dest).unwrap();
    fs::write(dest.join("scrcpy-server.jar"), b"official").unwrap();
    let journal = work.join("publish.json");
    let record = serde_json::json!({"stage":crate::storage::StableObject::at(&first).unwrap(),
        "old":null,"new":crate::storage::StableObject::at(&dest).unwrap()});
    let bytes = serde_json::to_vec(&record).unwrap();
    fs::write(&journal, &bytes).unwrap();
    assert!(archive::recover(&t.0, &work, &dest, pkg, &|| Ok(())).is_err());
    assert_eq!(fs::read(journal).unwrap(), bytes);
    assert_eq!(
        fs::read(dest.join("scrcpy-server.jar")).unwrap(),
        b"official"
    );
}

#[test]
fn read_only_verification_checks_content_without_publishing_a_receipt() {
    let temp = Temp::new();
    let path = temp.0.join("version");
    fs::create_dir(&path).unwrap();
    let bytes = b"verified payload";
    let pkg = fixture(bytes, String::new());
    fs::write(path.join("scrcpy-server.jar"), bytes).unwrap();
    assert_eq!(
        manifest::check(&temp.0, &path, pkg, Verification::ReadOnly, || Ok(())).unwrap(),
        "verified"
    );
    fs::write(path.join("scrcpy-server.jar"), vec![0; bytes.len()]).unwrap();
    assert!(manifest::check(&temp.0, &path, pkg, Verification::ReadOnly, || Ok(())).is_err());
    assert!(!temp.0.join("cache").exists());
}
