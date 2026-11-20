use super::*;
use sha2::Digest;
use std::collections::BTreeMap;
use std::process::Command;

fn hdi(args: &[&str]) {
    let result = Command::new("/usr/bin/hdiutil")
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

struct Mounted(PathBuf);

impl Mounted {
    fn attach(image: &Path, at: &Path) -> Self {
        hdi(&[
            "attach",
            "-nobrowse",
            "-mountpoint",
            at.to_str().unwrap(),
            image.to_str().unwrap(),
        ]);
        Self(at.to_owned())
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        hdi(&["detach", "-force", self.0.to_str().unwrap()]);
    }
}

fn create_disk(base: &Path, name: &str, size: &str) -> PathBuf {
    let image = base.join(format!("{name}.sparseimage"));
    hdi(&[
        "create",
        "-type",
        "SPARSE",
        "-fs",
        "APFS",
        "-size",
        size,
        "-volname",
        name,
        image.to_str().unwrap(),
    ]);
    image
}

fn prepare_prefix(version: &Component, digest: &str, source: &Path, bytes: u64) -> PathBuf {
    let stage = version
        .location
        .parent()
        .unwrap()
        .join(format!(".windroid-install-{}-{digest}", version.id));
    fs::create_dir(&stage).unwrap();
    fs::write(stage.join("owner"), b"windroid-package-install-v1\n").unwrap();
    let mut input = fs::File::open(source).unwrap().take(bytes);
    let mut part =
        fs::File::create_new(stage.join(format!("{}-{digest}.part", version.id))).unwrap();
    assert_eq!(std::io::copy(&mut input, &mut part).unwrap(), bytes);
    stage
}

// 不计读取操作本身可更新的 atime；逐项比较目录、文件对象、属性和完整字节摘要。
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    inode: u64,
    mode: u32,
    modified: (i64, i64),
    changed: (i64, i64),
    digest: Option<String>,
}

fn entries(root: &Path) -> BTreeMap<PathBuf, Entry> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        let m = fs::symlink_metadata(&path).unwrap();
        let digest = if m.is_dir() {
            pending.extend(
                fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
            None
        } else {
            assert!(m.is_file());
            Some(base16ct::lower::encode_string(&sha2::Sha256::digest(
                fs::read(&path).unwrap(),
            )))
        };
        entries.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            Entry {
                inode: m.ino(),
                mode: m.mode(),
                modified: (m.mtime(), m.mtime_nsec()),
                changed: (m.ctime(), m.ctime_nsec()),
                digest,
            },
        );
    }
    entries
}

fn damaged_jar(home: &Path, archives: &Path, byte: u8) -> (Provisioner, Component) {
    let p = Provisioner::new(home);
    let jar = row(&p.snapshot(), "scrcpy-server").clone();
    fs::create_dir_all(&jar.location).unwrap();
    let official = archives.join(jar.source.rsplit('/').next().unwrap());
    let mut bytes = fs::read(&official).unwrap();
    let digest = base16ct::lower::encode_string(&sha2::Sha256::digest(&bytes));
    bytes[1234] ^= byte;
    fs::write(jar.location.join("scrcpy-server.jar"), bytes).unwrap();
    prepare_prefix(&jar, &digest, &official, 32_768);
    p.recheck().unwrap();
    let observed = row(&p.snapshot(), jar.id).clone();
    assert_eq!(observed.integrity, "damaged");
    assert_eq!(observed.downloaded_bytes, 32_768);
    (p, observed)
}

const WORKER: &str = "WINDROID_QA_PUBLICATION_CHILD";

fn repair_worker(home: &Path) {
    let p = Provisioner::new(home);
    let emulator = row(&p.snapshot(), "emulator").clone();
    p.repair_confirmed(
        emulator.id,
        &emulator.location,
        emulator.repair_token.as_deref().unwrap(),
    )
    .unwrap();
    completed(&p);
}

#[test]
#[ignore = "needs official archives and creates, swaps and remounts two disposable APFS disk images"]
fn replacement_rejects_old_confirmation_and_original_volume_recovers() {
    let base = env_dir("WINDROID_QA_ROOT").join("publish-volume-repair");
    let home = base.join("home");
    if std::env::var_os(WORKER).is_some() {
        repair_worker(&home);
        return;
    }
    fs::create_dir(&base).unwrap();
    fs::create_dir(&home).unwrap();
    let archives = env_dir("WINDROID_QA_ARCHIVES");
    let slot = base.join("mount");
    let a = create_disk(&base, "WindroidPublishA", "4g");
    let b = create_disk(&base, "WindroidPublishB", "128m");
    let settings = StorageLocations {
        runtime_root: slot.join("runtime"),
        ..StorageLocations::defaults(&home)
    };
    fs::write(
        home.join(windroid_locations::SETTINGS),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();

    let mount = Mounted::attach(&a, &slot);
    let (controller, original) = damaged_jar(&home, &archives, 1);
    let on_a = fs::metadata(&original.location).unwrap();
    let old_confirm = original.repair_token.as_deref().unwrap();
    drop(mount);
    let mount = Mounted::attach(&b, &slot);
    let (_reader, replacement) = damaged_jar(&home, &archives, 2);
    let on_b = fs::metadata(&replacement.location).unwrap();
    assert_eq!(original.location, replacement.location);
    assert_ne!(original.repair_token, replacement.repair_token);
    let before = entries(&settings.runtime_root);
    let error = controller
        .repair_confirmed(original.id, &original.location, old_confirm)
        .unwrap_err();
    assert_eq!(before, entries(&settings.runtime_root));
    println!(
        "旧 JAR 确认被拒绝：{error}。A/B 版本目录 (dev,ino) 为 {:?} / {:?}，替换卷 {} 项属性和内容保持一致。",
        (on_a.dev(), on_a.ino()),
        (on_b.dev(), on_b.ino()),
        before.len()
    );
    drop(mount);

    let mount = Mounted::attach(&a, &slot);
    // 正常修复入口读取完整的官方前缀。原版本仅含名称可识别、内容损坏的入口。
    let p = Provisioner::new(&home);
    let emulator = row(&p.snapshot(), "emulator").clone();
    fs::create_dir_all(&emulator.location).unwrap();
    fs::write(emulator.location.join("emulator"), b"damaged").unwrap();
    let zip = archives.join(emulator.source.rsplit('/').next().unwrap());
    let mut file = fs::File::open(&zip).unwrap();
    let mut hash = sha1::Sha1::new();
    let mut block = [0; 65_536];
    loop {
        let n = file.read(&mut block).unwrap();
        if n == 0 {
            break;
        }
        hash.update(&block[..n]);
    }
    let stage = prepare_prefix(
        &emulator,
        &base16ct::lower::encode_string(&hash.finalize()),
        &zip,
        emulator.size_bytes,
    );
    let journal = stage.join("publish.json");
    let backup = stage.join("previous");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "volume_repair::replacement_rejects_old_confirmation_and_original_volume_recovers",
            "--ignored",
            "--nocapture",
        ])
        .env(WORKER, "1")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if journal.is_file()
            && backup.is_dir()
            && emulator.location.join("source.properties").is_file()
        {
            child.kill().unwrap();
            child.wait().unwrap();
            break;
        }
        assert!(child.try_wait().unwrap().is_none(), "发布已提前退出");
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("未观察到新版本换入");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(fs::read(backup.join("emulator")).unwrap(), b"damaged");
    let interrupted = fs::read(&journal).unwrap();
    let old_dev = fs::metadata(&journal).unwrap().dev();
    p.recheck().unwrap();
    let journal_token = row(&p.snapshot(), emulator.id)
        .repair_token
        .clone()
        .unwrap();
    drop(mount);
    // B 占据之前的设备号，再把 A 重新挂到最初的绝对路径。
    let parking = Mounted::attach(&b, &base.join("parking"));
    let mount = Mounted::attach(&a, &slot);
    let new_dev = fs::metadata(&journal).unwrap().dev();
    assert_ne!(old_dev, new_dev, "验收需要重挂后实际设备号改变");
    assert_eq!(fs::read(&journal).unwrap(), interrupted);
    let reopened = Provisioner::new(&home);
    let pending = row(&reopened.snapshot(), emulator.id).clone();
    assert_eq!(pending.integrity, "damaged");
    assert_eq!(pending.repair_token.as_ref(), Some(&journal_token));
    reopened
        .repair_confirmed(emulator.id, &pending.location, &journal_token)
        .unwrap();
    assert!(row(&completed(&reopened), emulator.id).ready);
    assert!(!backup.exists() && !journal.exists());
    reopened.verify(emulator.id).unwrap();
    assert_eq!(
        row(&completed(&reopened), emulator.id).integrity,
        "verified"
    );
    println!(
        "官方 Emulator {} 安装中断后，卷 A 从 dev {old_dev} 重挂为 {new_dev}，发布恢复并通过全量核验。",
        emulator.version
    );
    drop(mount);
    drop(parking);
    fs::remove_dir_all(base).unwrap();
}
