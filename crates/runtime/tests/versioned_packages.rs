//! 真实发布包专项：只写调用者明确提供的两个未使用验收子目录。
//! WINDROID_QA_ROOT 为工作区父目录，WINDROID_QA_VOLUME 为另一挂载卷的空父目录。
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
use runtime::provision::{Component, Preparation, Provisioner};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windroid_locations::StorageLocations;

fn env_dir(key: &str) -> PathBuf {
    std::env::var_os(key).map(PathBuf::from).expect(key)
}
fn until(p: &Provisioner, predicate: impl Fn(&Preparation) -> bool) -> Preparation {
    let start = Instant::now();
    loop {
        let s = p.snapshot();
        if predicate(&s) {
            return s;
        }
        assert!(
            start.elapsed() < Duration::from_secs(1200),
            "等待超时：{} {:?}",
            s.phase,
            s.error
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn completed(p: &Provisioner) -> Preparation {
    let s = until(p, |s| {
        !matches!(s.phase, "downloading" | "installing" | "verifying")
    });
    assert!(s.error.is_none(), "{} {:?}", s.phase, s.error);
    s
}
fn row<'a>(s: &'a Preparation, id: &str) -> &'a Component {
    s.components
        .iter()
        .chain(&s.images)
        .find(|r| r.id == id)
        .unwrap()
}
fn damage(file: &Path) {
    let mut writer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(file)
        .unwrap();
    let modified = writer.metadata().unwrap().modified().unwrap();
    let offset = writer.metadata().unwrap().len() / 2;
    writer.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0];
    writer.read_exact(&mut byte).unwrap();
    byte[0] ^= 1;
    writer.seek(SeekFrom::Start(offset)).unwrap();
    writer.write_all(&byte).unwrap();
    writer.set_modified(modified).unwrap();
}

#[test]
#[ignore = "downloads and repairs official multi-GB packages on a dedicated mounted volume"]
fn official_target_volume_pause_resume_and_repair() {
    let root = env_dir("WINDROID_QA_ROOT").join("provision-real-home");
    let external = env_dir("WINDROID_QA_VOLUME").join("provision-real-packages");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&external).unwrap();
    assert_ne!(
        fs::metadata(&root).unwrap().dev(),
        fs::metadata(&external).unwrap().dev()
    );
    let settings = StorageLocations {
        runtime_root: external.join("tools"),
        image_root: external.join("images"),
    };
    let settings_path = root.join(windroid_locations::SETTINGS);
    fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();

    let p = Provisioner::new(&root);
    p.start().unwrap();
    assert!(completed(&p).components_ready);
    p.start_image("system-image").unwrap();
    let active = until(&p, |s| {
        row(s, "system-image").downloaded_bytes >= 2 * 1024 * 1024 || s.phase == "failed"
    });
    assert_eq!(active.phase, "downloading", "{:?}", active.error);
    p.pause().unwrap();
    let paused = completed(&p);
    assert_eq!(paused.phase, "paused");
    let prefix = row(&paused, "system-image").downloaded_bytes;
    println!("target-volume official API34 prefix {prefix} bytes");
    assert!(prefix > 0 && prefix < row(&paused, "system-image").size_bytes);
    assert!(!root.join("staging").exists());
    drop(p);
    let p = Provisioner::new(&root);
    assert_eq!(row(&p.snapshot(), "system-image").downloaded_bytes, prefix);
    let other = StorageLocations {
        image_root: root.join("elsewhere"),
        ..settings.clone()
    };
    fs::write(&settings_path, serde_json::to_vec(&other).unwrap()).unwrap();
    p.recheck().unwrap();
    assert_eq!(row(&p.snapshot(), "system-image").downloaded_bytes, 0);
    fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
    p.recheck().unwrap();
    assert_eq!(row(&p.snapshot(), "system-image").downloaded_bytes, prefix);
    p.start_image("system-image").unwrap();
    assert!(row(&completed(&p), "system-image").ready);

    for (id, file) in [
        ("emulator", "emulator"),
        ("platform-tools", "adb"),
        ("build-tools", "aapt"),
        ("scrcpy-server", "scrcpy-server.jar"),
        ("system-image", "system.img"),
    ] {
        let before = p.snapshot();
        let resource = row(&before, id);
        let location = resource.location.clone();
        let inode = fs::metadata(&location).unwrap().ino();
        damage(&location.join(file));
        p.recheck().unwrap();
        assert_eq!(
            row(&p.snapshot(), id).integrity,
            "damaged",
            "{id}: {:?}",
            row(&p.snapshot(), id).issue
        );
        p.start().unwrap();
        assert_eq!(fs::metadata(&location).unwrap().ino(), inode);
        let t = Instant::now();
        p.repair_at(id, &location).unwrap();
        let fixed = completed(&p);
        assert!(row(&fixed, id).ready, "{id}: {:?}", row(&fixed, id).issue);
        assert_ne!(fs::metadata(&location).unwrap().ino(), inode);
        println!(
            "confirmed official repair {id}, {} archive bytes, {:.2}s",
            resource.size_bytes,
            t.elapsed().as_secs_f64()
        );
    }
    // 第二份工作区不需要标记或写入共享版本目录。
    let another = env_dir("WINDROID_QA_ROOT").join("reuse-real-home");
    fs::create_dir(&another).unwrap();
    fs::write(
        another.join(windroid_locations::SETTINGS),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let shared = Provisioner::new(&another);
    assert!(shared.snapshot().components_ready && row(&shared.snapshot(), "system-image").ready);
}

#[test]
#[ignore = "needs a mounted writable test volume smaller than the pinned Emulator"]
fn official_download_checks_small_target_volume_before_writing() {
    let home = env_dir("WINDROID_QA_ROOT").join("small-volume-home");
    fs::create_dir(&home).unwrap();
    let settings = StorageLocations {
        runtime_root: env_dir("WINDROID_QA_VOLUME").join("small-target"),
        ..StorageLocations::defaults(&home)
    };
    assert!(!settings.runtime_root.exists());
    fs::write(
        home.join(windroid_locations::SETTINGS),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let p = Provisioner::new(home);
    let snap = p.snapshot();
    let emulator = row(&snap, "emulator");
    let available = emulator.available_bytes.unwrap();
    assert!(available < emulator.required_bytes && emulator.writable);
    p.start().unwrap();
    let failure = until(&p, |s| s.phase == "failed");
    assert!(failure.error.unwrap().contains("空间不足"));
    assert!(!settings.runtime_root.exists());
    println!(
        "small target volume available {available}, required {} bytes",
        emulator.required_bytes
    );
}

#[test]
#[ignore = "needs an actual read-only mounted volume with an official scrcpy-server folder"]
fn officially_verified_read_only_volume_is_reused_in_place() {
    let home = env_dir("WINDROID_QA_ROOT").join("readonly-volume-home");
    fs::create_dir(&home).unwrap();
    let settings = StorageLocations {
        runtime_root: env_dir("WINDROID_QA_VOLUME").join("readonly-tools"),
        ..StorageLocations::defaults(&home)
    };
    fs::write(
        home.join(windroid_locations::SETTINGS),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let p = Provisioner::new(home);
    let snapshot = p.snapshot();
    let jar = row(&snapshot, "scrcpy-server");
    assert!(jar.ready && !jar.writable);
    let id = fs::metadata(&jar.location).unwrap().ino();
    p.verify(jar.id).unwrap();
    let checked = completed(&p);
    assert_eq!(row(&checked, jar.id).integrity, "verified");
    assert_eq!(fs::metadata(&jar.location).unwrap().ino(), id);
    assert_eq!(
        fs::read_dir(jar.location.parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    println!(
        "official JAR on read-only volume: {} / {} bytes",
        jar.location.display(),
        jar.size_bytes
    );
}

#[test]
#[ignore = "creates, force-ejects and reattaches its own disposable disk image during an official download"]
fn eject_target_during_official_download_and_reopen() {
    use std::process::Command;
    let base = env_dir("WINDROID_QA_ROOT");
    let home = base.join("ejection-home");
    fs::create_dir(&home).unwrap();
    let label = format!("WindroidEject{}", std::process::id());
    let backing = home.join("detachable.sparseimage");
    let mount = Path::new("/Volumes").join(&label);
    assert!(!mount.exists());
    let hdi = |args: &[&str]| {
        let status = Command::new("/usr/bin/hdiutil")
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "{args:?} {status}");
    };
    let image_file = backing.to_str().unwrap();
    let mount_str = mount.to_str().unwrap();
    hdi(&[
        "create", "-size", "7g", "-type", "SPARSE", "-fs", "APFS", "-volname", &label, image_file,
    ]);
    hdi(&["attach", "-nobrowse", image_file]);
    let settings = StorageLocations {
        image_root: mount.join("images"),
        ..StorageLocations::defaults(&home)
    };
    fs::write(
        home.join(windroid_locations::SETTINGS),
        serde_json::to_vec(&settings).unwrap(),
    )
    .unwrap();
    let p = Provisioner::new(&home);
    p.start_image("system-image-35").unwrap();
    let active = until(&p, |s| {
        row(s, "system-image-35").downloaded_bytes >= 2 * 1024 * 1024 || s.phase == "failed"
    });
    assert_eq!(active.phase, "downloading", "{:?}", active.error);
    hdi(&["detach", "-force", mount_str]);
    let failed = until(&p, |s| s.phase == "failed");
    println!("forced ejection surfaced {:?}", failed.error);
    assert!(!mount.exists());
    drop(p);
    hdi(&["attach", "-nobrowse", image_file]);
    let p = Provisioner::new(&home);
    let before = p.snapshot();
    let image = row(&before, "system-image-35");
    assert!(image.downloaded_bytes > 0 && image.downloaded_bytes < image.size_bytes);
    println!(
        "reattached official API35 prefix {} bytes",
        image.downloaded_bytes
    );
    p.start_image("system-image-35").unwrap();
    assert!(row(&completed(&p), "system-image-35").ready);
    drop(p);
    hdi(&["detach", mount_str]);
}
