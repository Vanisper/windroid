//! 表单按所选镜像和最终屏幕读取限制。实际保存与启动在运行时再次检查。
use super::*;
use runtime::instances::{DiskLimits, Hardware, HostCapacity, SpecLimits};

pub(super) fn requested(data: &JsonValue) -> Result<Hardware, Rejection> {
    let count = |key: &str| {
        data[key]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|v| *v > 0)
    };
    Ok(Hardware {
        cpu_cores: count("cpuCores").ok_or_else(|| error("CPU 需要正整数核数"))?,
        ram_mib: count("ramMiB").ok_or_else(|| error("内存需要正整数 MiB"))?,
        graphics: serde_json::from_value(data["gpuMode"].clone())
            .map_err(|_| error("请选择有效的图形渲染模式"))?,
    })
}

// 内存演示没有镜像目录或数据卷；固定观测只供这条演示路径使用。
fn demo_limits(template: &str, api: u32) -> Result<SpecLimits, Rejection> {
    SpecLimits::for_template(
        HostCapacity {
            logical_cores: 10,
            physical_mib: 16 * 1024,
            hv_max_vcpus: 64,
        },
        template,
        api,
        "sdk_gphone64_arm64",
    )
    .map_err(error)
}

fn demo_disk() -> DiskLimits {
    DiskLimits::from_available(64 << 30)
}

pub(super) fn demo_config(
    spec: contract::Spec,
    hardware: Hardware,
) -> Result<InstanceConfig, Rejection> {
    let limits = demo_limits(&spec.template, spec.image.api)?;
    limits.validate(hardware).map_err(error)?;
    if !(6 << 30..=demo_disk().max_gib << 30).contains(&spec.disk_bytes) {
        return Err(error("用户空间超出演示卷允许的范围"));
    }
    Ok(InstanceConfig {
        spec,
        cpu_cores: Some(hardware.cpu_cores),
        ram_mib: Some(hardware.ram_mib),
        graphics: Some(hardware.graphics),
        screen_width: Some(limits.screen_width),
        screen_height: Some(limits.screen_height),
    })
}

impl Library {
    pub(super) fn connect_hardware(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.channel.handle("windroid.hardwareLimits", move |call| {
            let Some(lib) = weak.upgrade() else {
                return call.reject("应用库已关闭");
            };
            thread::spawn(move || {
                let reply = lib
                    .hardware_limits(call.data())
                    .map_err(|reason| match reason {
                        Rejection::Error { detail, .. } => detail,
                        _ => "无法读取实例规格".to_owned(),
                    });
                call.reply(reply);
            });
        });
    }

    fn hardware_limits(&self, data: &JsonValue) -> Result<JsonValue, Rejection> {
        if let Some(id) = data["instanceId"].as_str() {
            let instance = self
                .state
                .lock()
                .unwrap()
                .instances
                .get(id)
                .cloned()
                .ok_or_else(|| error("目标实例已不存在"))?;
            let limits = match (&self.store, &instance.record) {
                (Some(store), Some(record)) => store.edit_limits(record).map_err(error)?,
                _ => {
                    let c = instance.config.ok_or_else(|| error("实例缺少配置"))?;
                    demo_limits(&c.spec.template, c.spec.image.api)?
                }
            };
            return Ok(json!(limits));
        }
        let template = data["template"]
            .as_str()
            .ok_or_else(|| error("请选择设备模板"))?;
        let image = self.selected_image(data)?;
        let (limits, disk) = match &self.store {
            Some(store) => (
                store.spec_limits(template, &image).map_err(error)?,
                store.disk_limits().map_err(error)?,
            ),
            None => (demo_limits(template, image.api)?, demo_disk()),
        };
        let mut view = json!(limits);
        view["maxDiskGiB"] = json!(disk.max_gib);
        view["defaultDiskGiB"] = json!(disk.default_gib);
        Ok(view)
    }

    pub(super) fn update_hardware(&self, instance: &Instance, data: &JsonValue) -> Reply {
        let snap = instance.handle.snapshot();
        if snap.facts.process_alive || !matches!(snap.state, State::Stopped | State::Failed) {
            return Err(error("请先停止模拟器，再修改硬件规格"));
        }
        let hardware = requested(data)?;
        let id = instance.handle.id();
        let config = match &self.store {
            Some(store) => store
                .update_hardware(id, hardware)
                .map(|record| record.config(&self.layout))
                .map_err(error),
            None => {
                let saved = instance
                    .config
                    .as_ref()
                    .ok_or_else(|| error("实例缺少配置"))?;
                demo_config(saved.spec.clone(), hardware)
            }
        };
        let frozen = self.state.lock().unwrap().frozen.contains(id.as_str());
        // 实际工作区以退出回执为准。保存中途失败时，旧快照也可能已经失效。
        let invalidated = frozen
            && match &self.store {
                Some(store) => !matches!(store.quick_boot_saved(id), Ok(true)),
                None => {
                    config.is_ok()
                        && instance.config.as_ref().is_some_and(|saved| {
                            saved.cpu_cores != Some(hardware.cpu_cores)
                                || saved.ram_mib != Some(hardware.ram_mib)
                                || saved.graphics != Some(hardware.graphics)
                        })
                }
            };
        let mut state = self.state.lock().unwrap();
        if invalidated {
            state.frozen.remove(id.as_str());
        }
        let config = config?;
        state
            .instances
            .get_mut(id.as_str())
            .ok_or_else(|| error("目标实例已不存在"))?
            .config = Some(config);
        drop(state);
        self.note(Some(id.as_str()), false, "硬件规格已保存，下次启动生效");
        Ok(self.revision())
    }
}
