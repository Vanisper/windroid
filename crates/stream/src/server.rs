//! 每个 server 独占一份设备端文件和一个由 adb 分配的 forward 端口。

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::{DisplayTarget, packet::invalid};

const COMMAND_LIMIT: Duration = Duration::from_secs(10);
const CLEANUP_LIMIT: Duration = Duration::from_secs(2);

fn command(device: adb::Device<'_>, until: Instant) -> io::Result<Command> {
    let mut cmd = device.command(until)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(cmd)
}

fn wait(child: &mut Child, until: Instant) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= until {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "等待 adb 子进程超时",
            ));
        }
        thread::sleep(Duration::from_millis(15));
    }
}

fn run(
    device: adb::Device<'_>,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    timeout: Duration,
) -> io::Result<String> {
    let until = Instant::now() + timeout;
    let mut cmd = command(device, until)?;
    cmd.args(args);
    let out = adb::output_with_timeout(cmd, until.saturating_duration_since(Instant::now()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if out.status.success() {
        Ok(stdout.trim().to_owned())
    } else {
        Err(io::Error::other(format!(
            "adb {}: {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
            stdout.trim()
        )))
    }
}
/// v5.0.1 将实际显示 ID 输出到本次 adb shell 的 stdout。
/// <https://github.com/Genymobile/scrcpy/blob/v5.0.1/server/src/main/java/com/genymobile/scrcpy/video/NewDisplayCapture.java>
fn parse_display_id(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("[server] INFO: New display: ")?;
    let (_, id) = rest.strip_suffix(')')?.rsplit_once(" (id=")?;
    id.parse().ok().filter(|&id| id != 0)
}

fn log_pipe(
    pipe: impl Read + Send + 'static,
    display: mpsc::Sender<u32>,
    diagnostic: Arc<Mutex<String>>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut reader).take(4096).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                _ => {}
            }
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end();
            if let Some(id) = parse_display_id(line) {
                let _ = display.send(id);
            }
            if !line.trim().is_empty() {
                let mut tail = diagnostic.lock().unwrap();
                tail.push_str(line);
                tail.push('\n');
                if tail.len() > 4096 {
                    let keep = tail.ceil_char_boundary(tail.len() - 4096);
                    tail.drain(..keep);
                }
            }
        }
    });
}

pub(crate) struct Server {
    adb: PathBuf,
    transport: adb::Transport,
    serial: String,
    scid: String,
    dir: String,
    port: Option<u16>,
    forward_requested: bool,
    child: Option<Child>,
    display: mpsc::Receiver<u32>,
    diagnostic: Arc<Mutex<String>>,
}

impl Server {
    pub(crate) fn start(
        device: adb::Device<'_>,
        jar: &Path,
        target: &DisplayTarget,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Self> {
        if !jar.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("找不到 scrcpy-server.jar: {}", jar.display()),
            ));
        }
        let mut entropy = [0; 4];
        File::open("/dev/urandom")?.read_exact(&mut entropy)?;
        let scid = format!("{:08x}", u32::from_ne_bytes(entropy) & 0x7fff_ffff);
        let dir = format!("/data/local/tmp/windroid-scrcpy-{scid}");
        let (display_tx, display) = mpsc::channel();
        // mkdir 是设备上该 ID 的独占操作，失败时不得接手或删除原目录。
        run(device, ["shell", "mkdir", "-m", "700", &dir], COMMAND_LIMIT)?;
        let mut me = Self {
            adb: device.program.to_owned(),
            serial: device.serial.to_owned(),
            transport: device.transport,
            scid,
            dir,
            port: None,
            forward_requested: false,
            child: None,
            display,
            diagnostic: Arc::default(),
        };
        crate::check_cancel(cancelled)?;
        let remote = format!("{}/server.jar", me.dir);
        run(
            device,
            [OsStr::new("push"), jar.as_os_str(), OsStr::new(&remote)],
            COMMAND_LIMIT,
        )?;
        crate::check_cancel(cancelled)?;
        let socket = me.socket();
        me.forward_requested = true;
        let port = run(
            device,
            ["forward", "--no-rebind", "tcp:0", &socket],
            COMMAND_LIMIT,
        )?;
        me.port = Some(
            port.parse::<u16>()
                .ok()
                .filter(|n| *n != 0)
                .ok_or_else(|| invalid("adb 未返回可用的 forward 端口"))?,
        );
        // shell 只插入本模块生成的 8 位十六进制 ID/路径，包名仅进入二进制控制通道。
        let mode = match target {
            DisplayTarget::Device => "display_id=0",
            DisplayTarget::App { .. } => {
                "new_display=720x1280/240 vd_destroy_content=false vd_system_decorations=false display_ime_policy=local flex_display=true"
            }
        };
        let launch = format!(
            "echo $$ > {}/pid && export CLASSPATH={remote} && exec app_process / com.genymobile.scrcpy.Server {} scid={} tunnel_forward=true log_level=info audio=false control=true video_codec=h264 max_size=1920 max_fps=60 clipboard_autosync=false cleanup=false {mode}",
            me.dir,
            crate::SERVER_VERSION,
            me.scid
        );
        crate::check_cancel(cancelled)?;
        let mut child = command(device, Instant::now() + COMMAND_LIMIT)?
            .args(["shell", "-T", &launch])
            .spawn()?;
        log_pipe(
            child.stdout.take().unwrap(),
            display_tx.clone(),
            Arc::clone(&me.diagnostic),
        );
        log_pipe(
            child.stderr.take().unwrap(),
            display_tx,
            Arc::clone(&me.diagnostic),
        );
        me.child = Some(child);
        Ok(me)
    }

    fn device(&self) -> adb::Device<'_> {
        adb::Device {
            program: &self.adb,
            serial: &self.serial,
            transport: self.transport,
        }
    }

    pub(crate) fn port(&self) -> u16 {
        self.port.expect("server forward")
    }
    fn socket(&self) -> String {
        format!("localabstract:scrcpy_{}", self.scid)
    }

    pub(crate) fn check_child(&mut self) -> io::Result<()> {
        if let Some(status) = self.child.as_mut().unwrap().try_wait()? {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("scrcpy-server 已退出 ({status})"),
            ));
        }
        Ok(())
    }

    pub(crate) fn context(&self, e: io::Error) -> io::Error {
        let tail = self.diagnostic.lock().unwrap();
        if tail.trim().is_empty() {
            e
        } else {
            io::Error::new(e.kind(), format!("{e} · {}", tail.trim()))
        }
    }

    pub(crate) fn display_id(
        &self,
        until: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<u32> {
        loop {
            crate::check_cancel(cancelled)?;
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "等候虚拟显示 ID 超时",
                ));
            }
            match self
                .display
                .recv_timeout(left.min(Duration::from_millis(150)))
            {
                Ok(id) => return Ok(id),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "取得虚拟显示 ID 前 server 已退出",
                    ));
                }
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // tcp 端口可能已被别人重新分配。只有 (serial, tcp, scid) 仍匹配才撤销。
        let socket = self.socket();
        if self.forward_requested
            && let Ok(list) = run(self.device(), ["forward", "--list"], CLEANUP_LIMIT)
        {
            // 返回端口前 adb 超时也可能已经建立映射。用私有 scid 找回它。
            for line in list.lines() {
                let mut fields = line.split_whitespace();
                if let (Some(serial), Some(local), Some(remote), None) =
                    (fields.next(), fields.next(), fields.next(), fields.next())
                    && serial == self.serial
                    && remote == socket
                    && let Some(port) = local
                        .strip_prefix("tcp:")
                        .and_then(|s| s.parse::<u16>().ok())
                    && self.port.is_none_or(|known| known == port)
                {
                    let _ = run(self.device(), ["forward", "--remove", local], CLEANUP_LIMIT);
                }
            }
        }
        // 握手中断时远端可能仍在 accept；pid 文件和进程的私有 CLASSPATH 同时匹配才发 TERM。
        let dir = &self.dir;
        let clean = format!(
            r#"p=$(cat {dir}/pid 2>/dev/null); case "$p" in ''|*[!0-9]*) ;; *) if [ "$p" -gt 1 ] && tr '\000' '\n' < /proc/$p/environ 2>/dev/null | grep -Fxq 'CLASSPATH={dir}/server.jar'; then kill -TERM "$p" 2>/dev/null; fi ;; esac; rm -f {dir}/server.jar {dir}/pid && rmdir {dir}"#
        );
        if let Some(child) = &mut self.child {
            let _ = wait(child, Instant::now() + Duration::from_millis(300));
        }
        let _ = run(self.device(), ["shell", &clean], CLEANUP_LIMIT);
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
