//! adb 命令行封装。
//!
//! 这一层只做两件事：把命令发出去、把输出原样取回来。它**不判断成功**——
//! adb 的退出码与 `Success` 文本都不是证据（见 docs/架构/02-设备域.md 第七节）：
//! `adb install` 在磁盘满时退出 0，`am start` 失败也退出 0。
//! 判断留给调用方，通过回读实际状态完成。

use std::env;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT_PER_STREAM: usize = 8 * 1024 * 1024;

/// 所有 adb 客户端（包括流域的长驻进程）使用同一路径。
/// 显式覆盖优先于 SDK，默认使用 Windroid 私有 SDK。
pub fn program() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("WINDROID_ADB").filter(|value| !value.is_empty()) {
        return Ok(path.into());
    }
    let exe = format!("adb{}", env::consts::EXE_SUFFIX);
    if let Some(sdk) = env::var_os("WINDROID_SDK").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(sdk).join("platform-tools").join(exe));
    }
    let tool = PathBuf::from("runtime/sdk/platform-tools").join(&exe);
    if let Some(home) = env::var_os("WINDROID_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join(tool));
    }
    let home = env::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "无法确定 Windroid 运行时目录"))?;
    let managed = home.join(".windroid").join(tool);
    // 单独指定 serial 是现有的只读 real_catalog 手工入口。在有私有 SDK 时仍优先用它。
    if !managed.exists() && env::var_os("WINDROID_SERIAL").is_some_and(|s| !s.is_empty()) {
        return Ok(exe.into());
    }
    Ok(managed)
}

/// 短命令的输出截止时间覆盖进程退出和两个输出流的 EOF。超时会终止并回收直接子进程。
/// cmd 的参数、工作目录和环境保持原样。stdin 关闭，stdout 与 stderr 分别最多捕获 8 MiB。
/// 任一流超限就回收子进程并报告 InvalidData，调用方只会收到完整输出。
pub fn output_with_timeout(mut cmd: Command, timeout: Duration) -> io::Result<Output> {
    let start = Instant::now();
    let (mut stdout, out_writer) = capture::channel()?;
    let (mut stderr, err_writer) = capture::channel()?;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(out_writer)
        .stderr(err_writer)
        .spawn()?;
    drop(cmd); // Command 自身也持有写端，留着它就收不到 EOF。
    let result = collect_output(&mut child, &mut stdout, &mut stderr, start, timeout);
    if result.is_err() {
        let _ = child.kill();
        child.wait()?;
    }
    result
}

fn collect_output(
    child: &mut Child,
    stdout: &mut impl Read,
    stderr: &mut impl Read,
    start: Instant,
    timeout: Duration,
) -> io::Result<Output> {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let (mut out_eof, mut err_eof) = (false, false);
    let mut status = None;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let left = timeout.saturating_sub(start.elapsed());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("命令超过 {timeout:?} 截止时间"),
            ));
        }
        // 每轮各读一块。任一流持续写入，也要给另一条流和截止时间留检查机会。
        let out_read = read_chunk(stdout, "stdout", &mut out, &mut out_eof, &mut buf)?;
        let err_read = read_chunk(stderr, "stderr", &mut err, &mut err_eof, &mut buf)?;
        if status.is_none() {
            status = child.try_wait()?;
        }
        if let Some(status) = status
            && out_eof
            && err_eof
        {
            return Ok(Output {
                status,
                stdout: out,
                stderr: err,
            });
        }
        if !out_read && !err_read {
            std::thread::sleep(left.min(Duration::from_millis(2)));
        }
    }
}

fn read_chunk(
    reader: &mut impl Read,
    name: &str,
    bytes: &mut Vec<u8>,
    eof: &mut bool,
    buf: &mut [u8],
) -> io::Result<bool> {
    if *eof {
        return Ok(false);
    }
    match reader.read(buf) {
        Ok(0) => {
            *eof = true;
            Ok(false)
        }
        Ok(n) if n > MAX_OUTPUT_PER_STREAM - bytes.len() => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("命令 {name} 超过 {MAX_OUTPUT_PER_STREAM} 字节捕获上限"),
        )),
        Ok(n) => {
            bytes.extend_from_slice(&buf[..n]);
            Ok(true)
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

// 非阻塞读端让截止时间也覆盖「子进程已退出，后代仍持有写端」的情形。
// 这样超时后直接关闭读端，无需留下等待 EOF 的读线程。
#[cfg(unix)]
mod capture {
    use super::*;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    pub(super) fn channel() -> io::Result<(impl Read, Stdio)> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        Ok((reader, Stdio::from(OwnedFd::from(writer))))
    }
}

#[cfg(windows)]
mod capture {
    use super::*;
    use std::ffi::c_void;
    use std::io::PipeReader;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn PeekNamedPipe(
            pipe: *mut c_void,
            buf: *mut c_void,
            len: u32,
            read: *mut u32,
            available: *mut u32,
            remaining: *mut u32,
        ) -> i32;
    }

    struct Reader(PipeReader);

    impl Read for Reader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut available = 0;
            // SAFETY: 读端在本次调用期间有效，available 是唯一的输出指针。
            let ok = unsafe {
                PeekNamedPipe(
                    self.0.as_raw_handle(),
                    null_mut(),
                    0,
                    null_mut(),
                    &mut available,
                    null_mut(),
                )
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                return if e.kind() == io::ErrorKind::BrokenPipe {
                    Ok(0)
                } else {
                    Err(e)
                };
            }
            if available == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = buf.len().min(available as usize);
            self.0.read(&mut buf[..n])
        }
    }

    pub(super) fn channel() -> io::Result<(impl Read, Stdio)> {
        let (reader, writer) = io::pipe()?;
        Ok((Reader(reader), writer.into()))
    }
}

/// 一条 adb 调用的结果。原样保留 stdout/stderr，不在这里解释。
#[derive(Debug)]
pub struct AdbOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
}

impl AdbOutput {
    /// 某行的存在与否，而不是退出码。
    ///
    /// 这是本项目读取 adb 结果的唯一正确姿势：找证据，不看结论。
    pub fn has(&self, needle: &str) -> bool {
        self.stdout.contains(needle) || self.stderr.contains(needle)
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.stdout.lines().map(str::trim).filter(|l| !l.is_empty())
    }
}

/// 对某台设备执行 adb 子命令。安装与文件传输有独立的较长预算。
pub fn run(serial: &str, args: &[&str]) -> io::Result<AdbOutput> {
    let timeout = match args.first().copied() {
        Some("install" | "install-multiple" | "uninstall" | "push" | "pull") => TRANSFER_TIMEOUT,
        _ => PROBE_TIMEOUT,
    };
    run_with_timeout(serial, args, timeout)
}

/// 对某台设备执行有明确时间预算的 adb 子命令。
pub fn run_with_timeout(serial: &str, args: &[&str], timeout: Duration) -> io::Result<AdbOutput> {
    let mut cmd = Command::new(program()?);
    cmd.args(["-s", serial]).args(args);
    let out = output_with_timeout(cmd, timeout)?;
    Ok(AdbOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
    })
}

/// 执行 adb shell 命令。
pub fn shell(serial: &str, cmd: &str) -> io::Result<AdbOutput> {
    run(serial, &["shell", cmd])
}

/// 当前所有设备的 serial 列表，附带连接状态。
///
/// 只解析 `adb devices` 的正文：首行是标题，其余每行 `serial\tstate`。
pub fn devices() -> io::Result<Vec<(String, String)>> {
    let mut cmd = Command::new(program()?);
    cmd.arg("devices");
    let out = output_with_timeout(cmd, PROBE_TIMEOUT)?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let serial = it.next()?;
            let state = it.next().unwrap_or("unknown");
            Some((serial.to_owned(), state.to_owned()))
        })
        .collect())
}

/// 该 serial 是否已连接且状态为 `device`。
pub fn is_ready(serial: &str) -> io::Result<bool> {
    Ok(devices()?
        .into_iter()
        .any(|(s, state)| s == serial && state == "device"))
}

/// 建立端口转发，返回实际使用的端口。
pub fn forward(serial: &str, local_port: u16, remote: &str) -> io::Result<AdbOutput> {
    run(serial, &["forward", &format!("tcp:{local_port}"), remote])
}

/// 撤销端口转发。
pub fn remove_forward(serial: &str, local_port: u16) -> io::Result<AdbOutput> {
    run(
        serial,
        &["forward", "--remove", &format!("tcp:{local_port}")],
    )
}

/// 推送文件到设备。
pub fn push(serial: &str, local: &str, remote: &str) -> io::Result<AdbOutput> {
    run(serial, &["push", local, remote])
}
