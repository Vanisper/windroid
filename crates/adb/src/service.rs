//! 在调用可能启动服务的 adb 客户端前，直接向本机服务查询协议版本。
//! Android 的客户端在版本不匹配时会杀掉原服务，因此先做无副作用的握手。
//! https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/client/adb_client.cpp
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;

use super::{PROBE_TIMEOUT, output_with_timeout};

fn version(address: SocketAddr) -> io::Result<u32> {
    let mut socket = TcpStream::connect_timeout(&address, PROBE_TIMEOUT)?;
    socket.set_read_timeout(Some(PROBE_TIMEOUT))?;
    socket.set_write_timeout(Some(PROBE_TIMEOUT))?;
    socket.write_all(b"000chost:version")?;
    let mut response = [0; 12];
    socket.read_exact(&mut response)?;
    if &response[..8] != b"OKAY0004" {
        return Err(io::Error::other("本地端口的服务未返回 adb 版本"));
    }
    std::str::from_utf8(&response[8..])
        .ok()
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .ok_or_else(|| io::Error::other("无法识别 adb 服务版本"))
}

pub fn ensure_server(program: &Path) -> io::Result<()> {
    // 模拟器与 Windroid 的各客户端使用同一本机 TCP 服务，避免误连远程设备。
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
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut query = Command::new(program);
    query.arg("version");
    let output = output_with_timeout(query, PROBE_TIMEOUT)?;
    let expected: u32 = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Android Debug Bridge version 1.0."))
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| io::Error::other("无法读取所选 adb 的协议版本"))?;
    let actual = match version(address) {
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            let mut start = Command::new(program);
            start.arg("start-server");
            let out = output_with_timeout(start, PROBE_TIMEOUT)?;
            if !out.status.success() {
                return Err(io::Error::other(format!(
                    "无法启动所选 adb 服务：{}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            version(address)?
        }
        other => other?,
    };
    if actual != expected {
        return Err(io::Error::other(format!(
            "本机 adb 服务协议 {actual} 与所选工具 {expected} 不兼容，请先处理已有服务"
        )));
    }
    Ok(())
}
