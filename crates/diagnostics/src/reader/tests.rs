use super::*;
use crate::{Event, RunWriter, new_run_id};
use std::fs;

const DAY_ONE: &str = "2030-10-31";
const DAY_TWO: &str = "2030-11-01";

struct Fixture {
    home: PathBuf,
    id: String,
    dir: Dir,
    manifest: Manifest,
}
impl Fixture {
    fn new(days: &[(&str, u64)]) -> Self {
        let home = std::env::temp_dir().join(format!("windroid-reader-{}", new_run_id().unwrap()));
        fs::create_dir(&home).unwrap();
        let writer = RunWriter::app(&home).unwrap();
        let id = writer.id().to_owned();
        let sink = writer.sink();
        let mut seq = 1;
        for &(day, count) in days {
            let stamp = Stamp::at(format!("{day}T12:00:00Z").parse().unwrap(), 0);
            for _ in 0..count {
                sink.capture(Event::new("windroid", format!("记录 {seq}")), || {
                    stamp.clone()
                });
                seq += 1;
            }
        }
        writer.finish("已退出").unwrap();
        let dir = run(&home, None, &id).unwrap();
        let manifest = manifest(&dir, &id, None).unwrap();
        Self {
            home,
            id,
            dir,
            manifest,
        }
    }
    fn publish(&self) {
        self.dir
            .publish("run.json", &serde_json::to_vec(&self.manifest).unwrap())
            .unwrap();
    }
    fn lines(&self, index: usize) -> Vec<Vec<u8>> {
        BufReader::new(
            self.dir
                .read(&self.manifest.segments[index].filename())
                .unwrap(),
        )
        .split(b'\n')
        .collect::<io::Result<_>>()
        .unwrap()
    }
    fn replace(&self, index: usize, bytes: &[u8]) {
        self.dir
            .publish(&self.manifest.segments[index].filename(), bytes)
            .unwrap();
    }
    fn page(&self, day: Option<&str>, after: u64, limit: usize) -> Page {
        read_page(&self.home, None, &self.id, day, after, limit).unwrap()
    }
    fn catalog(&self) -> RunInfo {
        Catalog::load(&self.home, None).unwrap().runs.pop().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
    }
}

fn complete(lines: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for l in lines {
        bytes.extend(l);
        bytes.push(b'\n')
    }
    bytes
}
fn seqs(page: &Page) -> Vec<u64> {
    page.records.iter().map(|r| r.seq).collect()
}

#[test]
fn partial_and_missing_published_tails_are_pageable_by_run_or_day() {
    let f = Fixture::new(&[(DAY_ONE, 4), (DAY_TWO, 3)]);
    let first = f.lines(0);
    let mut cut = complete(&first[..2]);
    cut.extend(&first[2][..15]);
    f.replace(0, &cut);
    let second = f.lines(1);
    f.replace(1, &complete(&second[..2]));

    let catalog = f.catalog();
    assert!(catalog.lost);
    assert_eq!(catalog.outcome.as_deref(), Some("已退出"));
    let day = f.page(Some(DAY_ONE), 0, 2);
    assert_eq!(seqs(&day), [1, 2]);
    assert!(day.more);
    let end = f.page(Some(DAY_ONE), 2, 2);
    assert_eq!(seqs(&end), [4]);
    assert!(!end.more);
    let gap = &end.records[0];
    assert_eq!(
        (gap.source.as_str(), gap.gap_start, gap.segment),
        ("gap", Some(3), 0)
    );
    assert!(gap.text.contains("3–4") && gap.text.contains("完整日志行缺失"));
    assert_eq!(gap.stamp.day, DAY_ONE);
    let day = f.page(Some(DAY_TWO), 0, 3);
    assert_eq!(seqs(&day), [5, 6, 7]);
    assert_eq!(day.records[2].gap_start, Some(7));
    assert_eq!(day.records[2].stamp.day, DAY_TWO);
    assert!(!day.more);

    let a = f.page(None, 0, 2);
    let b = f.page(None, 2, 2);
    let c = f.page(None, 5, 2);
    assert_eq!(seqs(&a), [1, 2]);
    assert_eq!(seqs(&b), [4, 5]);
    assert_eq!(seqs(&c), [6, 7]);
    assert!(a.more && b.more && !c.more);
    assert!(f.page(None, 7, 1).records.is_empty());
    assert_eq!(f.page(Some(DAY_ONE), 3, 1).records[0].gap_start, Some(4));
}

#[test]
fn missing_interior_rows_keep_later_output_and_existing_capture_gaps() {
    let f = Fixture::new(&[(DAY_ONE, 6)]);
    let lines = f.lines(0);
    let gap = Record::gap(4, 5, f.manifest.segments[0].stamp.clone(), 1, "采集缺口");
    let remaining = complete(&[
        lines[0].clone(),
        lines[2].clone(),
        serde_json::to_vec(&gap).unwrap(),
        lines[5].clone(),
    ]);
    f.replace(0, &remaining);
    for day in [None, Some(DAY_ONE)] {
        let a = f.page(day, 0, 1);
        let b = f.page(day, 1, 1);
        let c = f.page(day, 2, 1);
        let d = f.page(day, 3, 1);
        let e = f.page(day, 5, 1);
        assert_eq!(seqs(&a), [1]);
        assert_eq!(seqs(&b), [2]);
        assert_eq!(seqs(&c), [3]);
        assert_eq!(seqs(&d), [5]);
        assert_eq!(seqs(&e), [6]);
        assert_eq!((b.records[0].gap_start, b.records[0].segment), (Some(2), 0));
        assert_eq!((d.records[0].gap_start, d.records[0].segment), (Some(4), 1));
        assert!(a.more && b.more && c.more && d.more && !e.more);
    }
}

#[test]
fn active_reader_only_consumes_the_published_range() {
    let mut f = Fixture::new(&[(DAY_ONE, 4)]);
    let lines = f.lines(0);
    // 已写好第 3 行，正在写第 4 行；清单暂时只发布前两行。
    let mut bytes = complete(&lines[..3]);
    bytes.extend(&lines[3][..15]);
    f.replace(0, &bytes);
    let lock = f.dir.read("writer.lock").unwrap();
    lock.try_lock().unwrap();
    f.manifest.sealed = false;
    f.manifest.ended = None;
    f.manifest.outcome = None;
    f.manifest.segments[0].bytes = complete(&lines[..2]).len() as u64;
    f.manifest.segments[0].last = 2;
    f.publish();
    for day in [None, Some(DAY_ONE)] {
        let a = f.page(day, 0, 1);
        let b = f.page(day, 1, 1);
        assert_eq!(seqs(&a), [1]);
        assert_eq!(seqs(&b), [2]);
        assert!(a.more && !b.more);
        assert!(f.page(day, 2, 1).records.is_empty());
    }
    let catalog = f.catalog();
    assert!(catalog.active && !catalog.lost);
    f.manifest.segments[0].bytes = complete(&lines[..3]).len() as u64;
    f.manifest.segments[0].last = 3;
    f.publish();
    let next = f.page(None, 2, 1);
    assert_eq!(seqs(&next), [3]);
    assert!(!next.more);
    assert_eq!(next.records[0].source, "windroid");
}

#[test]
fn recovery_cannot_shrink_a_previously_published_sequence_range() {
    let mut f = Fixture::new(&[(DAY_ONE, 3)]);
    f.manifest.sealed = false;
    f.manifest.ended = None;
    f.manifest.outcome = None;
    f.publish();
    f.replace(0, &complete(&f.lines(0)[..1]));
    let catalog = f.catalog();
    assert!(catalog.lost && !catalog.active && catalog.outcome.is_none());
    let end = f.page(None, 1, 1);
    assert_eq!(seqs(&end), [3]);
    assert_eq!(end.records[0].gap_start, Some(2));
    assert!(!end.more);
}

#[test]
fn newline_terminated_invalid_json_preserves_the_prefix_and_following_rows() {
    let f = Fixture::new(&[(DAY_ONE, 4)]);
    let mut lines = f.lines(0);
    lines[1] = br#"{"seq":"#.to_vec();
    lines[3] = br#"{"text":"#.to_vec();
    f.replace(0, &complete(&lines));
    for day in [None, Some(DAY_ONE)] {
        let p = f.page(day, 0, 4);
        assert_eq!(seqs(&p), [1, 2, 3, 4]);
        assert!(!p.more);
        assert_eq!(
            p.records.iter().map(|r| r.gap_start).collect::<Vec<_>>(),
            [None, Some(2), None, Some(4)]
        );
        assert_eq!(p.records[2].text, "记录 3");
    }
}

#[test]
fn untrusted_row_ranges_cannot_create_an_invalid_rebuilt_index() {
    let f = Fixture::new(&[(DAY_ONE, 4)]);
    let mut lines = f.lines(0);
    let mut zero: Record = serde_json::from_slice(&lines[0]).unwrap();
    zero.seq = 0;
    lines[0] = serde_json::to_vec(&zero).unwrap();
    let mut bad_start: Record = serde_json::from_slice(&lines[1]).unwrap();
    bad_start.event.source = "gap".into();
    bad_start.gap_start = Some(0);
    lines[1] = serde_json::to_vec(&bad_start).unwrap();
    bad_start.seq = 3;
    bad_start.gap_start = Some(u64::MAX);
    lines[2] = serde_json::to_vec(&bad_start).unwrap();
    f.replace(0, &complete(&lines));
    f.dir.remove("run.json", false).unwrap();
    assert!(f.catalog().lost);
    let p = f.page(None, 0, 4);
    assert_eq!(seqs(&p), [4]);
    assert!(!p.more);
    assert!(f.page(None, u64::MAX, 1).records.is_empty());
}
