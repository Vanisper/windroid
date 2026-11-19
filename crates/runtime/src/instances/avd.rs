//! 按产品规格生成私有 AVD。每台实例从选定版本的共享镜像初始化自己的数据盘。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{InstanceRecord, invalid, private_dir};
use contract::{Image, Spec};

pub(super) fn validate(spec: &Spec) -> io::Result<()> {
    dimensions(&spec.template)?;
    if spec.disk_bytes < 6 << 30 || spec.disk_bytes > i64::MAX as u64 {
        return Err(invalid(
            "数据分区至少需要 6 GiB，大小必须能用有符号 64 位字节数表示",
        ));
    }
    if spec.image.revision == 0 {
        return Err(invalid("系统镜像需要正整数修订号"));
    }
    if spec.image.api == 0
        || spec.image.tag.is_empty()
        || spec.image.tag.len() > 80
        || !spec
            .image
            .tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(invalid("Android 镜像标识无效"));
    }
    abi(&spec.image)?;
    Ok(())
}

fn dimensions(template: &str) -> io::Result<(u32, u32, u32)> {
    match template {
        "compact-720" => Ok((1280, 720, 240)),
        "phone-1080" => Ok((1080, 2400, 420)),
        "tablet-1200" => Ok((1920, 1200, 240)),
        "tv-1080" => Ok((1920, 1080, 320)),
        "tv-4k" => Ok((3840, 2160, 320)),
        _ => Err(invalid(format!("未知设备模板 {template}"))),
    }
}

// SDK 镜像目录用 ABI，模拟器的 hw.cpu.arch 使用内核架构名。
fn abi(image: &Image) -> io::Result<(&'static str, &'static str)> {
    match image.abi.as_str() {
        "arm64" | "arm64-v8a" => Ok(("arm64-v8a", "arm64")),
        "x86_64" => Ok(("x86_64", "x86_64")),
        _ => Err(invalid(format!("不支持的 Android ABI {}", image.abi))),
    }
}

pub(crate) fn image_subdir(image: &Image) -> io::Result<PathBuf> {
    Ok(PathBuf::from(format!("android-{}", image.api))
        .join(&image.tag)
        .join(abi(image)?.0)
        .join(format!("r{}", image.revision)))
}

/// 模拟器读取 AVD 时先查 image.sysdir.1。继承时保留 SDK 相对路径，独立目录用绝对路径。
fn ini_image<'a>(image: &'a Path, sdk: &Path) -> io::Result<&'a str> {
    image
        .strip_prefix(sdk)
        .unwrap_or(image)
        .to_str()
        .filter(|path| !path.contains(['\n', '\r']))
        .ok_or_else(|| invalid("镜像目录不能表示为 Android AVD 配置路径"))
}

pub(super) fn sync_image(data: &Path, image: &Path, sdk: &Path) -> io::Result<()> {
    let path = ini_image(image, sdk)?;
    let key = "image.sysdir.1";
    let wanted = format!("{key}={path}/");
    let config = data.join("config.ini");
    let original = fs::read_to_string(&config)?;
    let matches = |line: &str| {
        line.split_once('=')
            .is_some_and(|(name, _)| name.trim() == key)
    };
    let mut found = original.lines().filter(|line| matches(line));
    if found.next() == Some(wanted.as_str()) && found.next().is_none() {
        return Ok(());
    }
    // 旧实例只更新镜像引用。保留硬件设置、私有数据与快照。
    let mut lines: Vec<_> = original.lines().filter(|line| !matches(line)).collect();
    lines.push(&wanted);
    let temp = data.join(format!(".config-{}", super::nonce()?));
    let mut file = fs::File::create_new(&temp)?;
    let result = (|| {
        use std::io::Write;
        writeln!(file, "{}", lines.join("\n"))?;
        file.sync_all()?;
        fs::rename(&temp, config)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

pub(crate) fn data_dir(dir: &Path, record: &InstanceRecord) -> PathBuf {
    dir.join("avd").join(format!("{}.avd", record.avd_name()))
}

pub(super) fn generate(
    stage: &Path,
    final_dir: &Path,
    record: &InstanceRecord,
    paths: &crate::provision::RuntimePaths,
) -> io::Result<()> {
    let spec = &record.spec;
    let (width, height, density) = dimensions(&spec.template)?;
    let (abi, arch) = abi(&spec.image)?;
    let image = paths.images.join(image_subdir(&spec.image)?);
    let name = record.avd_name();
    let avd = data_dir(stage, record);
    private_dir(&stage.join("avd"))?;
    private_dir(&avd)?;
    let data = data_dir(final_dir, record);
    let path = data
        .to_str()
        .filter(|p| !p.contains(['\n', '\r']))
        .ok_or_else(|| invalid("实例目录不能表示为 Android AVD 配置路径"))?;
    let pointer = format!(
        "avd.ini.encoding=UTF-8\npath={path}\ntarget=android-{}\n",
        spec.image.api
    );
    fs::write(stage.join("avd").join(format!("{name}.ini")), pointer)?;
    // 显示名可由用户填写，避免将其写入无转义机制的 INI。
    let config = format!(
        concat!(
            "avd.ini.encoding=UTF-8\nAvdId={name}\navd.ini.displayname={name}\n",
            "abi.type={abi}\nhw.cpu.arch={arch}\nhw.cpu.ncore=2\nhw.ramSize={ram}\n",
            "hw.lcd.width={width}\nhw.lcd.height={height}\nhw.lcd.density={density}\n",
            "hw.gpu.enabled=yes\nhw.gpu.mode=host\nhw.keyboard=yes\nhw.mainKeys=no\n",
            "hw.sdCard=no\nshowDeviceFrame=no\nfastboot.forceColdBoot=no\n",
            "image.sysdir.1={image}/\ntag.id={tag}\nPlayStore.enabled={play}\n",
            "disk.dataPartition.size={size}\n"
        ),
        name = name,
        abi = abi,
        arch = arch,
        width = width,
        height = height,
        density = density,
        ram = super::ram_mib(&spec.template),
        image = ini_image(&image, &paths.sdk)?,
        tag = spec.image.tag,
        play = spec.image.tag == "google_apis_playstore",
        size = spec.disk_bytes
    );
    fs::write(avd.join("config.ini"), config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_config_preserves_custom_hardware_and_private_disks_when_rebound() {
        let data =
            std::env::temp_dir().join(format!("windroid-avd-{}", super::super::nonce().unwrap()));
        fs::create_dir(&data).unwrap();
        let disk = data.join("userdata-qemu.img");
        fs::write(&disk, b"private").unwrap();
        let config = data.join("config.ini");
        let prior = "hw.cpu.ncore=6\nhw.ramSize=3072\nimage.sysdir.1 = /old/system/\nimage.sysdir.1=system-images/old/\nhw.gpu.mode=host\n";
        fs::write(&config, prior).unwrap();
        let sdk = Path::new("/shared/sdk");
        let standalone = Path::new("/external/images/android-34/play/test-abi");
        sync_image(&data, standalone, sdk).unwrap();
        let migrated = fs::read_to_string(&config).unwrap();
        assert_eq!(
            migrated
                .lines()
                .filter(|line| line.starts_with("image.sysdir.1"))
                .collect::<Vec<_>>(),
            ["image.sysdir.1=/external/images/android-34/play/test-abi/"]
        );
        for line in ["hw.cpu.ncore=6", "hw.ramSize=3072", "hw.gpu.mode=host"] {
            assert!(migrated.lines().any(|item| item == line));
        }
        sync_image(&data, &sdk.join("system-images/restored"), sdk).unwrap();
        assert!(
            fs::read_to_string(&config)
                .unwrap()
                .contains("image.sysdir.1=system-images/restored/\n")
        );
        assert_eq!(fs::read(&disk).unwrap(), b"private");
        fs::remove_dir_all(data).unwrap();
    }
}
