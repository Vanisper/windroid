//! 模拟器写入两个独立管道。数据路径与宿主的 wait/信号、运行锁及额度租约分开。
mod child;

use crate::app_log;
use log::Level;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, PipeReader, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SWITCH: &str = "--windroid-log-collector";
static PROGRAM: OnceLock<PathBuf> = OnceLock::new();

/// 独立嵌入 runtime 的客户端可提供实现同一分派入口的可执行文件。
pub fn register_collector(executable: &Path) {
    PROGRAM.get_or_init(|| executable.to_owned());
}

/// 在主程序打开窗口及工作区前分派采集子进程；普通调用登记本程序供随后启动使用。
pub fn collector_main() -> Option<io::Result<()>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(SWITCH.as_ref()) {
        if let Ok(exe) = std::env::current_exe() {
            register_collector(&exe);
        }
        return None;
    }
    Some((|| {
        let arg = |v: Option<std::ffi::OsString>| {
            v.and_then(|s| s.into_string().ok())
                .ok_or_else(|| super::invalid("采集参数无效"))
        };
        let home = args
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| super::invalid("缺少日志主目录"))?;
        let id = arg(args.next())?;
        let run = arg(args.next())?;
        let fd: RawFd = arg(args.next())?
            .parse()
            .map_err(|_| super::invalid("采集通道无效"))?;
        let app = args.next().map(|s| arg(Some(s))).transpose()?;
        child::serve(&home, &id, app.as_deref(), &run, fd)
    })())
}

#[derive(Serialize, Deserialize)]
enum Update {
    Started(u32),
    Finished(String),
}
#[derive(Serialize, Deserialize)]
enum Notice {
    Ready,
    Degraded,
    Error(String),
}

// 控制连接只携带少量生命周期信息；长度前缀防止异常子进程使宿主无限分配。
fn send(mut socket: &UnixStream, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    socket.write_all(&(bytes.len() as u32).to_be_bytes())?;
    socket.write_all(&bytes)
}
fn receive<T: DeserializeOwned>(mut socket: &UnixStream) -> io::Result<T> {
    let mut length = [0; 4];
    socket.read_exact(&mut length)?;
    let len = u32::from_be_bytes(length) as usize;
    if len > 64 * 1024 {
        return Err(super::invalid("采集控制消息过长"));
    }
    let mut bytes = vec![0; len];
    socket.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Clone)]
struct Identity {
    home: PathBuf,
    instance: String,
    run: Option<String>,
}

impl Identity {
    fn record(&self, level: Level, message: &str) {
        app_log::record(
            &self.home,
            level,
            "emulator",
            Some(&self.instance),
            self.run.as_deref(),
            message,
        );
    }

    fn issue(&self, message: String) {
        self.record(Level::Warn, &message);
        app_log::set_capture_issue(&self.home, &self.instance, Some(message));
    }
}

/// 宿主独占子进程句柄。监控线程在采集器断开时立即接读备用管道，直到真正的模拟器退出。
struct Capture {
    collector: Child,
    control: UnixStream,
    monitor: JoinHandle<()>,
    closing: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl Capture {
    fn start(command: &mut Command, id: &Identity, run: &str) -> io::Result<Self> {
        let executable = PROGRAM
            .get()
            .ok_or_else(|| io::Error::other("客户端未登记采集进程入口"))?;
        let (out, out_write) = io::pipe()?;
        let (err, err_write) = io::pipe()?;
        let (control, peer) = UnixStream::pair()?;
        let observer = control.try_clone()?;
        let mut helper = Command::new(executable);
        let control_fd = peer.as_raw_fd();
        helper
            .arg(SWITCH)
            .arg(&id.home)
            .arg(&id.instance)
            .arg(run)
            .arg(control_fd.to_string())
            .args(app_log::current_id(&id.home))
            .process_group(0)
            .stdin(out.try_clone()?)
            .stdout(err.try_clone()?)
            .stderr(Stdio::inherit());
        // 只在子进程取消该端的 CLOEXEC；模拟器与其他并发启动的进程不会继承控制端。
        unsafe {
            helper.pre_exec(move || {
                if libc::fcntl(control_fd, libc::F_SETFD, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut collector = helper.spawn()?;
        drop(peer);
        drop(helper);
        let ready = || {
            control.set_read_timeout(Some(Duration::from_secs(5)))?;
            let result = receive(&control)?;
            control.set_read_timeout(None)?;
            control.set_write_timeout(Some(Duration::from_secs(1)))?;
            match result {
                Notice::Ready => Ok(()),
                Notice::Error(reason) => Err(io::Error::other(reason)),
                Notice::Degraded => Err(io::Error::other("采集器尚未就绪")),
            }
        };
        let closing = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let monitor = ready().and_then(|()| {
            let (closing, stopped, id) = (closing.clone(), stopped.clone(), id.clone());
            std::thread::Builder::new()
                .name("emulator-log-watch".into())
                .spawn(move || {
                    while let Ok(notice) = receive::<Notice>(&observer) {
                        match notice {
                            Notice::Degraded => {
                                id.issue("模拟器日志写入降级，部分输出可能有缺口".into())
                            }
                            Notice::Error(reason) => {
                                id.issue(format!("模拟器日志采集失败：{reason}"))
                            }
                            Notice::Ready => (),
                        }
                    }
                    if !closing.load(Ordering::Acquire) {
                        id.issue("模拟器日志采集器意外退出，后续输出存在缺口".into());
                        discard([out, err], &stopped);
                    }
                })
        });
        match monitor {
            Ok(monitor) => {
                command.stdout(out_write).stderr(err_write);
                Ok(Self {
                    collector,
                    control,
                    monitor,
                    closing,
                    stopped,
                })
            }
            Err(e) => {
                let _ = collector.kill();
                let _ = collector.wait();
                Err(e)
            }
        }
    }

    fn finish(mut self, reason: &str) -> io::Result<()> {
        self.closing.store(true, Ordering::Release);
        let delivered = send(&self.control, &Update::Finished(reason.to_owned()));
        let _ = self.control.shutdown(std::net::Shutdown::Write);
        // 异常后代若一直继承输出 fd，不能让一次停止或宿主退出永久等待 EOF。
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            match self.collector.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                other => {
                    let _ = self.collector.kill();
                    let _ = self.collector.wait();
                    break other.and(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "等待采集器封存超时",
                    )));
                }
            }
        };
        self.stopped.store(true, Ordering::Release);
        let _ = self.monitor.join();
        delivered?;
        if !status?.success() {
            return Err(io::Error::other("采集器未正常封存"));
        }
        Ok(())
    }
}

fn discard(readers: [PipeReader; 2], stopped: &AtomicBool) {
    // 将 EOF 后的流从 poll 集合移除，另一条静默管道仍能等待；定期检查宿主收尾。
    let mut descriptors = readers.each_ref().map(|r| libc::pollfd {
        fd: r.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    });
    let mut bytes = [0u8; 16384];
    while !stopped.load(Ordering::Acquire) && descriptors.iter().any(|d| d.fd >= 0) {
        let rc = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, 200) };
        if rc < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        for (d, mut reader) in descriptors.iter_mut().zip(&readers) {
            if d.revents != 0 {
                match reader.read(&mut bytes) {
                    Ok(n) if n > 0 => (),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                    _ => d.fd = -1,
                }
            }
        }
    }
}

pub(super) struct Attempt {
    id: Identity,
    capture: Option<Capture>,
}

impl Attempt {
    pub(super) fn prepare(home: &Path, instance: &str, command: &mut Command) -> Self {
        let run = windroid_diagnostics::new_run_id();
        let id = Identity {
            home: home.to_owned(),
            instance: instance.to_owned(),
            run: run.as_ref().ok().cloned(),
        };
        app_log::start_attempt(home, instance, id.run.as_deref());
        app_log::set_capture_issue(home, instance, None);
        let capture = match run.and_then(|run| Capture::start(command, &id, &run)) {
            Ok(capture) => Some(capture),
            Err(e) => {
                // 已回收未就绪的采集器，模拟器仍可正常启动。
                command.stdout(Stdio::null()).stderr(Stdio::null());
                id.issue(format!("模拟器日志采集不可用：{e}"));
                None
            }
        };
        Self { id, capture }
    }

    pub(super) fn started(&self, pid: u32) {
        self.id
            .record(Level::Info, &format!("已创建模拟器进程 PID {pid}"));
        if let Some(capture) = &self.capture
            && let Err(e) = send(&capture.control, &Update::Started(pid))
        {
            self.id.issue(format!("未能向采集器发送启动结果：{e}"));
        }
    }
    pub(super) fn finish(mut self, level: Level, reason: String) {
        self.id.record(level, &reason);
        if let Some(capture) = self.capture.take()
            && let Err(e) = capture.finish(&reason)
        {
            self.id.issue(format!("模拟器日志可能有缺口：{e}"));
            // 子进程已回收，原写者锁已释放。用宿主实际 wait 到的结果修复该周期。
            if let Some(run) = &self.id.run
                && let Err(error) = windroid_diagnostics::record_interruption(
                    &self.id.home,
                    &self.id.instance,
                    run,
                    &reason,
                )
            {
                self.id
                    .record(Level::Warn, &format!("无法持久化采集中断结果：{error}"));
            }
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if let Some(mut capture) = self.capture.take() {
            // 宿主无法确认模拟器已退出时，释放控制端，让采集器继续排空到 EOF。
            // 仍存活的宿主负责在采集器最终结束时回收子进程。
            capture.closing.store(true, Ordering::Release);
            let _ = capture.control.shutdown(std::net::Shutdown::Write);
            let _ = std::thread::Builder::new()
                .name("emulator-log-reap".into())
                .spawn(move || {
                    let _ = capture.collector.wait();
                    capture.stopped.store(true, Ordering::Release);
                    let _ = capture.monitor.join();
                });
        }
    }
}
