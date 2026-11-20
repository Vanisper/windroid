//! 官方 adb 在版本不匹配时会终止已有服务。启动前可在空端口创建服务，
//! 后续每个客户端调用前仅查询 host:version，不重启或接管其他服务。
//! https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/client/adb_client.cpp
#[cfg(unix)]
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::process::Command;
#[cfg(unix)]
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use super::{PROBE_TIMEOUT, output_with_timeout};

pub(crate) fn remaining(until: Instant) -> io::Result<Duration> {
    until
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "adb 服务核验超过截止时间"))
}

fn address() -> io::Result<SocketAddr> {
    let port = std::env::var("ANDROID_ADB_SERVER_PORT")
        .ok()
        .map(|s| s.parse::<u16>())
        .transpose()
        .map_err(|_| io::Error::other("本机 adb 服务端口无效"))?
        .unwrap_or(5037);
    if std::env::var("ANDROID_ADB_SERVER_ADDRESS")
        .is_ok_and(|host| host != "localhost" && host != "127.0.0.1")
        || std::env::var("ADB_SERVER_SOCKET").is_ok_and(|s| {
            ![
                format!("tcp:localhost:{port}"),
                format!("tcp:127.0.0.1:{port}"),
                format!("tcp:{port}"),
            ]
            .contains(&s)
        })
    {
        return Err(io::Error::other(
            "所选 adb 服务地址与模拟器使用的本机 TCP 端口不一致",
        ));
    }
    Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
}

fn version(address: SocketAddr, until: Instant) -> io::Result<u32> {
    let mut socket = TcpStream::connect_timeout(&address, remaining(until)?)?;
    socket.set_write_timeout(Some(remaining(until)?))?;
    socket.write_all(b"000chost:version")?;
    let mut reply = [0; 12];
    let mut left = &mut reply[..];
    while !left.is_empty() {
        socket.set_read_timeout(Some(remaining(until)?))?;
        let count = socket.read(left)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "adb 服务提前关闭版本握手",
            ));
        }
        left = &mut left[count..];
    }
    if &reply[..8] != b"OKAY0004" {
        return Err(io::Error::other("本地端口的服务未返回 adb 版本"));
    }
    std::str::from_utf8(&reply[8..])
        .ok()
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .ok_or_else(|| io::Error::other("无法识别 adb 服务版本"))
}

#[cfg(unix)]
#[derive(PartialEq, Eq)]
struct Binary {
    dev: u64,
    ino: u64,
    ctime: i64,
    nsec: i64,
}
#[cfg(unix)]
static PROTOCOLS: LazyLock<Mutex<HashMap<PathBuf, (Binary, u32)>>> =
    LazyLock::new(Default::default);

fn expected(program: &Path, until: Instant) -> io::Result<u32> {
    if !program.is_absolute() {
        return Err(io::Error::other("adb 需要已选版本的绝对路径"));
    }
    // 固定工具跨命令复用其客户端协议；服务端版本每次均从 socket 实读。
    #[cfg(unix)]
    let binary = {
        let meta = std::fs::metadata(program)?;
        let binary = Binary {
            dev: meta.dev(),
            ino: meta.ino(),
            ctime: meta.ctime(),
            nsec: meta.ctime_nsec(),
        };
        if let Some((saved, version)) = PROTOCOLS.lock().unwrap().get(program)
            && *saved == binary
        {
            return Ok(*version);
        }
        binary
    };
    let mut query = Command::new(program);
    query.arg("version");
    let output = output_with_timeout(query, remaining(until)?)?;
    let value = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Android Debug Bridge version 1.0."))
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| io::Error::other("无法读取所选 adb 的协议版本"))?;
    #[cfg(unix)]
    PROTOCOLS
        .lock()
        .unwrap()
        .insert(program.to_owned(), (binary, value));
    Ok(value)
}

fn local_command(program: &Path, address: SocketAddr) -> Command {
    let mut command = Command::new(program);
    // 握手与真实客户端均显式使用此 IPv4 端点，避免 localhost/IPv6 解析到另一项服务。
    command.env("ADB_SERVER_SOCKET", format!("tcp:{address}"));
    command
}

fn checked(program: &Path, until: Instant, may_start: bool) -> io::Result<Command> {
    let address = address()?;
    let expected = expected(program, until)?;
    let actual = match version(address, until) {
        Err(e) if may_start && e.kind() == io::ErrorKind::ConnectionRefused => {
            // adb 只允许在不含显式主机名的本地 socket 上启动 daemon。
            // 让它按本机端口建立服务，再用同一个 IPv4 地址读回协议。
            let mut start = Command::new(program);
            start
                .env("ADB_SERVER_SOCKET", format!("tcp:{}", address.port()))
                .arg("start-server");
            let out = output_with_timeout(start, remaining(until)?)?;
            if !out.status.success() {
                return Err(io::Error::other(format!(
                    "无法启动所选 adb 服务：{}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            version(address, until)?
        }
        other => other?,
    };
    if actual != expected {
        return Err(io::Error::other(format!(
            "本机 adb 服务协议 {actual} 与所选工具 {expected} 不兼容，请先处理已有服务"
        )));
    }
    Ok(local_command(program, address))
}

pub fn ensure_server(program: &Path) -> io::Result<()> {
    checked(program, Instant::now() + PROBE_TIMEOUT, true).map(|_| ())
}

/// 注入的命令用于无 Android 服务的脚本测试。生产设备默认为本地协议握手。
#[derive(Debug, Clone, Copy)]
pub enum Transport {
    Local,
    Injected,
}

impl Transport {
    pub fn command(self, program: &Path, until: Instant) -> io::Result<Command> {
        match self {
            Self::Local => checked(program, until.min(Instant::now() + PROBE_TIMEOUT), false),
            Self::Injected => Ok(Command::new(program)),
        }
    }
}
