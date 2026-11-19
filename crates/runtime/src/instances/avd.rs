//! 按产品规格生成私有 AVD。模拟器在首次启动时由共享 userdata.img 初始化私有数据盘。

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
        .join(abi(image)?.0))
}

pub(crate) fn data_dir(dir: &Path, record: &InstanceRecord) -> PathBuf {
    dir.join("avd").join(format!("{}.avd", record.avd_name()))
}

pub(super) fn generate(stage: &Path, final_dir: &Path, record: &InstanceRecord) -> io::Result<()> {
    let spec = &record.spec;
    let (width, height, density) = dimensions(&spec.template)?;
    let (abi, arch) = abi(&spec.image)?;
    let image = Path::new("system-images").join(image_subdir(&spec.image)?);
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
        image = image.display(),
        tag = spec.image.tag,
        play = spec.image.tag == "google_apis_playstore",
        size = spec.disk_bytes
    );
    fs::write(avd.join("config.ini"), config)
}
