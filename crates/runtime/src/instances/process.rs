//! 持久化 PID 只用来阻止误删和重复启动。停止仅作用于本次 spawn 的子进程。
use std::path::Path;
use std::{fs, io};

use super::{Hardware, HostCapacity};
use super::{InstanceRecord, InstanceStore, data_dir, image_subdir, invalid};
use crate::SessionResources;
use crate::admission::{Budget, Demand, Reservation};
use contract::{GuestAllocation, InstanceId};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// 旧客户端可能只留下 active.pid；仅探测是否仍存活，不能据此接管进程。
pub(crate) fn live_marker(dir: &Path) -> io::Result<Option<i32>> {
    use std::io::Read;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("active.pid"))
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() || file.metadata()?.len() > 32 {
        return Err(super::invalid("实例运行记录无效"));
    }
    let mut value = String::new();
    file.take(33).read_to_string(&mut value)?;
    let pid = value
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| super::invalid("实例运行记录的 PID 无效"))?;
    Ok((unsafe { libc::kill(pid, 0) } == 0
        || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH))
    .then_some(pid))
}

pub(super) fn check_previous_exit(dir: &Path) -> io::Result<()> {
    if let Some(pid) = live_marker(dir)? {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("实例上次启动的进程 {pid} 尚未确认退出，请先关闭它"),
        ));
    }
    Ok(())
}

pub(crate) fn ensure_previous_exit(dir: &Path) -> io::Result<()> {
    check_previous_exit(dir)?;
    match fs::remove_file(dir.join("active.pid")) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopMode {
    Graceful,
    Restart { cold: bool },
    Force,
}

pub(crate) struct OwnedEmulator {
    child: Child,
    launched: SystemTime,
    pid_file: PathBuf,
    _lease: File,
    reservation: Reservation,
    pub resources: Arc<SessionResources>,
    pub record: InstanceRecord,
}

pub(super) fn launch_requirements(
    store: &InstanceStore,
    record: &InstanceRecord,
    paths: &crate::provision::RuntimePaths,
) -> io::Result<(Hardware, Demand, Budget)> {
    let host = HostCapacity::read()?;
    let hardware = store.launch_hardware_at(record, paths, host)?;
    let demand = Demand {
        cores: hardware.cpu_cores,
        ram_mib: hardware.ram_mib,
    };
    let budget = Budget {
        logical_cores: host.logical_cores,
        physical_mib: host.physical_mib,
    };
    Ok((hardware, demand, budget))
}

pub(super) fn launch_paths(
    dir: &Path,
    record: &InstanceRecord,
    paths: &crate::provision::RuntimePaths,
) -> io::Result<(PathBuf, PathBuf)> {
    let image = paths.images.join(image_subdir(&record.spec.image)?);
    for part in ["system.img", "vendor.img", "ramdisk.img", "kernel-ranchu"] {
        if !image.join(part).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("系统镜像缺少 {}", image.join(part).display()),
            ));
        }
    }
    let data = data_dir(dir, record);
    let pointer = dir.join("avd").join(format!("{}.ini", record.avd_name()));
    if data.canonicalize()? != data
        || pointer.canonicalize()? != pointer
        || !data.join("config.ini").is_file()
    {
        return Err(invalid("无法确认实例私有 AVD 目录"));
    }
    let text = fs::read_to_string(pointer)?;
    let mut actual = text.lines().filter_map(|line| line.strip_prefix("path="));
    if actual.next().map(Path::new) != Some(data.as_path()) || actual.next().is_some() {
        return Err(invalid("AVD 指针与该实例的数据目录不符"));
    }
    for entry in fs::read_dir(&data)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() {
            return Err(invalid(format!(
                "私有 AVD 中存在外部链接 {}",
                entry.path().display()
            )));
        }
    }
    Ok((image, data))
}

impl OwnedEmulator {
    pub fn start(
        store: &InstanceStore,
        id: &InstanceId,
        port: u16,
        active: &crate::ActiveRuntime,
        cold: bool,
    ) -> io::Result<Self> {
        let (record, dir, lease) = store.lease(id, port)?;
        let resources = active.acquire(store.home(), &record.spec.image)?;
        let paths = &resources.paths;
        let (image, data) = launch_paths(&dir, &record, paths)?;
        let skip_snapshot = cold || super::quick_boot::requires_cold_start(&dir, &record)?;
        if skip_snapshot {
            super::quick_boot::allow_skip(&dir, &record)?;
        }
        let (hardware, demand, budget) = launch_requirements(store, &record, paths)?;
        adb::ensure_server(&paths.adb)?;
        // adb 服务启动可能等待握手。完成后才读取宿主状态、取得额度并启动子进程。
        let mut reservation =
            active
                .admission
                .reserve(port, demand, budget, Some((store, &dir)))?;
        super::avd::sync_image(&data, &image)?;
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("emulator.log"))?;
        let mut cmd = Command::new(&paths.emulator);
        // AVD 搜索、快照和数据盘显式指向实例目录。
        // https://developer.android.com/studio/run/emulator-commandline#data-filedir
        // 受管 adb 服务已经启动。不给原生模拟器提供可绕过会话协议检查的 adb 客户端；
        // 它使用配置的端口注册传输，设备查询和文件操作由 Windroid 绑定的工具执行。
        cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env_remove("ANDROID_HOME")
            .env_remove("ANDROID_SDK_ROOT")
            .env("ANDROID_EMULATOR_HOME", &dir)
            .env("ANDROID_AVD_HOME", dir.join("avd"))
            .arg("-avd")
            .arg(record.avd_name())
            .arg("-port")
            .arg(port.to_string())
            .arg("-sysdir")
            .arg(&image)
            .arg("-datadir")
            .arg(&data)
            .arg("-data")
            .arg(data.join("userdata-qemu.img"))
            .args(["-no-window", "-no-audio", "-no-boot-anim"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        hardware.graphics.apply(&mut cmd);
        if skip_snapshot {
            cmd.arg("-no-snapshot-load");
        }
        // 版本锁与额度预留随 QEMU 子进程继续有效，包括宿主异常退出后模拟器仍在运行的情形。
        // 在 fork 后仅修改子进程自己的 fd 标志。其他子进程沿用 CLOEXEC 默认值。
        let inherited_fds: Vec<_> = resources
            ._leases
            .iter()
            .chain([&reservation.file, &lease])
            .map(AsRawFd::as_raw_fd)
            .collect();
        unsafe {
            cmd.pre_exec(move || {
                for fd in &inherited_fds {
                    if libc::fcntl(*fd, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        super::quick_boot::clear(&dir)?;
        let pid_file = dir.join("active.pid");
        let launched = SystemTime::now();
        let child = cmd.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("无法启动 {}：{e}", paths.emulator.display()),
            )
        })?;
        reservation.child_inherited();
        let owned = Self {
            child,
            launched,
            pid_file,
            _lease: lease,
            reservation,
            resources,
            record,
        };
        // 子进程已继承 run.lock。即使宿主在 spawn 与发布 PID 之间退出，也受运行锁保护。
        // 使用同目录暂存文件，下一次打开只能看到完整 PID 或尚未发布的状态。
        let temporary = dir.join(format!(".pid-{}", super::nonce()?));
        let result = (|| {
            let mut file = File::create_new(&temporary)?;
            writeln!(file, "{}", owned.child.id())?;
            file.sync_all()?;
            fs::rename(&temporary, &owned.pid_file)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(temporary);
            return Err(e);
        }
        Ok(owned)
    }

    pub fn check_replacement(
        &self,
        store: &InstanceStore,
        active: &crate::ActiveRuntime,
    ) -> io::Result<()> {
        // 此时原模拟器及其版本租约仍存活。未通过的预检不会发送停止信号或清理窗口。
        let record = store.get(&self.record.id)?;
        let resources = active.resolve_versions(store.home(), &record.spec.image)?;
        let dir = self.pid_file.parent().unwrap();
        crate::workspace::access(&resources.paths.emulator, libc::X_OK)?;
        launch_paths(dir, &record, &resources.paths)?;
        super::quick_boot::requires_cold_start(dir, &record)?;
        let (_, demand, budget) = launch_requirements(store, &record, &resources.paths)?;
        adb::ensure_server(&resources.paths.adb)?;
        active
            .admission
            .check_replacement(&self.reservation, demand, budget, Some(store))
    }

    pub fn alive(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }

    pub fn allocation(
        &mut self,
        admission: &crate::admission::Admission,
    ) -> Option<GuestAllocation> {
        let avd = data_dir(self.pid_file.parent()?, &self.record);
        let actual = super::hardware::allocation(&avd.join("hardware-qemu.ini"), self.launched)?;
        if let Err(e) = admission.reconcile(&mut self.reservation, actual) {
            log::warn!("无法更新实例 {} 的活动规格：{e}", self.record.id);
            return None;
        }
        Some(actual)
    }

    // 未回收且仍存活的子进程持有 PID；要求它是我们新建的进程组组长。
    fn signal(&mut self, signal: libc::c_int) -> io::Result<()> {
        if !self.alive()? {
            return Ok(());
        }
        let pid = self.child.id() as libc::pid_t;
        if unsafe { libc::getpgid(pid) } != pid {
            return Err(invalid("无法确认模拟器进程组归属"));
        }
        if unsafe { libc::kill(-pid, signal) } < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::ESRCH) {
                return Err(e);
            }
        }
        Ok(())
    }

    fn await_exit(&mut self, budget: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + budget;
        while self.alive()? {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        Ok(true)
    }

    /// SIGTERM 保存 Quick Boot 的客户机内存。另行让已挂载的 /data 刷盘，才能在随后的
    /// 冷启动中找回刚写入的字节。adb 每次探测和 shell 均有截止时间。
    fn sync_guest_disk(&self) -> io::Result<bool> {
        if self
            .record
            .verify_device(&self.resources.paths.adb)
            .is_err()
        {
            return Ok(false);
        }
        // 将身份复核与 sync 放在同一远端 shell 中，即使 serial 被重用也不触碰其他设备。
        let cmd = format!(
            r#"if [ "$(getprop ro.boot.qemu.avd_name)" != '{}' ]; then exit 61; fi
grep -q ' /data ' /proc/mounts || exit 62
sync && echo windroid-disk-synced"#,
            self.record.avd_name()
        );
        let serial = self.record.serial();
        let output = adb::shell(self.resources.device(&serial), &cmd)
            .map_err(|e| io::Error::other(format!("无法确认 Android 用户分区刷盘：{e}")))?;
        match output.code {
            Some(0) if output.stdout.trim() == "windroid-disk-synced" => Ok(true),
            Some(61 | 62) => Ok(false),
            _ => Err(io::Error::other(format!(
                "Android 用户分区刷盘失败（{:?} {}）",
                output.code,
                output.stderr.trim()
            ))),
        }
    }

    pub fn stop(&mut self, mode: StopMode) -> io::Result<()> {
        if mode != StopMode::Force && self.alive()? {
            // TERM 触发模拟器的正常退出与 Quick Boot 保存，参见 docs/实测/02-设备域.md 第七节。
            let dir = self.pid_file.parent().unwrap().to_owned();
            let needs_sync = if let StopMode::Restart { cold } = mode {
                cold || super::quick_boot::requires_cold_start(&dir, &self.record)?
            } else {
                false
            };
            let synced = match self.sync_guest_disk() {
                Ok(synced) => synced,
                Err(e) => {
                    // 普通退出仍能保存内存快照；缺少刷盘凭证时稍后不能直接跳过快照。
                    log::warn!("实例 {} 未确认 Android 用户分区刷盘：{e}", self.record.id);
                    false
                }
            };
            if needs_sync && !synced {
                return Err(io::Error::other(
                    "尚未确认 Android 用户分区刷盘，已保留当前会话。请等设备就绪后再冷启动。",
                ));
            }
            let before = super::quick_boot::stamp(&dir, &self.record);
            self.signal(libc::SIGTERM)?;
            if self.await_exit(Duration::from_secs(20))? {
                if let Err(e) = before.and_then(|stamp| {
                    super::quick_boot::record_save(&dir, &self.record, stamp, synced)
                }) {
                    log::warn!("无法确认实例 {} 的快照：{e}", self.record.id);
                }
                return Ok(());
            }
        }
        self.signal(libc::SIGKILL)?;
        if !self.await_exit(Duration::from_secs(2))? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "模拟器进程尚未退出",
            ));
        }
        Ok(())
    }
}

impl Drop for OwnedEmulator {
    fn drop(&mut self) {
        match self.stop(StopMode::Force) {
            Ok(()) => {
                let _ = fs::remove_file(&self.pid_file);
                let _ = self.reservation.file.unlock();
            }
            Err(e) => log::warn!("回收实例 {} 时无法确认进程退出：{e}", self.record.id),
        }
    }
}
