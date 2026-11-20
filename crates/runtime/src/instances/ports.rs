//! macOS 预览从本机 TCP 套接字读取端点；实际启动仍需绑定端口并取得共享预留。

use std::collections::BTreeSet;
use std::io;

pub(super) struct Occupied(BTreeSet<u16>);

impl Occupied {
    pub fn check(&self, port: u16) -> io::Result<()> {
        // Darwin 允许带 SO_REUSEADDR 的回环监听覆盖同一端口的通配监听。
        // 通配服务也覆盖该回环端点，创建和启动都避开它，避免接管别的服务的流量。
        if self.0.contains(&port) || self.0.contains(&(port + 1)) {
            return Err(super::in_use(port));
        }
        Ok(())
    }

    // 其他 Unix 宿主仍由创建/启动原有的绑定检查判断，受管模拟器需要 Apple 芯片 Mac。
    #[cfg(not(target_os = "macos"))]
    pub fn read() -> io::Result<Self> {
        Ok(Self(BTreeSet::new()))
    }

    #[cfg(target_os = "macos")]
    pub fn read() -> io::Result<Self> {
        let range = format!("-iTCP:{}-{}", super::FIRST_PORT, super::LAST_PORT + 1);
        let output = std::process::Command::new("/usr/sbin/lsof")
            .args(["-nP", "-Fn", &range])
            .env("LC_ALL", "C")
            .output()
            .map_err(|e| io::Error::new(e.kind(), format!("无法读取本机 TCP 端口：{e}")))?;
        // lsof 在有效的选择条件未匹配任何文件时退出 1，不产生输出。
        if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
            return Ok(Self(BTreeSet::new()));
        }
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "无法读取本机 TCP 端口（{}）：{}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        parse(&String::from_utf8_lossy(&output.stdout)).map(Self)
    }
}

// lsof(8) 字段 n 标识数值端点。无对端的本地 TCP 项包括已监听及仅 bind 的套接字；
// 已连接及 TIME_WAIT 的四元组不独占回环监听端点。
// https://github.com/lsof-org/lsof/blob/master/Lsof.8
#[cfg(any(target_os = "macos", test))]
fn parse(records: &str) -> io::Result<BTreeSet<u16>> {
    let mut ports = BTreeSet::new();
    let malformed = || io::Error::other("无法识别本机 TCP 端口，请重新检查");
    for endpoint in records.lines().filter_map(|s| s.strip_prefix('n')) {
        if endpoint.contains("->") {
            continue;
        }
        let (address, port) = endpoint.rsplit_once(':').ok_or_else(malformed)?;
        if matches!(address, "*" | "127.0.0.1" | "[::1]") {
            ports.insert(port.parse().map_err(|_| malformed())?);
        }
    }
    Ok(ports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connected_sockets_and_unrelated_interfaces_do_not_block_loopback() {
        let rows = "p72\nf7\nn127.0.0.1:5554->127.0.0.1:61333\nn127.0.0.1:61334->127.0.0.1:5555\nn192.0.2.1:5556\nn127.0.0.1:5557\nn*:5558\nn[::1]:5559";
        assert_eq!(parse(rows).unwrap(), BTreeSet::from([5557, 5558, 5559]));
        assert!(parse("nunrecognized").is_err());
    }
}
