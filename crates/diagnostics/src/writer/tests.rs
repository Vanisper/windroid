use super::*;
use crate::{Catalog, legacy_page, open_segment_file, read_page};
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
    let file = open_segment_file(&home.0, None, &id, 5).unwrap().path;
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
        let w = RunWriter::emulator(&home.0, owner, Some(&app_id), &new_run_id().unwrap()).unwrap();
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
    let p = open_segment_file(&home.0, None, &id, 1).unwrap().path;
    File::options()
        .append(true)
        .open(&p)
        .unwrap()
        .write_all(br#"{"seq":2"#)
        .unwrap();
    fs::write(p.parent().unwrap().join("run.json"), b"{").unwrap();
    let c = Catalog::load(&home.0, None).unwrap();
    assert!(c.runs[0].outcome.is_none() && !c.runs[0].active && c.runs[0].lost);
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
    fs::create_dir(home.0.join(".diagnostic-budget")).unwrap();
    let g = File::create(home.0.join(".diagnostic-budget/lock")).unwrap();
    g.lock().unwrap();
    let sink = w.sink();
    for _ in 0..QUEUE * 3 {
        sink.emit(Event::new("stdout", "一行"))
    }
    assert!(sink.degraded());
    drop(g);
    let deadline = Instant::now() + Duration::from_secs(8);
    while sink.degraded() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!sink.degraded());
    assert!(Catalog::load(&home.0, None).unwrap().runs[0].lost);
    w.finish("ok").unwrap();
    let p = read_page(&home.0, None, &id, None, 0, 500).unwrap();
    let last = p.records.last().unwrap();
    assert_eq!(last.seq as usize, QUEUE * 3);
    assert_eq!(last.source, "gap");
    assert!(p.records.len() <= QUEUE + 2);
    assert!(Catalog::load(&home.0, None).unwrap().runs[0].lost);
}

fn row(seq: u64, when: Stamp, text: &str) -> Record {
    Record {
        seq,
        gap_start: None,
        segment: 0,
        stamp: when,
        event: Event::new("stdout", text),
    }
}

#[test]
fn retention_uses_utc_and_keeps_the_active_segment_and_legacy() {
    let home = Home::new();
    let dir = home.instance("one");
    let legacy = dir.join("emulator.log");
    fs::write(&legacy, "2000").unwrap();
    let id = new_run_id().unwrap();
    let mut disk = Disk::open(
        &home.0,
        Some("one"),
        None,
        &id,
        retention::Limits::default(),
    )
    .unwrap();
    disk.write_row(row(1, stamp("2000-01-01T00:00:00+08:00"), "旧"))
        .unwrap();
    retention::maintain(&mut disk, None).unwrap();
    assert!(!disk.manifest.segments[0].removed);
    let old_file = open_segment_file(&home.0, Some("one"), &id, 1)
        .unwrap()
        .path;
    disk.write_row(row(2, Stamp::now(), "新")).unwrap();
    assert!(disk.manifest.segments[0].removed);
    assert!(!old_file.exists());
    let old = read_page(&home.0, Some("one"), &id, Some("2000-01-01"), 0, 1).unwrap();
    assert_eq!(old.records[0].source, "gap");
    assert!(old.records[0].text.contains("1–1"));
    let new = read_page(&home.0, Some("one"), &id, None, 1, 1).unwrap();
    assert_eq!(new.records[0].seq, 2);
    assert_eq!(fs::read(legacy).unwrap(), b"2000");
}

#[test]
fn quota_across_live_writers_reclaims_sealed_segments_then_resumes_with_a_gap() {
    let home = Home::new();
    let limits = retention::Limits {
        segment: 2048,
        total: 6000,
        days: 14,
    };
    let first = new_run_id().unwrap();
    let second = new_run_id().unwrap();
    let mut a = Disk::open(&home.0, None, None, &first, limits).unwrap();
    let mut b = Disk::open(&home.0, None, None, &second, limits).unwrap();
    let text = "x".repeat(950);
    a.write_row(row(1, Stamp::now(), &text)).unwrap();
    a.write_row(row(2, Stamp::now(), &text)).unwrap();
    assert!(b.write_row(row(1, Stamp::now(), &text)).is_err());
    assert!(!a.manifest.segments[0].removed);
    retention::maintain(&mut a, None).unwrap();
    assert!(a.manifest.segments[0].removed && !a.manifest.segments[1].removed);
    b.write_row(row(2, Stamp::now(), "恢复")).unwrap();
    b.save().unwrap();
    let page = read_page(&home.0, None, &second, None, 0, 2).unwrap();
    assert_eq!(page.records[0].source, "gap");
    assert_eq!(page.records[0].seq, 1);
    assert_eq!(page.records[1].text, "恢复");
}

#[test]
fn only_host_with_an_actual_exit_can_finalize_dead_collector_history_once() {
    let home = Home::new();
    home.instance("test");
    let id = new_run_id().unwrap();
    let writer = RunWriter::emulator(&home.0, "test", None, &id).unwrap();
    writer.sink().emit(Event::new("stderr", "before"));
    assert!(record_interruption(&home.0, "test", &id, "退出码 3").is_err());
    writer.finish_unknown().unwrap();
    record_interruption(&home.0, "test", &id, "退出码 3").unwrap();
    record_interruption(&home.0, "test", &id, "不应覆盖").unwrap();
    let catalog = Catalog::load(&home.0, Some("test")).unwrap();
    assert_eq!(catalog.runs[0].outcome.as_deref(), Some("退出码 3"));
    assert!(catalog.runs[0].lost);
    let page = read_page(&home.0, Some("test"), &id, None, 0, 8).unwrap();
    assert_eq!(
        page.records
            .iter()
            .map(|r| r.source.as_str())
            .collect::<Vec<_>>(),
        ["stderr", "gap", "windroid"]
    );
}

#[test]
fn damaged_neighbours_do_not_hide_valid_runs_or_redirect_writes() {
    let home = Home::new();
    let external = Home::new();
    let one = home.instance("one");
    let two = home.instance("two");
    fs::write(one.join("logs"), "不是目录").unwrap();
    symlink(&external.0, two.join("logs")).unwrap();
    let bad = RunWriter::app(&home.0).unwrap();
    let bad_id = bad.id().to_owned();
    bad.sink().emit(Event::new("windroid", "原记录"));
    bad.finish("退出").unwrap();
    let index = home.0.join("logs/app").join(&bad_id).join("run.json");
    let mut poisoned: serde_json::Value =
        serde_json::from_reader(File::open(&index).unwrap()).unwrap();
    poisoned["id"] = "wrong-owner".into();
    fs::write(&index, serde_json::to_vec(&poisoned).unwrap()).unwrap();
    let good = RunWriter::app(&home.0).unwrap();
    let good_id = good.id().to_owned();
    good.sink()
        .emit(Event::new("windroid", "实例采集目录不可用"));
    good.finish("退出").unwrap();
    let catalog = Catalog::load(&home.0, None).unwrap();
    assert_eq!(catalog.runs.len(), 1);
    assert_eq!(catalog.runs[0].id, good_id);
    assert_eq!(catalog.issues.len(), 1);
    assert!(catalog.issues[0].contains(&bad_id));
    assert_eq!(
        read_page(&home.0, None, &good_id, None, 0, 8)
            .unwrap()
            .records[0]
            .text,
        "实例采集目录不可用"
    );
    assert_eq!(fs::read_dir(&external.0).unwrap().count(), 0);
    assert_eq!(
        fs::read(index).unwrap(),
        serde_json::to_vec(&poisoned).unwrap()
    );
}

#[test]
fn unreachable_owner_keeps_its_reserved_bytes_while_other_runs_can_use_the_rest() {
    let home = Home::new();
    let owner = home.instance("one");
    let id = new_run_id().unwrap();
    let limits = retention::Limits {
        segment: 2048,
        total: 3800,
        days: 14,
    };
    let mut prior = Disk::open(&home.0, Some("one"), None, &id, limits).unwrap();
    prior.write_row(row(1, Stamp::now(), "protected")).unwrap();
    drop(prior);
    fs::rename(owner.join("logs"), owner.join("hidden")).unwrap();
    fs::write(owner.join("logs"), "原位置无法访问").unwrap();
    let id = new_run_id().unwrap();
    let mut other = Disk::open(&home.0, None, None, &id, limits).unwrap();
    assert!(other.write_row(row(1, Stamp::now(), "over quota")).is_err());
    other.limit.total = 7000;
    other
        .write_row(row(2, Stamp::now(), "still accessible"))
        .unwrap();
    other.save().unwrap();
    let page = read_page(&home.0, None, &id, None, 0, 4).unwrap();
    assert_eq!(page.records[0].source, "gap");
    assert_eq!(page.records[1].text, "still accessible");
}

#[test]
fn emulator_can_write_when_application_log_tree_is_unusable() {
    let home = Home::new();
    home.instance("one");
    fs::write(home.0.join("logs"), "unavailable").unwrap();
    let id = new_run_id().unwrap();
    let w = RunWriter::emulator(&home.0, "one", None, &id).unwrap();
    w.sink().emit(Event::new("stderr", "ready"));
    w.finish("退出").unwrap();
    assert_eq!(
        read_page(&home.0, Some("one"), &id, None, 0, 4)
            .unwrap()
            .records[0]
            .text,
        "ready"
    );
}

#[test]
fn unreadable_coordinator_preserves_known_exit_and_tail_range_if_index_is_writable() {
    let home = Home::new();
    let writer = RunWriter::app(&home.0).unwrap();
    let id = writer.id().to_owned();
    fs::write(home.0.join(".diagnostic-budget"), "blocked").unwrap();
    for _ in 0..3 {
        writer.sink().emit(Event::new("windroid", "record"))
    }
    assert!(writer.finish("正常退出").is_err());
    let c = Catalog::load(&home.0, None).unwrap();
    assert!(c.runs[0].lost);
    assert_eq!(c.runs[0].outcome.as_deref(), Some("正常退出"));
    let rows = read_page(&home.0, None, &id, None, 0, 4).unwrap().records;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].gap_start, rows[0].seq, rows[0].segment),
        (Some(1), 3, 0)
    );
    assert!(rows[0].text.contains("采集缺口"));
}
