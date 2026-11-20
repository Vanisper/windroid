use crate::{
    Manifest, Record, Segment, Stamp,
    fs::{Dir, invalid, valid_run},
    parse_segment, valid_day,
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInfo {
    pub id: String,
    pub started: Stamp,
    pub ended: Option<Stamp>,
    pub outcome: Option<String>,
    pub active: bool,
    pub dates: Vec<String>,
    pub lost: bool,
    pub instance_id: Option<String>,
    pub app_run_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Catalog {
    pub runs: Vec<RunInfo>,
    pub legacy: bool,
}

pub(crate) fn manifest(dir: &Dir, id: &str, owner: Option<&str>) -> io::Result<Manifest> {
    let m: Manifest = dir.json("run.json")?;
    if m.format != 1
        || m.id != id
        || m.instance_id.as_deref() != owner
        || m.source != if owner.is_some() { "emulator" } else { "app" }
    {
        return Err(invalid("启动清单与所选来源不符"));
    }
    let mut number = 0;
    for seg in &m.segments {
        if !valid_day(&seg.stamp.day) || seg.number <= number || seg.first > seg.last {
            return Err(invalid("日志段索引无效"));
        }
        number = seg.number;
    }
    Ok(m)
}

fn active(dir: &Dir) -> io::Result<bool> {
    let lock = dir.read("writer.lock")?;
    match lock.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(e) => Err(e.into()),
    }
}
/// 不接受未换行的尾项；每个读取缓冲都有上限，即使文件在读到一半时损坏。
fn records(file: File, mut visit: impl FnMut(Record) -> bool) -> io::Result<()> {
    let mut input = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = input
            .by_ref()
            .take(512 << 10)
            .read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        let row: Record = serde_json::from_slice(&line)?;
        if !visit(row) {
            break;
        }
    }
    Ok(())
}
fn recover(dir: &Dir, id: &str, owner: Option<&str>, m: Option<Manifest>) -> io::Result<Manifest> {
    let mut m = m;
    let mut names: Vec<_> = dir
        .names()?
        .into_iter()
        .filter_map(|name| parse_segment(&name).map(|(n, _)| (n, name.clone())))
        .collect();
    names.sort_unstable();
    let last = m.as_ref().and_then(|m| m.segments.last().map(|s| s.number));
    for (n, name) in names {
        if last.is_some_and(|last| n < last)
            || m.as_ref()
                .is_some_and(|m| m.segments.iter().any(|s| s.number == n && s.removed))
        {
            continue;
        }
        let file = dir.read(&name)?;
        let bytes = file.metadata()?.len();
        let mut rebuilt: Option<Segment> = None;
        let mut lost = false;
        records(file, |r| {
            if r.segment != n || parse_segment(&name).is_none_or(|(_, d)| d != r.stamp.day) {
                return false;
            }
            if r.source == "gap" {
                lost = true
            }
            let seg = rebuilt.get_or_insert_with(|| Segment {
                number: n,
                first: r.seq,
                last: r.seq,
                stamp: r.stamp.clone(),
                latest_utc: r.stamp.utc.clone(),
                bytes,
                removed: false,
            });
            seg.last = r.seq;
            if seg.latest_utc < r.stamp.utc {
                seg.latest_utc = r.stamp.utc
            }
            true
        })?;
        if let Some(seg) = rebuilt {
            let m = m.get_or_insert_with(|| Manifest {
                format: 1,
                id: id.into(),
                instance_id: owner.map(str::to_owned),
                source: if owner.is_some() { "emulator" } else { "app" }.into(),
                started: seg.stamp.clone(),
                app_run_id: None,
                ended: None,
                outcome: None,
                segments: vec![],
                lost: false,
                sealed: false,
            });
            m.segments.retain(|s| s.number != n);
            m.segments.push(seg);
            m.lost |= lost;
        }
    }
    m.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "日志没有可恢复的完整记录"))
}
fn load(dir: &Dir, id: &str, owner: Option<&str>) -> io::Result<(Manifest, bool)> {
    let active = active(dir)?;
    let m = if active {
        manifest(dir, id, owner)?
    } else {
        sealed_manifest(dir, id, owner)?
    };
    Ok((m, active))
}
pub(crate) fn sealed_manifest(dir: &Dir, id: &str, owner: Option<&str>) -> io::Result<Manifest> {
    match manifest(dir, id, owner) {
        Ok(m) if m.ended.is_some() || m.sealed => Ok(m),
        Ok(m) => recover(dir, id, owner, Some(m)),
        // 明确的来源不符始终拒绝。中断的 JSON 可按段内容重建。
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.get_ref().is_some_and(|e| e.is::<serde_json::Error>()) =>
        {
            recover(dir, id, owner, None)
        }
        Err(e) => Err(e),
    }
}
impl Catalog {
    pub fn load(home: &Path, instance: Option<&str>) -> io::Result<Self> {
        let root = Dir::home(home)?;
        let legacy = if let Some(id) = instance {
            match root.instance(id)?.read("emulator.log") {
                Ok(_) => true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(e) => return Err(e),
            }
        } else {
            false
        };
        let mut runs = Vec::new();
        if let Some(dirs) = optional(root.runs(instance, false))? {
            for id in dirs.names()?.into_iter().filter(|id| valid_run(id)) {
                let Some(dir) = optional(dirs.sub(&id, false))? else {
                    continue;
                };
                let Some((m, active)) = optional(load(&dir, &id, instance))? else {
                    continue;
                };
                let dates: BTreeSet<_> = m.segments.iter().map(|s| s.stamp.day.clone()).collect();
                runs.push(RunInfo {
                    id,
                    started: m.started,
                    ended: m.ended,
                    outcome: m.outcome,
                    active,
                    dates: dates.into_iter().rev().collect(),
                    lost: m.lost || m.segments.iter().any(|s| s.removed),
                    instance_id: m.instance_id,
                    app_run_id: m.app_run_id,
                });
            }
        }
        runs.sort_by(|a, b| {
            b.started
                .utc
                .cmp(&a.started.utc)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(Self { runs, legacy })
    }
}
pub(crate) fn optional<T>(r: io::Result<T>) -> io::Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
fn run(home: &Path, instance: Option<&str>, id: &str) -> io::Result<Dir> {
    if !valid_run(id) {
        return Err(invalid("启动标识无效"));
    }
    Dir::home(home)?.runs(instance, false)?.sub(id, false)
}
#[derive(Clone, Debug, Serialize)]
pub struct Page {
    pub records: Vec<Record>,
    pub more: bool,
}
pub fn read_page(
    home: &Path,
    instance: Option<&str>,
    id: &str,
    day: Option<&str>,
    after: u64,
    limit: usize,
) -> io::Result<Page> {
    if day.is_some_and(|d| !valid_day(d)) {
        return Err(invalid("日期无效"));
    }
    let dir = run(home, instance, id)?;
    let (m, _) = load(&dir, id, instance)?;
    let limit = limit.clamp(1, 500);
    let mut rows = Vec::new();
    for seg in m
        .segments
        .iter()
        .filter(|s| s.last > after && day.is_none_or(|d| s.stamp.day == d))
    {
        let file = if seg.removed {
            None
        } else {
            optional(dir.read(&seg.filename()))?
        };
        if let Some(file) = file {
            records(file, |r| {
                if r.seq > after && r.seq <= seg.last && r.segment == seg.number {
                    rows.push(r)
                }
                rows.len() <= limit
            })?;
        } else {
            rows.push(Record::gap(
                seg.first.max(after + 1),
                seg.last,
                seg.stamp.clone(),
                seg.number,
                if seg.removed {
                    "保留期限或容量已清理此段"
                } else {
                    "此段已不可读"
                },
            ));
        }
        if rows.len() > limit {
            break;
        }
    }
    let more = rows.len() > limit;
    rows.truncate(limit);
    Ok(Page {
        records: rows,
        more,
    })
}

pub fn legacy_page(
    home: &Path,
    id: &str,
    offset: u64,
    limit: usize,
) -> io::Result<(String, u64, bool)> {
    let mut file = Dir::home(home)?.instance(id)?.read("emulator.log")?;
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(limit.clamp(1, 65536) as u64)
        .read_to_end(&mut bytes)?;
    // 翻页不要拆开合法 UTF-8 字符。旧版输出也可含任意字节。
    if let Err(e) = std::str::from_utf8(&bytes)
        && e.error_len().is_none()
        && e.valid_up_to() > 0
        && offset + (bytes.len() as u64) < size
    {
        bytes.truncate(e.valid_up_to())
    }
    let next = offset.saturating_add(bytes.len() as u64);
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        next,
        next < size,
    ))
}
pub fn reveal_path(
    home: &Path,
    instance: Option<&str>,
    id: Option<&str>,
    legacy: bool,
    seq: Option<u64>,
) -> io::Result<PathBuf> {
    let home = home.to_path_buf();
    let root = Dir::home(&home)?;
    if legacy {
        let id = instance.ok_or_else(|| invalid("旧日志需要实例标识"))?;
        root.instance(id)?.read("emulator.log")?;
        return Ok(home.join("instances").join(id).join("emulator.log"));
    }
    root.runs(instance, false)?;
    let mut path = match instance {
        None => home.join("logs/app"),
        Some(i) => home.join("instances").join(i).join("logs/emulator"),
    };
    if let Some(id) = id {
        let dir = run(&home, instance, id)?;
        let (m, _) = load(&dir, id, instance)?;
        path.push(id);
        if let Some(seq) = seq {
            let seg = m
                .segments
                .iter()
                .find(|s| s.first <= seq && seq <= s.last && !s.removed)
                .ok_or_else(|| invalid("所选日志段已清理"))?;
            let name = seg.filename();
            dir.read(&name)?;
            path.push(name);
        }
    }
    Ok(path)
}
