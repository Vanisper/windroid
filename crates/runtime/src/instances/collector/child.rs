//! 采集进程持有双流读取端和本轮写者。宿主消失时，控制通道的 EOF 不会关闭双流。
use super::{Notice, Update, receive, send};
use contract::InstanceId;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;
use windroid_diagnostics::{Event, RunWriter, Sink};

/// 每个流只缓存尚未完成的一个块。时间戳在 emit 中取得，未收完的半行可以跨日。
const CHUNK: usize = 16 * 1024;

fn lines(mut reader: impl Read, mut emit: impl FnMut(String, bool)) -> io::Result<()> {
    let mut input = [0; 8192];
    let mut pending = Vec::with_capacity(CHUNK);
    let mut continued = false;
    loop {
        let n = match reader.read(&mut input) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if n == 0 {
            if !pending.is_empty() {
                emit(String::from_utf8_lossy(&pending).into_owned(), continued);
            }
            return Ok(());
        }
        for &byte in &input[..n] {
            if byte == b'\n' {
                if !pending.is_empty() || !continued {
                    if pending.last() == Some(&b'\r') {
                        pending.pop();
                    }
                    emit(String::from_utf8_lossy(&pending).into_owned(), continued);
                }
                pending.clear();
                continued = false;
            } else {
                pending.push(byte);
                if pending.len() == CHUNK {
                    // UTF-8 字符可能跨 read 或分块边界。留下未齐的尾部，交给下一个块。
                    let end = match std::str::from_utf8(&pending) {
                        Err(e) if e.error_len().is_none() => e.valid_up_to(),
                        _ => pending.len(),
                    };
                    emit(String::from_utf8_lossy(&pending[..end]).into_owned(), true);
                    pending.drain(..end);
                    continued = true;
                }
            }
        }
    }
}

fn drain(reader: File, sink: &Sink, source: &str) -> io::Result<()> {
    lines(reader, |text, continued| {
        let mut event = Event::new(source, text);
        event.continued = continued;
        sink.emit(event);
    })
}

pub(super) fn serve(
    home: &Path,
    id: &str,
    app_run: Option<&str>,
    run: &str,
    control_fd: RawFd,
) -> io::Result<()> {
    // 父进程把两条 pipe 的读端放在 fd 0/1，另行继承双向控制 socket。
    // 只从已验证的控制连接收取结果，此进程不打开任何窗口，也不持有模拟器句柄。
    // 即使参数异常，也只取得内核实际复制出的描述符的所有权。
    let fd = unsafe { libc::fcntl(control_fd, libc::F_DUPFD_CLOEXEC, 3) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let socket = unsafe { UnixStream::from_raw_fd(fd) };
    socket.peer_addr()?;
    let input = File::from(io::stdin().as_fd().try_clone_to_owned()?);
    let errors = File::from(io::stdout().as_fd().try_clone_to_owned()?);
    let start = || {
        let root = home.join("instances");
        if !std::fs::symlink_metadata(home)?.is_dir() || !std::fs::symlink_metadata(&root)?.is_dir()
        {
            return Err(super::super::invalid("日志主目录或实例注册目录无效"));
        }
        super::super::read_record(&root, &InstanceId::new(id))?;
        let lease = super::super::instance_directory(&root.join(id))?;
        lease.lock_shared()?;
        Ok((lease, RunWriter::emulator(home, id, app_run, run)?))
    };
    let (_lease, writer) = match start() {
        Ok(result) => result,
        Err(e) => {
            let _ = send(&socket, &Notice::Error(e.to_string()));
            return Err(e);
        }
    };
    // 就绪只说明采集器可写；模拟器尚未创建。
    let _ = send(&socket, &Notice::Ready);
    let sink = writer.sink();
    let outcome = std::thread::scope(|scope| -> io::Result<Option<String>> {
        // 慢盘错误可能出现在最后一条消息入队之后。静默时也能向仍在运行的宿主报告降级。
        let (done, waiting) = mpsc::channel::<()>();
        let (observer, status) = (&socket, &sink);
        scope.spawn(move || {
            loop {
                if status.degraded() {
                    let _ = send(observer, &Notice::Degraded);
                    break;
                }
                if waiting.recv_timeout(Duration::from_millis(250))
                    != Err(mpsc::RecvTimeoutError::Timeout)
                {
                    break;
                }
            }
        });
        let stdout = scope.spawn(|| drain(input, &sink, "stdout"));
        let stderr = scope.spawn(|| drain(errors, &sink, "stderr"));
        let outcome = loop {
            match receive::<Update>(&socket) {
                Ok(Update::Started(pid)) => sink.emit(Event::new(
                    "windroid",
                    format!("已创建模拟器进程 PID {pid}"),
                )),
                Ok(Update::Finished(reason)) => {
                    sink.emit(Event::new("windroid", &reason));
                    break Some(reason);
                }
                Err(_) => break None,
            }
        };
        for reader in [stdout, stderr] {
            reader
                .join()
                .map_err(|_| io::Error::other("排空模拟器输出时发生异常"))??;
        }
        drop(done);
        Ok(outcome)
    })?;
    match outcome {
        Some(result) => writer.finish(result),
        None => writer.finish_unknown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_preserve_empty_lines_unterminated_tail_and_utf8_at_chunk_boundary() {
        let prefix = "a".repeat(CHUNK - 1);
        let input = format!("\r\n{prefix}界余\r\n无换行");
        let mut output = Vec::new();
        lines(input.as_bytes(), |text, continued| {
            output.push((text, continued))
        })
        .unwrap();
        assert_eq!(
            output,
            vec![
                (String::new(), false),
                (prefix, true),
                ("界余".into(), true),
                ("无换行".into(), false)
            ]
        );
    }

    #[test]
    fn incomplete_line_is_stamped_only_when_complete() {
        use std::cell::Cell;
        struct Timed<'a> {
            day: &'a Cell<u32>,
            part: usize,
        }
        impl Read for Timed<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let chunk = match self.part {
                    0 => {
                        self.day.set(18);
                        "before\npartial"
                    }
                    1 => {
                        self.day.set(19);
                        " after\n"
                    }
                    _ => return Ok(0),
                };
                self.part += 1;
                buf[..chunk.len()].copy_from_slice(chunk.as_bytes());
                Ok(chunk.len())
            }
        }
        let day = Cell::new(0);
        let mut stamped = Vec::new();
        lines(Timed { day: &day, part: 0 }, |text, _| {
            stamped.push((day.get(), text))
        })
        .unwrap();
        assert_eq!(
            stamped,
            [
                (18, "before".to_string()),
                (19, "partial after".to_string())
            ]
        );
    }
}
