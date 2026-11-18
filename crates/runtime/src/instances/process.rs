//! 持久化 PID 只用来阻止误删和重复启动。停止仅作用于本次 spawn 的子进程。
use std::path::Path;
use std::{fs, io};

use super::{InstanceRecord, InstanceStore, data_dir, image_relative, invalid};
use crate::provision::RuntimePaths;
use contract::InstanceId;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) fn ensure_previous_exit(dir: &Path) -> io::Result<()> {
    let marker = dir.join("active.pid");
    match fs::read_to_string(&marker) {
        Ok(value) => {
            let pid: i32 = value
                .trim()
                .parse()
                .map_err(|_| super::invalid("实例运行记录无效"))?;
            if pid <= 0 {
                return Err(super::invalid("实例进程号无效"));
            }
            // 信号 0 仅探测，重用的 PID 不会收到停止信号。
            if unsafe { libc::kill(pid, 0) } == 0
                || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("实例上次启动的进程 {pid} 尚未确认退出，请先关闭它"),
                ));
            }
            fs::remove_file(marker)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub(crate) struct OwnedEmulator {
    child: Child,
    pid_file: PathBuf,
    _lease: File,
    pub record: InstanceRecord,
}

impl OwnedEmulator {
    pub fn start(
        store: &InstanceStore,
        id: &InstanceId,
        port: u16,
        paths: &RuntimePaths,
    ) -> io::Result<Self> {
        let (record, dir, lease) = store.lease(id, port)?;
        let sdk = paths.sdk.canonicalize().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Android SDK {}：{e}", paths.sdk.display()),
            )
        })?;
        let image = sdk.join(image_relative(&record.spec)?);
        for part in [
            "system.img",
            "vendor.img",
            "ramdisk.img",
            "kernel-ranchu",
            "userdata.img",
        ] {
            if !image.join(part).is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("系统镜像缺少 {}", image.join(part).display()),
                ));
            }
        }
        let data = data_dir(&dir, &record);
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
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("emulator.log"))?;
        let mut cmd = Command::new(&paths.emulator);
        // 共享只读 SDK 和 adb server 的授权上下文。AVD 搜索、快照和数据盘显式指向实例目录。
        // https://developer.android.com/studio/run/emulator-commandline#data-filedir
        cmd.env("ANDROID_HOME", &sdk)
            .env("ANDROID_SDK_ROOT", &sdk)
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
            .args(["-no-window", "-no-audio", "-no-boot-anim", "-gpu"])
            .arg(std::env::var_os("WINDROID_GPU").unwrap_or_else(|| "host".into()))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        super::quick_boot::clear(&dir)?;
        let pid_file = dir.join("active.pid");
        let mut pid = File::create_new(&pid_file)?;
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                fs::remove_file(&pid_file)?;
                return Err(io::Error::new(
                    e.kind(),
                    format!("无法启动 {}：{e}", paths.emulator.display()),
                ));
            }
        };
        let owned = Self {
            child,
            pid_file,
            _lease: lease,
            record,
        };
        // 先构造 RAII owner，写入 PID 失败也能回收刚启动的进程组。
        writeln!(pid, "{}", owned.child.id())?;
        pid.sync_all()?;
        Ok(owned)
    }

    pub fn alive(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_none())
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

    pub fn stop(&mut self, graceful: bool) -> io::Result<()> {
        if graceful && self.alive()? {
            // TERM 触发模拟器的正常退出与 Quick Boot 保存，参见 docs/实测/02-设备域.md 第七节。
            let dir = self.pid_file.parent().unwrap().to_owned();
            let before = super::quick_boot::stamp(&dir, &self.record);
            self.signal(libc::SIGTERM)?;
            if self.await_exit(Duration::from_secs(20))? {
                if let Err(e) = before
                    .and_then(|stamp| super::quick_boot::record_save(&dir, &self.record, stamp))
                {
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
        match self.stop(false) {
            Ok(()) => {
                let _ = fs::remove_file(&self.pid_file);
            }
            Err(e) => log::warn!("回收实例 {} 时无法确认进程退出：{e}", self.record.id),
        }
    }
}
