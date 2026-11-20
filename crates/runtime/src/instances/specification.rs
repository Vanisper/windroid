//! 受管 Apple Silicon Emulator 的单台规格；宿主容量与数据卷余量每次从当前机器取得。
use super::{avd, hardware, invalid};
use crate::provision::RuntimePaths;
use contract::Image;
use serde::{Deserialize, Serialize};
use std::{io, process::Command};

const GIB: u64 = 1 << 30;

// 版本清单限定 darwin-aarch64 构建。该启动器选择 type=virt，Darwin 的默认 GIC 为 v2。
// https://android.googlesource.com/platform/external/qemu/+/8aaadeb1fea1202b287f1b8361f764a98950a27e/android-qemu2-glue/main.cpp#2740
// https://android.googlesource.com/platform/external/qemu/+/8aaadeb1fea1202b287f1b8361f764a98950a27e/hw/arm/virt.c#1766
struct EmulatorMachine {
    version: &'static str,
    build: &'static str,
    argument_max_cores: u32,
    machine_max_cores: u32,
    machine_max_ram_mib: u32,
}

const MANAGED_MACHINE: EmulatorMachine = EmulatorMachine {
    version: "37.2.13",
    build: "16740729",
    // https://android.googlesource.com/platform/external/qemu/+/8aaadeb1fea1202b287f1b8361f764a98950a27e/android/android-emu/android/main-common.c#2157
    argument_max_cores: 64,
    // https://android.googlesource.com/platform/external/qemu/+/8aaadeb1fea1202b287f1b8361f764a98950a27e/include/hw/intc/arm_gic_common.h#32
    machine_max_cores: 8,
    // https://android.googlesource.com/platform/external/qemu/+/8aaadeb1fea1202b287f1b8361f764a98950a27e/hw/arm/virt.c#125
    machine_max_ram_mib: 31 * 1024,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GraphicsMode {
    #[default]
    Host,
    Auto,
    Software,
    Batch,
}

impl GraphicsMode {
    pub(super) fn gpu(self) -> &'static str {
        match self {
            Self::Host | Self::Batch => "host",
            Self::Auto => "auto",
            Self::Software => "software",
        }
    }

    pub fn apply(self, command: &mut Command) {
        command.args(["-gpu", self.gpu()]);
        // 受管 Emulator 的 lib/advancedFeatures.ini 将客户机 Vulkan HAL 和描述符批处理分别控制。
        if self == Self::Batch {
            command.args([
                "-feature",
                "Vulkan",
                "-feature",
                "VulkanBatchedDescriptorSetUpdate",
            ]);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hardware {
    pub cpu_cores: u32,
    #[serde(rename = "ramMiB")]
    pub ram_mib: u32,
    pub graphics: GraphicsMode,
}

impl Hardware {
    pub(super) fn original(template: &str) -> Self {
        Self {
            cpu_cores: 2,
            ram_mib: super::ram_mib(template) as u32,
            graphics: GraphicsMode::Host,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HostCapacity {
    pub logical_cores: u32,
    pub physical_mib: u64,
    pub hv_max_vcpus: u32,
}

impl HostCapacity {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub fn read() -> io::Result<Self> {
        #[link(name = "Hypervisor", kind = "framework")]
        unsafe extern "C" {
            fn hv_vm_get_max_vcpu_count(count: *mut u32) -> i32;
        }
        let mut hv_max_vcpus = 0;
        // https://developer.apple.com/documentation/hypervisor/hv_vm_get_max_vcpu_count(_:)
        let result = unsafe { hv_vm_get_max_vcpu_count(&mut hv_max_vcpus) };
        if result != 0 || hv_max_vcpus == 0 {
            return Err(invalid(format!(
                "无法取得 Hypervisor 的单台核心上限（{result:#x}）"
            )));
        }
        let logical_cores = u32::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) })
            .ok()
            .filter(|n| *n != 0)
            .ok_or_else(|| invalid("无法取得本机逻辑核数"))?;
        Ok(Self {
            logical_cores,
            physical_mib: super::physical_memory_mib()?,
            hv_max_vcpus,
        })
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    pub fn read() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "当前受管模拟器需要 Apple 芯片的 Mac",
        ))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskLimits {
    #[serde(rename = "maxGiB")]
    pub max_gib: u64,
    #[serde(rename = "defaultGiB")]
    pub default_gib: Option<u64>,
}

impl DiskLimits {
    pub fn from_available(bytes: u64) -> Self {
        let max_gib = (u128::from(bytes) * 7 / 10 / u128::from(GIB)) as u64;
        let default_gib = match max_gib {
            0..=5 => None,
            6..=7 => Some(6),
            _ => Some(8),
        };
        Self {
            max_gib,
            default_gib,
        }
    }

    pub(super) fn check(&self, bytes: u64) -> io::Result<()> {
        if bytes > self.max_gib.saturating_mul(GIB) || self.max_gib < 6 {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "实例数据卷可用空间的 70% 仅能分配 {} GiB，本次需要 {} GiB，最小 6 GiB。请释放空间。",
                    self.max_gib,
                    bytes.div_ceil(GIB)
                ),
            ));
        }
        if bytes < 6 * GIB || !bytes.is_multiple_of(GIB) {
            return Err(invalid("用户空间需要至少 6 GiB 的整数"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecLimits {
    pub max_cpu_cores: u32,
    #[serde(rename = "minRamMiB")]
    pub min_ram_mib: u32,
    #[serde(rename = "maxRamMiB")]
    pub max_ram_mib: u32,
    pub default_cpu_cores: u32,
    #[serde(rename = "defaultRamMiB")]
    pub default_ram_mib: u32,
    pub screen_width: u32,
    pub screen_height: u32,
}

impl SpecLimits {
    pub fn for_template(
        host: HostCapacity,
        template: &str,
        api: u32,
        product_name: &str,
    ) -> io::Result<Self> {
        let (width, height, _) = avd::dimensions(template)?;
        Self::calculate(host, template, api, product_name, width, height, false)
    }

    pub(super) fn calculate(
        host: HostCapacity,
        template: &str,
        api: u32,
        product: &str,
        width: u32,
        height: u32,
        foldable: bool,
    ) -> io::Result<Self> {
        if product.is_empty() || api < 29 || width == 0 || height == 0 {
            return Err(invalid("缺少已验证的镜像规格或主屏像素配置"));
        }
        if host.logical_cores < 2 || host.hv_max_vcpus == 0 {
            return Err(invalid(
                "运行此镜像至少需要双核 Mac 和可用的 Hypervisor 核心",
            ));
        }
        let max_cpu_cores = MANAGED_MACHINE
            .argument_max_cores
            .min(MANAGED_MACHINE.machine_max_cores)
            .min(host.logical_cores)
            .min(host.hv_max_vcpus);
        let max_ram_mib =
            (host.physical_mib / 2).min(u64::from(MANAGED_MACHINE.machine_max_ram_mib)) as u32;
        // 按 propertyFile_getAvdFlavor 的优先顺序和 main-common.c 的 CDD 分支计算。
        // https://android.googlesource.com/platform/external/qemu/+/8aaadeb1fea1202b287f1b8361f764a98950a27e/android/emu/avd/src/android/avd/util.c#236
        // https://android.googlesource.com/platform/external/qemu/+/8aaadeb1fea1202b287f1b8361f764a98950a27e/android/android-emu/android/main-common.c#1266
        let regular_min = if api >= 37
            || api >= 33 && (foldable || u64::from(width) * u64::from(height) >= 3_000_000)
        {
            4096
        } else if api >= 34 {
            2560
        } else {
            2048
        };
        let min_ram_mib = if product.contains("phone") {
            regular_min
        } else if product.contains("atv") {
            1024
        } else if product.contains("aw") || product.contains("wear") {
            2048
        } else if !["car", "pc", "desktop"].iter().any(|p| product.contains(p))
            && product.contains("xr")
        {
            4096
        } else {
            regular_min
        };
        if max_ram_mib < min_ram_mib {
            return Err(invalid(format!(
                "此实例至少需要 {min_ram_mib} MiB 内存，本机单台最多可分配 {max_ram_mib} MiB"
            )));
        }
        Ok(Self {
            max_cpu_cores,
            max_ram_mib,
            min_ram_mib,
            default_cpu_cores: 2.min(max_cpu_cores),
            default_ram_mib: (super::ram_mib(template) as u32).clamp(min_ram_mib, max_ram_mib),
            screen_width: width,
            screen_height: height,
        })
    }

    pub fn validate(&self, hardware: Hardware) -> io::Result<()> {
        if !(1..=self.max_cpu_cores).contains(&hardware.cpu_cores) {
            return Err(invalid(format!(
                "本机与当前机型允许分配 1 至 {} 个虚拟核心",
                self.max_cpu_cores
            )));
        }
        if !(self.min_ram_mib..=self.max_ram_mib).contains(&hardware.ram_mib) {
            return Err(invalid(format!(
                "此配置可分配 {} 至 {} MiB 客户机内存",
                self.min_ram_mib, self.max_ram_mib
            )));
        }
        Ok(())
    }

    pub(super) fn defaults(&self) -> Hardware {
        Hardware {
            cpu_cores: self.default_cpu_cores,
            ram_mib: self.default_ram_mib,
            graphics: GraphicsMode::Host,
        }
    }
}

/// 预测适用范围绑定到实际版本元数据；导入、迁移和内置镜像走同一条属性读取路径。
pub(super) fn product_name(paths: &RuntimePaths, image: &Image) -> io::Result<String> {
    let emulator_dir = paths
        .emulator
        .parent()
        .ok_or_else(|| invalid("模拟器目录无效"))?;
    let properties = hardware::read_ini(&emulator_dir.join("source.properties"), None)
        .map_err(|e| invalid(format!("无法读取受管 Emulator 版本：{e}")))?;
    if hardware::field(&properties, "Pkg.Revision")? != Some(MANAGED_MACHINE.version)
        || hardware::field(&properties, "Pkg.BuildId")? != Some(MANAGED_MACHINE.build)
        || image.abi != concat!("arm64", "-v8a")
    {
        return Err(invalid("此 Emulator 版本、构建或机型缺少已验证的硬件限值"));
    }
    let build = hardware::read_ini(
        &paths
            .images
            .join(avd::image_subdir(image)?)
            .join("build.prop"),
        None,
    )
    .map_err(|e| invalid(format!("无法读取镜像的 build.prop：{e}")))?;
    if hardware::field(&build, "ro.build.version.sdk")?.and_then(|s| s.parse().ok())
        != Some(image.api)
        || hardware::field(&build, "ro.product.cpu.abi")? != Some(image.abi.as_str())
    {
        return Err(invalid("镜像 build.prop 的 API 或 ABI 与所选修订不符"));
    }
    for key in [
        "ro.product.name",
        "ro.product.system.name",
        "ro.build.flavor",
    ] {
        if let Some(name) = hardware::field(&build, key)?.filter(|s| !s.is_empty()) {
            return Ok(name.to_owned());
        }
    }
    Err(invalid("镜像缺少产品名称，无法确认客户机内存下限"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(cores: u32, gib: u64, hv_max_vcpus: u32) -> HostCapacity {
        HostCapacity {
            logical_cores: cores,
            physical_mib: gib * 1024,
            hv_max_vcpus,
        }
    }

    #[test]
    fn disk_limits_round_down_before_choosing_the_default() {
        for (max, default) in [
            (0, None),
            (5, None),
            (6, Some(6)),
            (7, Some(6)),
            (8, Some(8)),
            (32, Some(8)),
        ] {
            let threshold = (max * GIB * 10_u64).div_ceil(7);
            let limits = DiskLimits::from_available(threshold);
            assert_eq!((limits.max_gib, limits.default_gib), (max, default));
            if max >= 6 {
                limits.check(max * GIB).unwrap();
                assert!(
                    DiskLimits::from_available(threshold - 1)
                        .check(max * GIB)
                        .is_err()
                );
            }
        }
        assert!(
            DiskLimits::from_available(u64::MAX)
                .check(6 * GIB + 1)
                .is_err()
        );
    }

    #[test]
    fn caps_track_host_and_machine_and_floors_track_image_and_pixels() {
        for (host, cpu, ram) in [
            (host(4, 8, 64), 4, 4096),
            (host(10, 16, 64), 8, 8192),
            (host(32, 64, 6), 6, 31 * 1024),
        ] {
            let limits = SpecLimits::for_template(host, "compact-720", 34, "phone").unwrap();
            assert_eq!((limits.max_cpu_cores, limits.max_ram_mib), (cpu, ram));
            for hw in [
                Hardware {
                    cpu_cores: 1,
                    ..limits.defaults()
                },
                Hardware {
                    cpu_cores: cpu,
                    ram_mib: ram,
                    ..limits.defaults()
                },
            ] {
                limits.validate(hw).unwrap();
            }
            assert!(
                limits
                    .validate(Hardware {
                        cpu_cores: cpu + 1,
                        ..limits.defaults()
                    })
                    .is_err()
            );
            assert!(
                limits
                    .validate(Hardware {
                        ram_mib: ram + 1,
                        ..limits.defaults()
                    })
                    .is_err()
            );
        }
        for (api, product, width, folded, expected) in [
            (33, "phone", 1999, false, 2048),
            (34, "phone", 1999, false, 2560),
            (35, "phone", 2000, false, 4096),
            (34, "other", 1999, true, 4096),
            (37, "phone", 1, false, 4096),
            (35, "atv", 2000, false, 1024),
            (35, "wear", 1999, false, 2048),
            (33, "xr", 1, false, 4096),
        ] {
            let limit = SpecLimits::calculate(
                host(6, 16, 64),
                "phone-1080",
                api,
                product,
                width,
                1500,
                folded,
            )
            .unwrap();
            assert_eq!(limit.min_ram_mib, expected);
        }
        assert!(
            SpecLimits::for_template(host(4, 4, 64), "tv-4k", 34, "phone")
                .unwrap_err()
                .to_string()
                .contains("4096")
        );
    }
}
