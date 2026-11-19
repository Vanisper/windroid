use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::Shared;
use super::catalog::Artifact;
use super::integrity::invalid;
use super::paths::regular;

/// 在私有 staging 下只打开普通文件。每个运行时进程持有同一份 advisory lock。
pub(super) fn part_len(path: &Path) -> io::Result<u64> {
    match regular(path) {
        Ok(meta) if meta.nlink() == 1 => Ok(meta.len()),
        Ok(_) => Err(invalid("暂存文件不能含外部硬链接")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

pub(super) fn fetch(shared: &Shared, pkg: Artifact, path: &Path) -> io::Result<()> {
    let mut offset = part_len(path)?;
    if offset > pkg.bytes {
        fs::remove_file(path)?;
        return Err(invalid(format!(
            "{} 的暂存文件超过预期体积，已丢弃",
            pkg.name
        )));
    }
    loop {
        shared.check_pause()?;
        shared.progress(pkg, offset);
        if offset == pkg.bytes {
            return Ok(());
        }
        let (status, diagnostic) = transfer(shared, pkg, path, offset)?;
        if status.success() {
            if part_len(path)? == pkg.bytes {
                return Ok(());
            }
            fs::remove_file(path)?;
            return Err(invalid(format!(
                "{} 的下载大小与记录不符，已丢弃",
                pkg.name
            )));
        }
        // libcurl 在服务器忽略 Range 时以 CURLE_RANGE_ERROR(33) 退出，
        // 不会把 200 的完整响应拼到前缀后面。只在同源从零重试一次。
        if offset > 0 && status.code() == Some(33) {
            fs::remove_file(path)?;
            offset = 0;
            continue;
        }
        return Err(io::Error::other(format!(
            "{} 下载失败（{status}）：{diagnostic}",
            pkg.name
        )));
    }
}

fn transfer(
    shared: &Shared,
    pkg: Artifact,
    path: &Path,
    offset: u64,
) -> io::Result<(ExitStatus, String)> {
    let mut part = OpenOptions::new()
        .append(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if part.metadata()?.len() != offset || part.metadata()?.nlink() != 1 {
        return Err(invalid("暂存文件已发生变化"));
    }
    // curl 只向管道写入。应用被终止时子进程会失去读端，不能继续改写磁盘。
    // -q 必须位于第一位，防止用户的 curlrc 改写输出目标、认证方式或证书策略。
    let mut cmd = Command::new("/usr/bin/curl");
    let protocol = "=https";
    #[cfg(test)]
    let protocol = if pkg.url.starts_with("http://127.0.0.1:") {
        "=http"
    } else {
        protocol
    };
    cmd.args([
        "-q",
        "--proto",
        protocol,
        "--proto-redir",
        protocol,
        "--globoff",
        "--location",
        "--max-redirs",
        "5",
        "--fail",
        "--no-buffer",
        "--silent",
        "--show-error",
        "--max-time",
        "21600",
        "--connect-timeout",
        "15",
        "--speed-limit",
        "1024",
        "--speed-time",
        "45",
    ]);
    if offset != 0 {
        cmd.arg("--continue-at").arg(offset.to_string());
    }
    let mut child = cmd
        .arg("--url")
        .arg(pkg.url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pipe = child.stdout.take().unwrap();
    {
        let mut slot = shared.transfer.lock().unwrap();
        *slot = Some(child);
        if shared.check_pause().is_err() {
            let _ = slot.as_mut().unwrap().kill();
        }
    }
    let mut downloaded = offset;
    let mut last_update = Instant::now();
    let mut buf = vec![0; 128 * 1024];
    let result: io::Result<()> = (|| {
        let mut pipe = pipe;
        loop {
            shared.check_pause()?;
            let count = pipe.read(&mut buf)?;
            if count == 0 {
                return Ok(());
            }
            downloaded = downloaded
                .checked_add(count as u64)
                .filter(|s| *s <= pkg.bytes)
                .ok_or_else(|| invalid("响应超过组件固定大小"))?;
            part.write_all(&buf[..count])?;
            if last_update.elapsed() >= Duration::from_millis(100) {
                shared.progress(pkg, downloaded);
                last_update = Instant::now();
            }
        }
    })();
    let mut child = shared.transfer.lock().unwrap().take().unwrap();
    if result.is_err() {
        let _ = child.kill();
    }
    let output = child.wait_with_output()?;
    shared.check_pause()?;
    if let Err(e) = result {
        if e.kind() == io::ErrorKind::InvalidData {
            fs::remove_file(path)?;
        }
        return Err(e);
    }
    let diagnostic = String::from_utf8_lossy(&output.stderr)
        .trim()
        .chars()
        .take(400)
        .collect();
    Ok((output.status, diagnostic))
}
