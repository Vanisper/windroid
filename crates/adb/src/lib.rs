//! adb 命令行封装。
//!
//! 这一层只做两件事：把命令发出去、把输出原样取回来。它**不判断成功**——
//! adb 的退出码与 `Success` 文本都不是证据（见 docs/架构/02-设备域.md 第七节）：
//! `adb install` 在磁盘满时退出 0，`am start` 失败也退出 0。
//! 判断留给调用方，通过回读实际状态完成。

use std::io;
use std::process::{Command, Output as ProcOutput};

/// adb 可执行文件路径。
///
/// 默认从 PATH 取。可用 `WINDROID_ADB` 覆盖——测试与多 SDK 环境需要它。
fn program() -> String {
    std::env::var("WINDROID_ADB").unwrap_or_else(|_| "adb".to_owned())
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

/// 对某台设备执行一条 adb 子命令。
pub fn run(serial: &str, args: &[&str]) -> io::Result<AdbOutput> {
    let out: ProcOutput = Command::new(program())
        .arg("-s")
        .arg(serial)
        .args(args)
        .output()?;
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
    let out = Command::new(program()).arg("devices").output()?;
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
    run(serial, &["forward", "--remove", &format!("tcp:{local_port}")])
}

/// 推送文件到设备。
pub fn push(serial: &str, local: &str, remote: &str) -> io::Result<AdbOutput> {
    run(serial, &["push", local, remote])
}
