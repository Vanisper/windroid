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
    pub issues: Vec<String>,
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
    let mut last = 0;
    for seg in &m.segments {
        if !valid_day(&seg.stamp.day)
            || seg.number <= number
            || seg.first > seg.last
            || seg.first <= last
            || seg.number == u64::MAX
        {
            return Err(invalid("日志段索引无效"));
        }
        number = seg.number;
        last = seg.last;
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
fn records(file: impl Read, mut visit: impl FnMut(Record) -> bool) -> io::Result<bool> {
    let mut input = BufReader::new(file);
    let mut line = Vec::new();
    let mut intact = true;
    loop {
        line.clear();
        let n = input
            .by_ref()
            .take(512 << 10)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            intact = false;
            // 超长损坏行也要消费至下一行，保留它后面仍可读的记录。
            input.skip_until(b'\n')?;
            continue;
        }
        let Ok(row) = serde_json::from_slice::<Record>(&line) else {
            intact = false;
            continue;
        };
        if row.seq == 0
            || row
                .gap_start
                .is_some_and(|start| start == 0 || start > row.seq || row.source != "gap")
        {
            intact = false;
            continue;
        }
        if !visit(row) {
            break;
        }
    }
    Ok(intact)
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
        if n == u64::MAX
            || last.is_some_and(|last| n < last)
            || m.as_ref().is_some_and(|m| {
                m.segments
                    .iter()
                    .any(|s| s.number == n && (s.removed || s.filename() != name))
            })
        {
            continue;
        }
        let file = dir.read(&name)?;
        let bytes = file.metadata()?.len();
        let mut rebuilt: Option<Segment> = None;
        let mut lost = false;
        let mut previous = m
            .as_ref()
            .and_then(|m| m.segments.iter().rev().find(|s| s.number < n))
            .map_or(0, |s| s.last);
        let day = parse_segment(&name).unwrap().1;
        let intact = records(file, |r| {
            if r.segment != n || r.stamp.day != day || r.gap_start.unwrap_or(r.seq) <= previous {
                lost = true;
                return true;
            }
            lost |= r.source == "gap" || (previous != 0 && r.seq - 1 != previous);
            previous = r.seq;
            let seg = rebuilt.get_or_insert_with(|| Segment {
                number: n,
                first: r.gap_start.unwrap_or(r.seq),
                last: r.seq,
                stamp: r.stamp.clone(),
                latest_utc: r.stamp.utc.clone(),
                bytes,
                removed: false,
                unwritten: false,
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
            if let Some(published) = m.segments.iter_mut().find(|s| s.number == n) {
                // 清单可能落后于段的最后完整行，也可能在文件后来受损前已经发布更多记录。
                // 恢复只扩展已知范围，使分页仍能指出其中缺失的序号。
                lost |= seg.first > published.first
                    || seg.last < published.last
                    || seg.bytes < published.bytes;
                published.first = published.first.min(seg.first);
                published.last = published.last.max(seg.last);
                published.bytes = published.bytes.max(seg.bytes);
                if published.latest_utc < seg.latest_utc {
                    published.latest_utc = seg.latest_utc;
                }
            } else {
                m.segments.push(seg);
            }
        }
        if let Some(m) = &mut m {
            m.lost |= lost || !intact;
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
    let capturing = active && !m.sealed && m.ended.is_none();
    Ok((m, capturing))
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
        let mut issues = Vec::new();
        let legacy = if let Some(id) = instance {
            match root.instance(id)?.read("emulator.log") {
                Ok(_) => true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(e) => {
                    issues.push(format!("旧版日志不可读 · {e}"));
                    false
                }
            }
        } else {
            false
        };
        let mut runs = Vec::new();
        if let Some(dirs) = optional(root.runs(instance, false))? {
            for id in dirs.names()?.into_iter().filter(|id| valid_run(id)) {
                let (dir, m, active) = match optional(dirs.sub(&id, false).and_then(|dir| {
                    let (m, active) = load(&dir, &id, instance)?;
                    Ok((dir, m, active))
                })) {
                    Ok(Some(run)) => run,
                    Ok(None) => continue,
                    Err(e) => {
                        issues.push(format!("启动记录 {id} 不可读 · {e}"));
                        continue;
                    }
                };
                let dates: BTreeSet<_> = m.segments.iter().map(|s| s.stamp.day.clone()).collect();
                runs.push(RunInfo {
                    id,
                    started: m.started,
                    ended: m.ended,
                    outcome: m.outcome,
                    active,
                    dates: dates.into_iter().rev().collect(),
                    lost: m.lost
                        || m.segments.iter().any(|s| {
                            s.removed
                                || match dir.size(&s.filename()) {
                                    Ok(bytes) => bytes < s.bytes,
                                    Err(e) => e.kind() == io::ErrorKind::NotFound,
                                }
                        }),
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
        issues.sort();
        Ok(Self {
            runs,
            legacy,
            issues,
        })
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
/// 每页仅核对清单已经发布的范围。完整的采集缺口行自身可覆盖多个序号；
/// 合成缺口占一个分页位置，以末序号接续下一页。开头被游标截去的范围不重放。
fn read_published(
    file: File,
    seg: &Segment,
    after: u64,
    limit: usize,
    rows: &mut Vec<Record>,
) -> io::Result<()> {
    let missing = |first, last| {
        Record::gap(
            first,
            last,
            seg.stamp.clone(),
            0,
            "清单已登记，但完整日志行缺失",
        )
    };
    let mut covered = after.max(seg.first.saturating_sub(1));
    records(file.take(seg.bytes), |r| {
        if r.seq > seg.last
            || r.seq <= covered
            || r.segment != seg.number
            || r.stamp.day != seg.stamp.day
        {
            return true;
        }
        let first = if r.source == "gap" {
            // 早期格式的缺口只在文本中记载起点，末序号仍是其游标。
            r.gap_start.unwrap_or(covered + 1)
        } else {
            r.seq
        };
        if first > covered + 1 {
            rows.push(missing(covered + 1, first - 1));
        }
        covered = r.seq;
        if rows.len() <= limit {
            rows.push(r)
        }
        rows.len() <= limit && covered < seg.last
    })?;
    if rows.len() <= limit && covered < seg.last {
        rows.push(missing(covered + 1, seg.last));
    }
    Ok(())
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
            read_published(file, seg, after, limit, &mut rows)?;
        } else {
            rows.push(Record::gap(
                seg.first.max(after + 1),
                seg.last,
                seg.stamp.clone(),
                0,
                if seg.unwritten {
                    "采集缺口"
                } else if seg.removed {
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
    let mut file = open_legacy_file(home, id)?.file;
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
/// 位置用于错误提示，已校验的句柄用于文件管理器定位。两者在打开时属于同一对象。
pub struct LocatedFile {
    pub path: PathBuf,
    pub file: File,
}

pub fn open_legacy_file(home: &Path, id: &str) -> io::Result<LocatedFile> {
    let file = Dir::home(home)?.instance(id)?.read("emulator.log")?;
    Ok(LocatedFile {
        file,
        path: home.join("instances").join(id).join("emulator.log"),
    })
}

pub fn open_segment_file(
    home: &Path,
    instance: Option<&str>,
    id: &str,
    seq: u64,
) -> io::Result<LocatedFile> {
    let dir = run(home, instance, id)?;
    let (m, _) = load(&dir, id, instance)?;
    let seg = m
        .segments
        .iter()
        .find(|s| s.first <= seq && seq <= s.last && !s.removed)
        .ok_or_else(|| invalid("所选日志段已清理"))?;
    let name = seg.filename();
    let file = dir.read(&name)?;
    let root = match instance {
        None => home.join("logs/app"),
        Some(i) => home.join("instances").join(i).join("logs/emulator"),
    };
    Ok(LocatedFile {
        file,
        path: root.join(id).join(name),
    })
}

#[cfg(test)]
mod tests;
