//! 已保存的实例规格。旧实例的硬件设置保存在私有 AVD，创建清单记录模板、镜像和用户空间。

use super::{InstanceRecord, data_dir};
use crate::Layout;
use contract::{GuestAllocation, Spec};
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
}

impl InstanceRecord {
    /// 在挂载实例时读取一遍。单项缺失或格式不明保留为 None，旧工作区无需迁移。
    pub fn config(&self, layout: &Layout) -> InstanceConfig {
        let path = data_dir(&layout.instance(&self.id), self).join("config.ini");
        let ini = read_ini(&path, None).unwrap_or_default();
        InstanceConfig {
            spec: self.spec.clone(),
            cpu_cores: number(&ini, "hw.cpu.ncore"),
            ram_mib: number(&ini, "hw.ramSize"),
            screen_width: number(&ini, "hw.lcd.width"),
            screen_height: number(&ini, "hw.lcd.height"),
        }
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
