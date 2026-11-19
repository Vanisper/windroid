use serde::{Deserialize, Serialize};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryPressure {
    Normal,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MemoryObservation {
    pub pressure: MemoryPressure,
    pub additional_mib: u64,
    /// CLOCK_MONOTONIC 纳秒，与同一台宿主上其他进程的预留时间可直接比较。
    pub sampled_at: u64,
}

impl MemoryObservation {
    pub fn current(pressure: MemoryPressure, additional_mib: u64) -> io::Result<Self> {
        Ok(Self {
            pressure,
            additional_mib,
            sampled_at: now()?,
        })
    }

    pub(super) fn is_fresh(self, now: u64) -> bool {
        now.checked_sub(self.sampled_at)
            .is_some_and(|age| age < super::SAMPLE_LIFETIME_NS)
    }
}

pub(super) fn now() -> io::Result<u64> {
    let mut ts = std::mem::MaybeUninit::uninit();
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, ts.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let ts = unsafe { ts.assume_init() };
    Ok(ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
}

#[cfg(target_os = "macos")]
pub(super) fn read() -> io::Result<MemoryObservation> {
    use std::ffi::CStr;
    unsafe extern "C" {
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }
    fn sysctl(name: &CStr) -> io::Result<u32> {
        let mut value = 0_u32;
        let mut len = size_of::<u32>();
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut value as *mut u32).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len != size_of::<u32>() {
            return Err(io::Error::other(format!(
                "无法读取 {}",
                name.to_string_lossy()
            )));
        }
        Ok(value)
    }

    let sampled_at = now()?;
    // 用户态 sysctl 返回 NOTE_MEMORYSTATUS_PRESSURE_*，有别于内核内部的枚举。
    // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_memorystatus_notify.c
    let pressure = match sysctl(c"kern.memorystatus_vm_pressure_level")? {
        1 => MemoryPressure::Normal,
        2 => MemoryPressure::Warning,
        4 => MemoryPressure::Critical,
        value => return Err(io::Error::other(format!("未知内存压力等级 {value}"))),
    };
    if pressure == MemoryPressure::Critical {
        return Ok(MemoryObservation {
            pressure,
            additional_mib: 0,
            sampled_at,
        });
    }
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    // SAFETY: host_statistics64 按 count 写入 vm_statistics64，返回后释放 host send right。
    #[allow(deprecated)]
    let rc = unsafe {
        let host = libc::mach_host_self();
        let rc = libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            (&mut stats as *mut libc::vm_statistics64).cast(),
            &mut count,
        );
        mach_port_deallocate(libc::mach_task_self(), host);
        rc
    };
    let bytes = count as usize * size_of::<libc::integer_t>();
    if rc != 0
        || bytes < std::mem::offset_of!(libc::vm_statistics64, internal_page_count)
        || page_size <= 0
    {
        return Err(io::Error::other("无法读取本机 VM 页统计"));
    }
    // free_count 含 speculative；扣掉后再估计可回收页。两种计数的较小值是估算，
    // 内核不会保证这些页都干净。保留本机 pageout 的 free target，避免假设固定机型余量。
    // https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/mach/vm_statistics.h
    // https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/vm/vm_pageout.c
    let free = stats.free_count.saturating_sub(stats.speculative_count);
    // 新版 SDK 的 rev6 直接报告非活动的外部页；旧内核仅提供两个总数。
    let file_cache = if bytes
        >= std::mem::offset_of!(libc::vm_statistics64, inactive_external_count) + size_of::<u64>()
    {
        stats.inactive_external_count
    } else {
        u64::from(stats.inactive_count.min(stats.external_page_count))
    };
    let target = sysctl(c"vm.vm_page_free_target")?;
    let pages = (u64::from(free) + file_cache).saturating_sub(u64::from(target));
    let additional_mib = pages.saturating_mul(page_size as u64) >> 20;
    Ok(MemoryObservation {
        pressure,
        additional_mib,
        sampled_at,
    })
}

#[cfg(not(target_os = "macos"))]
pub(super) fn read() -> io::Result<MemoryObservation> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "当前宿主不提供 macOS 内存观测",
    ))
}
