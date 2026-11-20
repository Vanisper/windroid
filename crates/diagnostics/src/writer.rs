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
    atomic::{AtomicBool, Ordering},
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
#[derive(Clone)]
pub struct Sink {
    input: Arc<Mutex<Input>>,
    degraded: Arc<AtomicBool>,
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
            self.degraded.store(true, Ordering::Release);
        }
    }
    pub fn degraded(&self) -> bool {
        self.degraded.load(Ordering::Acquire)
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
        };
        dir.publish("run.json", &serde_json::to_vec(&manifest)?)?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let degraded = Arc::new(AtomicBool::new(false));
        let mut disk = Disk {
            home: home.to_owned(),
            dir,
            manifest,
            _lock: lock,
            segment: None,
            limit: retention::Limits::default(),
            persisted: 0,
            dirty: false,
        };
        let flag = degraded.clone();
        let thread = thread::Builder::new()
            .name("diagnostic-writer".into())
            .spawn(move || disk.consume(receiver, &flag))?;
        let sink = Sink {
            input: Arc::new(Mutex::new(Input {
                sender,
                last: 0,
                closed: false,
            })),
            degraded,
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
    pub(crate) fn save(&mut self) -> io::Result<()> {
        self.dir
            .publish("run.json", &serde_json::to_vec(&self.manifest)?)?;
        self.dirty = false;
        Ok(())
    }
    fn consume(&mut self, receiver: Receiver<Message>, flag: &AtomicBool) -> io::Result<()> {
        // 多个来源共用保留预算。静默写者也处理清理请求，且不为静默日期建段。
        let mut maintenance = Instant::now();
        let mut unfinished = 0;
        loop {
            if maintenance.elapsed() >= Duration::from_secs(2) {
                if let Err(e) = retention::maintain(self, false) {
                    flag.store(true, Ordering::Release);
                    eprintln!("日志保留检查失败：{e}")
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
                        if self.write_row(*row).is_err() {
                            self.manifest.lost = true;
                            flag.store(true, Ordering::Release)
                        }
                    }
                    Message::End {
                        seq,
                        stamp,
                        outcome,
                    } => {
                        let gap = self.fill_gap(seq, &stamp);
                        self.manifest.ended = Some(stamp);
                        self.manifest.outcome = outcome;
                        self.segment = None;
                        self.save()?;
                        return gap;
                    }
                }
            }
            if self.dirty
                && let Err(e) = self.save()
            {
                if !flag.swap(true, Ordering::AcqRel) {
                    eprintln!("无法发布诊断日志索引：{e}")
                }
            } else if self.persisted == unfinished {
                flag.store(false, Ordering::Release)
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
            reservation = Some(retention::maintain(self, true)?);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Catalog, legacy_page, read_page, reveal_path};
    use chrono::DateTime;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};
    struct Home(PathBuf);
    impl Home {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("windroid-log-test-{}", new_run_id().unwrap()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn instance(&self, id: &str) -> PathBuf {
            let dir = self.0.join("instances").join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("instance.json"),
                serde_json::json!({"id":id,"name":"同名实例"}).to_string(),
            )
            .unwrap();
            dir
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn stamp(local: &str) -> Stamp {
        let t = DateTime::parse_from_rfc3339(local).unwrap();
        Stamp::at(t.to_utc(), t.offset().local_minus_utc())
    }
    #[test]
    fn capture_time_controls_midnight_silence_clock_rollback_and_paging() {
        let home = Home::new();
        let writer = RunWriter::app(&home.0).unwrap();
        let id = writer.id().to_owned();
        // 固定未来日期，免受真实 UTC 保留期限影响。写入线程可能在这些记录之后才运行。
        let times = [
            "2030-10-31T23:59:59-04:00",
            "2030-11-01T00:00:01-04:00",
            "2030-11-03T01:30:00-04:00",
            "2030-11-03T01:30:00-05:00",
            "2030-10-31T15:00:00-08:00",
        ];
        let s = writer.sink();
        for t in times {
            s.capture(Event::new("windroid", t), || stamp(t))
        }
        assert!(Catalog::load(&home.0, None).unwrap().runs[0].active);
        writer.finish("正常退出").unwrap();
        let catalog = Catalog::load(&home.0, None).unwrap();
        assert_eq!(catalog.runs[0].outcome.as_deref(), Some("正常退出"));
        assert_eq!(
            catalog.runs[0].dates,
            ["2030-11-03", "2030-11-01", "2030-10-31"]
        );
        let a = read_page(&home.0, None, &id, None, 0, 3).unwrap();
        let b = read_page(&home.0, None, &id, None, 3, 3).unwrap();
        assert!(a.more && !b.more);
        let joined = a.records.into_iter().chain(b.records).collect::<Vec<_>>();
        assert_eq!(
            joined
                .iter()
                .map(|r| (r.seq, r.segment))
                .collect::<Vec<_>>(),
            [(1, 1), (2, 2), (3, 3), (4, 3), (5, 4)]
        );
        assert_ne!(joined[2].stamp.offset, joined[3].stamp.offset);
        let date = read_page(&home.0, None, &id, Some("2030-10-31"), 0, 20).unwrap();
        assert_eq!(
            date.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            [1, 5]
        );
        let file = reveal_path(&home.0, None, Some(&id), false, Some(5)).unwrap();
        assert!(file.ends_with("000004-2030-10-31.jsonl"));
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(file.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
    }
    #[test]
    fn startup_history_isolated_by_registered_owner_and_legacy_is_read_only() {
        let home = Home::new();
        let app_id = new_run_id().unwrap();
        let run_id = new_run_id().unwrap();
        for owner in ["one", "two"] {
            let dir = home.instance(owner);
            fs::write(dir.join("emulator.log"), "旧版记录").unwrap();
            let w = RunWriter::emulator(&home.0, owner, Some(&app_id), &run_id).unwrap();
            w.sink().emit(Event::new("stdout", owner));
            w.finish("exit 0").unwrap();
            let w =
                RunWriter::emulator(&home.0, owner, Some(&app_id), &new_run_id().unwrap()).unwrap();
            w.finish("进程创建失败").unwrap();
            assert_eq!(
                read_page(&home.0, Some(owner), &run_id, None, 0, 8)
                    .unwrap()
                    .records[0]
                    .text,
                owner
            );
            let c = Catalog::load(&home.0, Some(owner)).unwrap();
            assert!(c.legacy);
            assert_eq!(c.runs.len(), 2);
            assert!(
                c.runs
                    .iter()
                    .all(|r| r.instance_id.as_deref() == Some(owner)
                        && r.app_run_id.as_ref() == Some(&app_id))
            );
            assert_eq!(
                legacy_page(&home.0, owner, 0, 5).unwrap(),
                ("旧".into(), 3, true)
            );
            assert_eq!(
                fs::read(dir.join("emulator.log")).unwrap(),
                "旧版记录".as_bytes()
            );
        }
        assert!(Catalog::load(&home.0, None).unwrap().runs.is_empty());
        assert!(RunWriter::emulator(&home.0, "one", None, &run_id).is_err());
        assert!(read_page(&home.0, Some(".."), &run_id, None, 0, 4).is_err());
        fs::rename(
            home.0.join("instances/one/emulator.log"),
            home.0.join("outside"),
        )
        .unwrap();
        symlink(
            home.0.join("outside"),
            home.0.join("instances/one/emulator.log"),
        )
        .unwrap();
        assert!(legacy_page(&home.0, "one", 0, 8).is_err());
        fs::rename(
            home.0.join("instances/one/logs"),
            home.0.join("logs-elsewhere"),
        )
        .unwrap();
        symlink(
            home.0.join("logs-elsewhere"),
            home.0.join("instances/one/logs"),
        )
        .unwrap();
        assert!(read_page(&home.0, Some("one"), &run_id, None, 0, 4).is_err());
    }
    #[test]
    fn rebuilds_after_incomplete_manifest_and_tail_without_inferring_an_exit() {
        let home = Home::new();
        let writer = RunWriter::app(&home.0).unwrap();
        let id = writer.id().to_owned();
        writer.sink().emit(Event::new("windroid", "完整"));
        writer.finish("ok").unwrap();
        let p = reveal_path(&home.0, None, Some(&id), false, Some(1)).unwrap();
        File::options()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(br#"{"seq":2"#)
            .unwrap();
        fs::remove_file(p.parent().unwrap().join("run.json")).unwrap();
        let c = Catalog::load(&home.0, None).unwrap();
        assert!(c.runs[0].outcome.is_none() && !c.runs[0].active);
        assert_eq!(
            read_page(&home.0, None, &id, None, 0, 50)
                .unwrap()
                .records
                .len(),
            1
        );
    }
    #[test]
    fn stalled_disk_never_blocks_producers_and_reports_exact_sequence_gap() {
        let home = Home::new();
        let w = RunWriter::app(&home.0).unwrap();
        let id = w.id().to_owned();
        let g = File::create(home.0.join("logs/.retention.lock")).unwrap();
        g.lock().unwrap();
        let sink = w.sink();
        for _ in 0..QUEUE * 3 {
            sink.emit(Event::new("stdout", "一行"))
        }
        assert!(sink.degraded());
        drop(g);
        w.finish("ok").unwrap();
        let p = read_page(&home.0, None, &id, None, 0, 500).unwrap();
        let last = p.records.last().unwrap();
        assert_eq!(last.seq as usize, QUEUE * 3);
        assert_eq!(last.source, "gap");
        assert!(p.records.len() <= QUEUE + 2);
        assert!(Catalog::load(&home.0, None).unwrap().runs[0].lost);
    }
}
