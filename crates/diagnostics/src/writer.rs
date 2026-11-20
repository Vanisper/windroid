use crate::{
    Event, Manifest, Record, Segment, Stamp,
    fs::{Dir, invalid, valid_run},
    new_run_id, retention,
};
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const QUEUE: usize = 256;
const BATCH: usize = 64;
const TEXT_LIMIT: usize = 64 << 10;

enum Message {
    Row(Box<Record>),
    End {
        seq: u64,
        stamp: Stamp,
        outcome: Option<String>,
    },
}
struct Input {
    sender: SyncSender<Message>,
    last: u64,
    closed: bool,
}
#[derive(Default)]
struct Health {
    disk_failed: AtomicBool,
    last_dropped: AtomicU64,
    saved: AtomicU64,
}

#[derive(Clone)]
pub struct Sink {
    input: Arc<Mutex<Input>>,
    health: Arc<Health>,
}
impl Sink {
    /// 锁只保护打戳、分配序号及 try_send，从不等待磁盘或队列容量。
    pub fn emit(&self, event: Event) {
        self.capture(event, Stamp::now)
    }
    fn capture(&self, mut event: Event, stamp: impl FnOnce() -> Stamp) {
        if event.text.len() > TEXT_LIMIT {
            let end = event.text.floor_char_boundary(TEXT_LIMIT);
            event.text.truncate(end);
            event.text.push_str(" [文本已截断]");
        }
        let mut input = self.input.lock().unwrap();
        if input.closed {
            return;
        }
        input.last += 1;
        let row = Record {
            seq: input.last,
            segment: 0,
            stamp: stamp(),
            event,
        };
        if input.sender.try_send(Message::Row(Box::new(row))).is_err() {
            self.health
                .last_dropped
                .store(input.last, Ordering::Release);
        }
    }
    pub fn degraded(&self) -> bool {
        self.health.disk_failed.load(Ordering::Acquire)
            || self.health.last_dropped.load(Ordering::Acquire)
                > self.health.saved.load(Ordering::Acquire)
    }
}

pub struct RunWriter {
    id: String,
    sink: Sink,
    thread: Option<JoinHandle<io::Result<()>>>,
}
impl RunWriter {
    pub fn app(home: &Path) -> io::Result<Self> {
        Self::app_with_id(home, &new_run_id()?)
    }
    pub fn app_with_id(home: &Path, id: &str) -> io::Result<Self> {
        Self::open(home, None, None, id)
    }
    pub fn emulator(
        home: &Path,
        instance_id: &str,
        app_run_id: Option<&str>,
        run_id: &str,
    ) -> io::Result<Self> {
        Self::open(home, Some(instance_id), app_run_id, run_id)
    }
    fn open(home: &Path, instance: Option<&str>, app: Option<&str>, id: &str) -> io::Result<Self> {
        let mut disk = Disk::open(home, instance, app, id, retention::Limits::default())?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let health = Arc::new(Health::default());
        let status = health.clone();
        let thread = thread::Builder::new()
            .name("diagnostic-writer".into())
            .spawn(move || disk.consume(receiver, &status))?;
        let sink = Sink {
            input: Arc::new(Mutex::new(Input {
                sender,
                last: 0,
                closed: false,
            })),
            health,
        };
        Ok(Self {
            id: id.into(),
            sink,
            thread: Some(thread),
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn sink(&self) -> Sink {
        self.sink.clone()
    }
    pub fn degraded(&self) -> bool {
        self.sink.degraded()
    }
    pub fn finish(mut self, outcome: impl Into<String>) -> io::Result<()> {
        self.close(Some(outcome.into()))
    }
    pub fn finish_unknown(mut self) -> io::Result<()> {
        self.close(None)
    }
    fn close(&mut self, outcome: Option<String>) -> io::Result<()> {
        let Some(worker) = self.thread.take() else {
            return Ok(());
        };
        let mut input = self.sink.input.lock().unwrap();
        input.closed = true;
        let seq = input.last;
        let _ = input.sender.send(Message::End {
            seq,
            stamp: Stamp::now(),
            outcome,
        });
        drop(input);
        worker
            .join()
            .map_err(|_| io::Error::other("诊断日志写者意外退出"))?
    }
}
impl Drop for RunWriter {
    fn drop(&mut self) {
        let _ = self.close(None);
    }
}

pub(crate) struct Disk {
    pub(crate) home: PathBuf,
    pub(crate) dir: Dir,
    pub(crate) manifest: Manifest,
    _lock: File,
    segment: Option<File>,
    pub(crate) limit: retention::Limits,
    persisted: u64,
    dirty: bool,
}
impl Disk {
    fn open(
        home: &Path,
        instance: Option<&str>,
        app: Option<&str>,
        id: &str,
        limit: retention::Limits,
    ) -> io::Result<Self> {
        if !valid_run(id) || app.is_some_and(|id| !valid_run(id)) {
            return Err(invalid("启动标识无效"));
        }
        let root = Dir::home(home)?;
        let runs = root.runs(instance, true)?;
        runs.mkdir(id)?;
        let dir = runs.sub(id, false)?;
        let lock = dir.create("writer.lock")?;
        lock.try_lock()?;
        let manifest = Manifest {
            format: 1,
            source: if instance.is_some() {
                "emulator"
            } else {
                "app"
            }
            .into(),
            id: id.into(),
            instance_id: instance.map(str::to_owned),
            app_run_id: app.map(str::to_owned),
            started: Stamp::now(),
            ended: None,
            outcome: None,
            segments: vec![],
            lost: false,
            sealed: false,
        };
        dir.publish("run.json", &serde_json::to_vec(&manifest)?)?;
        Ok(Self {
            home: home.to_owned(),
            dir,
            manifest,
            _lock: lock,
            segment: None,
            limit,
            persisted: 0,
            dirty: false,
        })
    }
    pub(crate) fn save(&mut self) -> io::Result<()> {
        self.dir
            .publish("run.json", &serde_json::to_vec(&self.manifest)?)?;
        self.dirty = false;
        Ok(())
    }
    fn consume(&mut self, receiver: Receiver<Message>, health: &Health) -> io::Result<()> {
        // 多个来源共用保留预算。静默写者也处理清理请求，且不为静默日期建段。
        let mut maintenance = Instant::now();
        let mut unfinished = 0;
        loop {
            if maintenance.elapsed() >= Duration::from_secs(2) {
                if let Err(e) = retention::maintain(self, None)
                    && !health.disk_failed.swap(true, Ordering::AcqRel)
                {
                    eprintln!("日志保留检查失败：{e}");
                }
                maintenance = Instant::now();
            }
            let first = match receiver.recv_timeout(Duration::from_secs(2)) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            };
            for message in std::iter::once(first).chain(receiver.try_iter().take(BATCH - 1)) {
                match message {
                    Message::Row(row) => {
                        unfinished = row.seq;
                        if let Err(e) = self.write_row(*row) {
                            self.manifest.lost = true;
                            if !health.disk_failed.swap(true, Ordering::AcqRel) {
                                eprintln!("写入诊断日志失败：{e}")
                            }
                        }
                    }
                    Message::End {
                        seq,
                        stamp,
                        outcome,
                    } => {
                        let gap = self.fill_gap(seq, &stamp);
                        self.manifest.ended = outcome.as_ref().map(|_| stamp);
                        self.manifest.outcome = outcome;
                        self.manifest.sealed = true;
                        self.segment = None;
                        self.save()?;
                        return gap;
                    }
                }
            }
            if self.dirty
                && let Err(e) = self.save()
            {
                if !health.disk_failed.swap(true, Ordering::AcqRel) {
                    eprintln!("无法发布诊断日志索引：{e}")
                }
            } else if self.persisted == unfinished {
                health.saved.store(self.persisted, Ordering::Release);
                health.disk_failed.store(false, Ordering::Release)
            }
        }
    }
    fn fill_gap(&mut self, last: u64, stamp: &Stamp) -> io::Result<()> {
        if self.persisted < last {
            self.manifest.lost = true;
            self.append(Record::gap(
                self.persisted + 1,
                last,
                stamp.clone(),
                0,
                "采集缺口",
            ))?;
        }
        Ok(())
    }
    fn write_row(&mut self, row: Record) -> io::Result<()> {
        self.fill_gap(row.seq - 1, &row.stamp)?;
        self.append(row)
    }
    fn append(&mut self, mut row: Record) -> io::Result<()> {
        let mut reservation = None;
        let previous = self.manifest.segments.last();
        row.segment = previous.map_or(1, |s| s.number);
        let mut bytes = serde_json::to_vec(&row)?;
        if self.segment.is_none()
            || previous.is_none_or(|s| {
                s.stamp.day != row.stamp.day
                    || s.bytes + bytes.len() as u64 + 1 > self.limit.segment
            })
        {
            self.segment = None;
            row.segment = previous.map_or(1, |s| s.number + 1);
            bytes = serde_json::to_vec(&row)?;
            reservation = Some(retention::maintain(
                self,
                Some(self.limit.charge(bytes.len() as u64 + 1)),
            )?);
            let segment = Segment {
                number: row.segment,
                first: row.seq,
                last: row.seq,
                stamp: row.stamp.clone(),
                latest_utc: row.stamp.utc.clone(),
                bytes: 0,
                removed: false,
            };
            self.segment = Some(self.dir.create(&segment.filename())?);
            self.manifest.segments.push(segment);
        } else if let Some(previous) = previous {
            let old = self.limit.charge(previous.bytes);
            let new = self.limit.charge(previous.bytes + bytes.len() as u64 + 1);
            if new > old {
                reservation = Some(retention::maintain(self, Some(new - old))?)
            }
        }
        let file = self.segment.as_mut().unwrap();
        let segment = self.manifest.segments.last_mut().unwrap();
        bytes.push(b'\n');
        if let Err(e) = file.write_all(&bytes) {
            // write_all 可能留下半条。回到旧边界；若无法回滚，封存本段，读者忽略残尾。
            if file
                .set_len(segment.bytes)
                .and_then(|()| file.seek(SeekFrom::Start(segment.bytes)))
                .is_err()
            {
                self.segment = None
            }
            return Err(e);
        }
        segment.bytes += bytes.len() as u64;
        segment.last = row.seq;
        if segment.latest_utc < row.stamp.utc {
            segment.latest_utc = row.stamp.utc
        }
        self.persisted = row.seq;
        self.dirty = true;
        if reservation.is_some() {
            self.save()?;
        }
        Ok(())
    }

    pub(crate) fn active_segment(&self) -> Option<u64> {
        self.segment
            .as_ref()
            .and_then(|_| self.manifest.segments.last().map(|s| s.number))
    }
}

/// 采集子进程意外退出后，宿主通过自己的进程句柄取得最终结果。只有原写者
/// 已释放锁才能续写同一清单。丢失输出的具体数量未知，不能伪造其序号范围。
pub fn record_interruption(home: &Path, instance: &str, id: &str, outcome: &str) -> io::Result<()> {
    if !valid_run(id) {
        return Err(invalid("启动标识无效"));
    }
    let dir = Dir::home(home)?
        .runs(Some(instance), false)?
        .sub(id, false)?;
    let lock = dir.read("writer.lock")?;
    lock.try_lock()?;
    let manifest = crate::reader::sealed_manifest(&dir, id, Some(instance))?;
    if manifest.outcome.is_some() {
        return Ok(());
    }
    let persisted = manifest.segments.last().map_or(0, |s| s.last);
    let mut disk = Disk {
        home: home.into(),
        dir,
        manifest,
        _lock: lock,
        segment: None,
        limit: retention::Limits::default(),
        persisted,
        dirty: false,
    };
    disk.manifest.lost = true;
    let stamp = Stamp::now();
    let result = disk
        .append(Record {
            seq: persisted + 1,
            segment: 0,
            stamp: stamp.clone(),
            event: Event::new("gap", "采集器中断，后续模拟器输出可能缺失，数量未知"),
        })
        .and_then(|_| {
            disk.append(Record {
                seq: persisted + 2,
                segment: 0,
                stamp: stamp.clone(),
                event: Event::new("windroid", outcome),
            })
        });
    disk.manifest.ended = Some(stamp);
    disk.manifest.outcome = Some(outcome.into());
    disk.manifest.sealed = true;
    disk.save()?;
    result
}

#[cfg(test)]
mod tests;
