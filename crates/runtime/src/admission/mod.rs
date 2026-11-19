//! 本机同一登录用户的启动预留。申请端口、核数和客户机内存在同一把锁内完成。
//! 每个活动端口的文件锁由启动者及模拟器继承；异常退出无需清理持久计数器。

mod observation;
use crate::instances::InstanceStore;
pub use observation::{MemoryObservation, MemoryPressure};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// 交叠的启动往往尚未体现在内核 VM 计数中。在这个短窗口内同时遵守先前观测的余额。
// XNU host_statistics 会在一秒窗口内复用页计数。每次仍检查当前压力，再额外保留一次
// 采样窗口覆盖进程进入 QEMU 的时间；两秒不是虚拟机内存实际全部常驻的保证。
// https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/host.c
const SAMPLE_LIFETIME_NS: u64 = 2_000_000_000;

#[derive(Clone)]
pub struct Admission {
    directory: PathBuf,
    observe: Arc<dyn Fn() -> io::Result<MemoryObservation> + Send + Sync>,
}

impl Default for Admission {
    fn default() -> Self {
        // WINDROID_HOME、TMPDIR 和工具的 Android 环境都可按实例不同。
        // uid 与内核端口命名空间用于跨独立打开的工作区共享活动额度。
        let directory = Path::new("/tmp").join(format!("windroid-admission-v1-{}", unsafe {
            libc::geteuid()
        }));
        Self::new(directory, observation::read)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) struct Demand {
    pub cores: u32,
    pub ram_mib: u32,
}

#[derive(Clone, Copy)]
pub(crate) struct Budget {
    pub logical_cores: u32,
    pub physical_mib: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Grant {
    port: u16,
    since: u64,
    #[serde(default)]
    instance: Option<(u64, u64)>,
    demand: Demand,
    sample: MemoryObservation,
}

pub(crate) struct Reservation {
    pub file: File,
    grant: Grant,
    child_holds_lock: bool,
}

impl Reservation {
    pub fn child_inherited(&mut self) {
        self.child_holds_lock = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // spawn 之前失败时可立即归还。已交接时由持有同一文件描述的最后一个进程释放。
        if !self.child_holds_lock {
            let _ = self.file.unlock();
        }
    }
}

impl Admission {
    /// 隔离测试可提供自己的锁目录及按单调时钟标记的宿主观测。
    pub fn new(
        directory: PathBuf,
        observe: impl Fn() -> io::Result<MemoryObservation> + Send + Sync + 'static,
    ) -> Self {
        Self {
            directory,
            observe: Arc::new(observe),
        }
    }

    fn lock(&self) -> io::Result<File> {
        match fs::DirBuilder::new().mode(0o700).create(&self.directory) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            other => other?,
        }
        let meta = fs::symlink_metadata(&self.directory)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err(io::Error::other("启动资源预留目录需要当前用户的私有目录"));
        }
        let guard = lock_file(&self.directory.join("lock"))?;
        guard.lock()?;
        Ok(guard)
    }

    fn grants(&self) -> io::Result<Vec<Grant>> {
        let mut active = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let Some(port) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u16>().ok())
            else {
                continue;
            };
            let file = lock_file(&entry.path())?;
            match file.try_lock() {
                Ok(()) => {
                    // 显式解锁，避免并发 fork 短暂继承这个只用于检查的文件描述符。
                    file.unlock()?;
                    continue;
                }
                Err(std::fs::TryLockError::WouldBlock) => (),
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
            let grant: Grant = serde_json::from_reader(file.take(4096))
                .map_err(|e| io::Error::other(format!("无法确认端口 {port} 的活动预留：{e}")))?;
            if grant.port != port || grant.demand.cores == 0 || grant.demand.ram_mib == 0 {
                return Err(io::Error::other(format!("端口 {port} 的活动资源记录无效")));
            }
            active.push(grant);
        }
        Ok(active)
    }

    fn sample(&self) -> io::Result<MemoryObservation> {
        let read = || {
            let sample = (self.observe)()?;
            if !sample.is_fresh(observation::now()?) {
                return Err(io::Error::other("观测已过期"));
            }
            Ok(sample)
        };
        read().or_else(|_| read()).map_err(|e| {
            io::Error::other(format!("无法确认宿主当前的内存余量或压力，请重试（{e}）"))
        })
    }

    pub(crate) fn reserve(
        &self,
        port: u16,
        demand: Demand,
        budget: Budget,
        workspace: Option<(&InstanceStore, &Path)>,
    ) -> io::Result<Reservation> {
        let _guard = self.lock()?;
        let grants = self.grants()?;
        let instance = if let Some((store, dir)) = workspace {
            check_workspace(store, &grants)?;
            Some(object(dir)?)
        } else {
            None
        };
        if grants.iter().any(|g| g.port == port) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("端口 {port} 已被另一次启动预留"),
            ));
        }
        let sample = self.sample()?;
        let since = observation::now()?;
        check(&grants, None, demand, budget, sample, since)?;
        let grant = Grant {
            port,
            since,
            instance,
            demand,
            sample,
        };
        let mut file = lock_file(&self.directory.join(port.to_string()))?;
        file.try_lock().map_err(io::Error::from)?;
        file.set_len(0)?;
        serde_json::to_writer(&mut file, &grant)?;
        file.flush()?;
        Ok(Reservation {
            file,
            grant,
            child_holds_lock: false,
        })
    }

    /// 同一启动回读到 QEMU 最终参数后，以真实分配计入后续共享额度。
    pub(crate) fn reconcile(
        &self,
        current: &mut Reservation,
        actual: contract::GuestAllocation,
    ) -> io::Result<()> {
        let prior = current.grant.demand;
        let cores = actual.cpu_cores.unwrap_or(prior.cores);
        let ram_mib = actual.ram_mib.unwrap_or(prior.ram_mib);
        if cores == prior.cores && ram_mib == prior.ram_mib {
            return Ok(());
        }
        let _guard = self.lock()?;
        let updated = Grant {
            demand: Demand { cores, ram_mib },
            ..current.grant
        };
        // 与读取同锁。写入中断留下不可解析的活动记录，其他启动会等待修复或退出。
        current.file.rewind()?;
        current.file.set_len(0)?;
        serde_json::to_writer(&mut current.file, &updated)?;
        current.file.flush()?;
        current.grant = updated;
        Ok(())
    }

    pub(crate) fn check_replacement(
        &self,
        current: &Reservation,
        demand: Demand,
        budget: Budget,
        workspace: Option<&InstanceStore>,
    ) -> io::Result<()> {
        let _guard = self.lock()?;
        let grants = self.grants()?;
        if let Some(store) = workspace {
            check_workspace(store, &grants)?;
        }
        if !grants
            .iter()
            .any(|g| g.port == current.grant.port && g.since == current.grant.since)
        {
            return Err(io::Error::other("无法确认本次模拟器的活动预留"));
        }
        check(
            &grants,
            Some(current.grant),
            demand,
            budget,
            self.sample()?,
            observation::now()?,
        )
    }
}

fn object(dir: &Path) -> io::Result<(u64, u64)> {
    let meta = fs::metadata(dir)?;
    Ok((meta.dev(), meta.ino()))
}

fn check_workspace(store: &InstanceStore, grants: &[Grant]) -> io::Result<()> {
    // 旧版窗口能在当前工作区留下仍在运行、只有 PID 而没有额度记录的实例。
    // 端口可被其他工作区复用，同时核对实例目录对象才能认作同一笔活动预留。
    for record in store.list()? {
        let dir = store.home().join("instances").join(record.id.as_str());
        if let Some(pid) = crate::instances::process::live_marker(&dir)? {
            let source = object(&dir)?;
            if !grants
                .iter()
                .any(|g| g.port == record.port && g.instance == Some(source))
            {
                return Err(io::Error::other(format!(
                    "无法确认实例「{}」的活动资源（PID {pid}），请先关闭其原有会话再启动",
                    record.name
                )));
            }
        }
    }
    Ok(())
}

fn lock_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 || meta.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other("无法确认启动资源预留文件"));
    }
    Ok(file)
}

fn check(
    grants: &[Grant],
    replaced: Option<Grant>,
    demand: Demand,
    budget: Budget,
    sample: MemoryObservation,
    now: u64,
) -> io::Result<()> {
    if sample.pressure == MemoryPressure::Critical {
        return Err(io::Error::other("本机内存压力过高，请稍后重新启动实例"));
    }
    let others = grants
        .iter()
        .filter(|g| Some(g.port) != replaced.map(|r| r.port));
    let used_cores: u64 = others.clone().map(|g| u64::from(g.demand.cores)).sum();
    let free_cores = u64::from(budget.logical_cores).saturating_sub(used_cores);
    if u64::from(demand.cores) > free_cores {
        return Err(io::Error::other(format!(
            "启动需要 {} 核，本机当前可分配 {free_cores} 核",
            demand.cores
        )));
    }
    let used_mib: u64 = others.clone().map(|g| u64::from(g.demand.ram_mib)).sum();
    let free_mib = (budget.physical_mib / 2).saturating_sub(used_mib);
    if u64::from(demand.ram_mib) > free_mib {
        return Err(io::Error::other(format!(
            "启动需要 {} MiB，客户机内存共享额度剩余 {free_mib} MiB",
            demand.ram_mib
        )));
    }
    // 逐笔对照采样时刻。已存在的 VM 占用已反映在系统页数中；旧 VM 退出不能抵消
    // 采样后另一个 VM 的新增申请。重启只可使用自己将释放的容量。
    let mut available = u64::MAX;
    for s in
        std::iter::once(sample).chain(grants.iter().map(|g| g.sample).filter(|s| s.is_fresh(now)))
    {
        let new_mib: u64 = others
            .clone()
            .filter(|g| g.since >= s.sampled_at)
            .map(|g| u64::from(g.demand.ram_mib))
            .sum();
        let released = replaced
            .filter(|g| g.since < s.sampled_at)
            .map_or(0, |g| u64::from(g.demand.ram_mib));
        available = available.min(
            s.additional_mib
                .saturating_add(released)
                .saturating_sub(new_mib),
        );
    }
    if u64::from(demand.ram_mib) > available {
        return Err(io::Error::other(format!(
            "启动需要 {} MiB，宿主当前预计可分配 {available} MiB",
            demand.ram_mib
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
