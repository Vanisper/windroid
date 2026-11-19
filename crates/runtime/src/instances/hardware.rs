//! 已保存的实例规格。旧实例的硬件设置保存在私有 AVD，创建清单记录模板、镜像和用户空间。

use super::{
    DiskLimits, GraphicsMode, Hardware, HostCapacity, InstanceRecord, InstanceStore, SpecLimits,
    data_dir, invalid, specification,
};
use crate::Layout;
use crate::provision::RuntimePaths;
use contract::{GuestAllocation, Image, InstanceId, Spec};
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceConfig {
    #[serde(flatten)]
    pub spec: Spec,
    pub cpu_cores: Option<u32>,
    #[serde(rename = "ramMiB")]
    pub ram_mib: Option<u32>,
    pub screen_width: Option<u32>,
    pub screen_height: Option<u32>,
    pub graphics: Option<GraphicsMode>,
}

impl InstanceRecord {
    /// 在挂载实例时读取一遍。单项缺失或格式不明保留为 None，旧工作区无需迁移。
    pub fn config(&self, layout: &Layout) -> InstanceConfig {
        let path = data_dir(&layout.instance(&self.id), self).join("config.ini");
        let ini = read_ini(&path, None).unwrap_or_default();
        InstanceConfig::read(self, &ini)
    }
}

/// 模拟器可能留下上一次启动的 hardware-qemu.ini。只有本轮写入才是当前分配。
pub(super) fn allocation(path: &Path, launched: SystemTime) -> Option<GuestAllocation> {
    let ini = read_ini(path, Some(launched)).ok()?;
    let cpu_cores = number(&ini, "hw.cpu.ncore");
    let ram_mib = number(&ini, "hw.ramSize");
    (cpu_cores.is_some() || ram_mib.is_some()).then_some(GuestAllocation { cpu_cores, ram_mib })
}

pub(super) fn read_ini(path: &Path, launched: Option<SystemTime>) -> io::Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(super::invalid("AVD 硬件配置需要普通文件"));
    }
    if let Some(start) = launched
        && !(start..=SystemTime::now()).contains(&meta.modified()?)
    {
        return Err(super::invalid("缺少本轮启动生成的 AVD 硬件配置"));
    }
    let mut text = String::new();
    file.take(65537).read_to_string(&mut text)?;
    if text.len() > 65536 {
        return Err(super::invalid("AVD 硬件配置过大"));
    }
    Ok(text)
}

/// 未提供的键与内容冲突分别处理；重复的值没有唯一解释。
pub(super) fn field<'a>(ini: &'a str, key: &str) -> io::Result<Option<&'a str>> {
    let mut matches = ini.lines().filter_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim())
    });
    let value = matches.next();
    if matches.next().is_some() {
        return Err(super::invalid(format!("配置项 {key} 重复")));
    }
    Ok(value)
}

fn number(ini: &str, key: &str) -> Option<u32> {
    field(ini, key).ok()??.parse().ok().filter(|n| *n > 0)
}

impl InstanceConfig {
    fn read(record: &InstanceRecord, ini: &str) -> Self {
        Self {
            spec: record.spec.clone(),
            cpu_cores: number(ini, "hw.cpu.ncore"),
            ram_mib: number(ini, "hw.ramSize"),
            screen_width: number(ini, "hw.lcd.width"),
            screen_height: number(ini, "hw.lcd.height"),
            graphics: graphics(ini).ok(),
        }
    }

    fn hardware(&self) -> io::Result<Hardware> {
        Ok(Hardware {
            cpu_cores: self
                .cpu_cores
                .ok_or_else(|| invalid("无法读取此实例配置的虚拟核心数"))?,
            ram_mib: self
                .ram_mib
                .ok_or_else(|| invalid("无法读取此实例配置的客户机内存"))?,
            graphics: self
                .graphics
                .ok_or_else(|| invalid("无法读取此实例配置的图形渲染模式"))?,
        })
    }
}

fn graphics(ini: &str) -> io::Result<GraphicsMode> {
    // 第三项以前的 AVD 固定写入 host，没有批处理标记。
    match (
        field(ini, "hw.gpu.mode")?,
        field(ini, "windroid.graphics.batch")?,
    ) {
        (Some("host"), Some("true")) => Ok(GraphicsMode::Batch),
        (Some("host"), None | Some("false")) => Ok(GraphicsMode::Host),
        (Some("auto"), None | Some("false")) => Ok(GraphicsMode::Auto),
        (Some("software"), None | Some("false")) => Ok(GraphicsMode::Software),
        _ => Err(invalid("AVD 图形渲染模式无法确认")),
    }
}

/// 37.2.13 的区域默认偏移为 -1、宽高为 0。折叠区域与传感器判断来自
/// https://android.googlesource.com/platform/external/qemu/+/8aaadeb1fea1202b287f1b8361f764a98950a27e/android/android-emu/android/hw-sensors.cpp
fn foldable(ini: &str) -> io::Result<bool> {
    fn integer(ini: &str, key: &str, default: i32) -> io::Result<i32> {
        field(ini, key)?
            .map(|s| s.parse().map_err(|_| invalid(format!("{key} 必须是整数"))))
            .unwrap_or(Ok(default))
    }
    for sensor in ["hw.sensor.hinge", "hw.sensor.roll"] {
        let enabled = match field(ini, sensor)? {
            None | Some("no" | "false" | "0") => false,
            Some("yes" | "true" | "1") => true,
            _ => return Err(invalid(format!("无法判断 {sensor} 是否启用"))),
        };
        if enabled && integer(ini, &format!("{sensor}.count"), 0)? > 0 {
            return Ok(true);
        }
    }
    for index in 1..=3 {
        let read =
            |prop, default| integer(ini, &format!("hw.displayRegion.0.{index}.{prop}"), default);
        if (0..=9999).contains(&read("xOffset", -1)?)
            && (0..=9999).contains(&read("yOffset", -1)?)
            && (1..=9999).contains(&read("width", 0)?)
            && (1..=9999).contains(&read("height", 0)?)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

impl InstanceStore {
    pub fn disk_limits(&self) -> io::Result<DiskLimits> {
        let path = self.home().join("instances");
        let (_, bytes, readonly) = crate::storage::volume_space(&path)?;
        if readonly {
            return Err(invalid(format!("实例数据卷为只读：{}", path.display())));
        }
        Ok(DiskLimits::from_available(bytes))
    }

    // 预览仅需小元数据。实际发布在版本租约中重复检查。
    pub fn spec_limits(&self, template: &str, image: &Image) -> io::Result<SpecLimits> {
        let paths = RuntimePaths::discover(self.home());
        let product = specification::product_name(&paths, image)?;
        SpecLimits::for_template(HostCapacity::read()?, template, image.api, &product)
    }

    pub fn edit_limits(&self, record: &InstanceRecord) -> io::Result<SpecLimits> {
        let paths = RuntimePaths::discover(self.home());
        let (limits, _) = self.saved_profile(record, &paths, HostCapacity::read()?)?;
        Ok(limits)
    }

    pub fn launch_hardware(&self, record: &InstanceRecord) -> io::Result<Hardware> {
        self.launch_hardware_at(
            record,
            &RuntimePaths::discover(self.home()),
            HostCapacity::read()?,
        )
    }

    pub fn launch_hardware_at(
        &self,
        record: &InstanceRecord,
        paths: &RuntimePaths,
        host: HostCapacity,
    ) -> io::Result<Hardware> {
        let (limits, saved) = self.saved_profile(record, paths, host)?;
        let mut effective = saved.hardware()?;
        // 早期 AVD 可以保存低于镜像下限的值。Emulator 自动提升内存，准入按提升后的值。
        effective.ram_mib = effective.ram_mib.max(limits.min_ram_mib);
        limits.validate(effective)?;
        Ok(effective)
    }

    fn saved_profile(
        &self,
        record: &InstanceRecord,
        paths: &RuntimePaths,
        host: HostCapacity,
    ) -> io::Result<(SpecLimits, InstanceConfig)> {
        let dir = self.layout.instance(&record.id);
        let pointer = read_ini(
            &dir.join("avd").join(format!("{}.ini", record.avd_name())),
            None,
        )?;
        if field(&pointer, "target")? != Some(&format!("android-{}", record.spec.image.api)) {
            return Err(invalid("AVD target 与绑定的镜像 API 不符"));
        }
        let ini = read_ini(&data_dir(&dir, record).join("config.ini"), None)?;
        let saved = InstanceConfig::read(record, &ini);
        let product = specification::product_name(paths, &record.spec.image)?;
        let limits = SpecLimits::calculate(
            host,
            &record.spec.template,
            record.spec.image.api,
            &product,
            saved
                .screen_width
                .ok_or_else(|| invalid("缺少 AVD 主屏宽度"))?,
            saved
                .screen_height
                .ok_or_else(|| invalid("缺少 AVD 主屏高度"))?,
            foldable(&ini)?,
        )?;
        Ok((limits, saved))
    }

    /// 与启动共用运行锁。原子替换 config.ini；镜像修订、用户分区与其他 AVD 选项由原文件保留。
    pub fn update_hardware(
        &self,
        id: &InstanceId,
        hardware: Hardware,
    ) -> io::Result<InstanceRecord> {
        let _selection = crate::storage::resource_lease(self.home(), true, true)?;
        let (root, _registry) = self.locked()?;
        let record = super::read_record(&root, id)?;
        let dir = root.join(id.as_str());
        let _lease = super::acquire_run_lock(&dir)
            .map_err(|e| io::Error::new(e.kind(), format!("请在实例停止后调整硬件：{e}")))?;
        super::process::ensure_previous_exit(&dir)?;
        let data = data_dir(&dir, &record);
        if data.canonicalize()? != data {
            return Err(invalid("无法确认实例私有 AVD 目录"));
        }
        let (paths, _versions) = crate::storage::instance_lease(self.home(), &record.spec.image)?;
        let (limits, saved) = self.saved_profile(&record, &paths, HostCapacity::read()?)?;
        // 已有实例可以只改核心数或图形并保留历史内存值。主动更换内存时须满足当前下限。
        let mut check = hardware;
        if saved.ram_mib == Some(check.ram_mib) {
            check.ram_mib = check.ram_mib.max(limits.min_ram_mib);
        }
        limits.validate(check)?;
        if saved.hardware().ok() == Some(hardware) {
            return Ok(record);
        }
        super::quick_boot::invalidate(&dir, &record)?;
        super::avd::replace_keys(
            &data,
            &[
                ("hw.cpu.ncore", hardware.cpu_cores.to_string()),
                ("hw.ramSize", hardware.ram_mib.to_string()),
                ("hw.gpu.enabled", "yes".to_owned()),
                ("hw.gpu.mode", hardware.graphics.gpu().to_owned()),
                (
                    "windroid.graphics.batch",
                    (hardware.graphics == GraphicsMode::Batch).to_string(),
                ),
            ],
        )?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foldable_regions_and_sensors_use_their_final_avd_values() {
        assert!(!foldable("").unwrap());
        assert!(!foldable("hw.sensor.hinge=yes").unwrap());
        assert!(foldable("hw.sensor.hinge=yes\nhw.sensor.hinge.count=1").unwrap());
        assert!(foldable("hw.sensor.roll=1\nhw.sensor.roll.count=1").unwrap());
        let region = "hw.displayRegion.0.2.xOffset=0\nhw.displayRegion.0.2.yOffset=15\nhw.displayRegion.0.2.width=120\nhw.displayRegion.0.2.height=256";
        assert!(foldable(region).unwrap());
        assert!(!foldable(&region.replace("height=256", "height=0")).unwrap());
        assert!(foldable("hw.sensor.hinge=unspecified").is_err());
    }

    #[test]
    fn preexisting_hardware_and_each_graphics_mode_have_unambiguous_launch_arguments() {
        assert_eq!(graphics("hw.gpu.mode=host").unwrap(), GraphicsMode::Host);
        for (mode, args) in [
            (GraphicsMode::Auto, vec!["-gpu", "auto"]),
            (GraphicsMode::Host, vec!["-gpu", "host"]),
            (GraphicsMode::Software, vec!["-gpu", "software"]),
            (
                GraphicsMode::Batch,
                vec![
                    "-gpu",
                    "host",
                    "-feature",
                    "VulkanBatchedDescriptorSetUpdate",
                ],
            ),
        ] {
            let ini = format!(
                "hw.gpu.mode={}\nwindroid.graphics.batch={}",
                mode.gpu(),
                mode == GraphicsMode::Batch
            );
            let mut command = std::process::Command::new("emulator");
            graphics(&ini).unwrap().apply(&mut command);
            assert_eq!(command.get_args().collect::<Vec<_>>(), args);
        }
    }
}
