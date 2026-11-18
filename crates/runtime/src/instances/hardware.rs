//! 已保存的实例规格。旧实例的硬件设置保存在私有 AVD，创建清单记录模板、镜像和用户空间。

use super::{InstanceRecord, data_dir};
use crate::Layout;
use contract::Spec;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

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
        let ini = read_ini(&path).unwrap_or_default();
        InstanceConfig {
            spec: self.spec.clone(),
            cpu_cores: number(&ini, "hw.cpu.ncore"),
            ram_mib: number(&ini, "hw.ramSize"),
            screen_width: number(&ini, "hw.lcd.width"),
            screen_height: number(&ini, "hw.lcd.height"),
        }
    }
}

fn read_ini(path: &Path) -> io::Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let mut text = String::new();
    if file.metadata()?.is_file() {
        file.take(65537).read_to_string(&mut text)?;
    }
    if text.len() > 65536 {
        return Err(super::invalid("AVD 硬件配置过大"));
    }
    Ok(text)
}

fn number(ini: &str, key: &str) -> Option<u32> {
    let mut matches = ini.lines().filter_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim())
    });
    let value: u32 = matches.next()?.parse().ok()?;
    (value > 0 && matches.next().is_none()).then_some(value)
}
