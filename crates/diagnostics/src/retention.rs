//! 全库共用一把预算锁，小块预留写入空间。活跃周期的清单只有该周期的
//! 写者能修改；清理者通过同目录请求通知它回收已封存段。
use crate::{
    Manifest,
    fs::{Dir, invalid, valid_instance, valid_run},
    reader::{manifest, optional, sealed_manifest},
    writer::Disk,
};
use chrono::{SecondsFormat, Utc};
use rustix::fs::OFlags;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) segment: u64,
    pub(crate) total: u64,
    pub(crate) days: i64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            segment: 8 << 20,
            total: 256 << 20,
            days: 14,
        }
    }
}
impl Limits {
    // 小块预留使多个写者可以并行追加；每次跨过预留边界才重新计算全库预算。
    pub(crate) fn charge(&self, bytes: u64) -> u64 {
        const BLOCK: u64 = 256 << 10;
        bytes
            .div_ceil(BLOCK)
            .saturating_mul(BLOCK)
            .min(self.segment)
            .max(bytes)
    }
}
struct Candidate {
    instance: Option<String>,
    id: String,
    number: u64,
    utc: String,
    bytes: u64,
}

// 在全库协调锁内先登记即将使用的预算。若以后某个目录失去访问权，仍可
// 保护它上次登记的额度；恢复访问时以其真实文件和写者锁重新核定。
type Ledger = BTreeMap<String, BTreeMap<String, u64>>;
fn sources(root: &Dir, ledger: &Ledger) -> BTreeSet<String> {
    let mut owners = ledger.keys().cloned().collect::<BTreeSet<_>>();
    owners.insert(String::new());
    if let Ok(names) = root.sub("instances", false).and_then(|d| d.names()) {
        owners.extend(names.into_iter().filter(|n| valid_instance(n)));
    }
    owners
}
fn remove(dir: &Dir, manifest: &mut Manifest, n: u64) -> io::Result<()> {
    let Some(seg) = manifest.segments.iter_mut().find(|s| s.number == n) else {
        return Ok(());
    };
    let name = seg.filename();
    seg.removed = true;
    manifest.lost = true;
    dir.publish("run.json", &serde_json::to_vec(manifest)?)?;
    optional(dir.remove(&name, false))?;
    Ok(())
}

fn requests(dir: &Dir) -> io::Result<Vec<u64>> {
    Ok(optional(dir.json("prune.json"))?.unwrap_or_default())
}

fn expired(dir: &Dir, parent: &Dir, m: &Manifest) -> io::Result<()> {
    // 只删除本格式拥有的文件；若有陌生文件，则保留目录供检查。
    let names = dir.names()?;
    let owned = |name: &str| {
        matches!(
            name,
            "run.json" | "writer.lock" | "prune.json" | ".prune.json.tmp" | ".run.json.tmp"
        ) || m.segments.iter().any(|s| s.filename() == name)
    };
    if names.iter().all(|s| owned(s)) {
        for name in names {
            optional(dir.remove(&name, false))?;
        }
        optional(parent.remove(&m.id, true))?;
    }
    Ok(())
}
fn coordinator(home: &Path) -> io::Result<(Dir, File)> {
    let budget = Dir::home(home)?.sub(".diagnostic-budget", true)?;
    let f = budget.file("lock", OFlags::RDWR | OFlags::CREATE)?;
    f.lock()?;
    Ok((budget, f))
}
struct Inventory<'a> {
    disk: &'a Disk,
    cutoff: &'a str,
    owner: Option<&'a str>,
    runs: &'a Dir,
}
impl Inventory<'_> {
    fn inspect(&self, id: &str, dir: &Dir, candidates: &mut Vec<Candidate>) -> io::Result<u64> {
        let mine =
            id == self.disk.manifest.id && self.owner == self.disk.manifest.instance_id.as_deref();
        let lock = dir.read("writer.lock")?;
        let active = match lock.try_lock() {
            Ok(()) => false,
            Err(std::fs::TryLockError::WouldBlock) => true,
            Err(e) => return Err(e.into()),
        };
        let m = if mine {
            self.disk.manifest.clone()
        } else if active {
            manifest(dir, id, self.owner)?
        } else {
            let mut m = sealed_manifest(dir, id, self.owner)?;
            if !m.sealed && m.ended.is_none() {
                m.sealed = true;
                dir.publish("run.json", &serde_json::to_vec(&m)?)?;
            }
            m
        };
        if !active
            && m.segments.iter().all(|s| s.removed)
            && m.ended.as_ref().unwrap_or(&m.started).utc.as_str() < self.cutoff
        {
            expired(dir, self.runs, &m)?;
            return Ok(0);
        }
        let current = if mine {
            self.disk.active_segment()
        } else if active {
            m.segments.last().map(|s| s.number)
        } else {
            None
        };
        let mut total = dir.size("run.json")?;
        let managed: BTreeSet<_> = m.segments.iter().map(|s| s.filename()).collect();
        for name in dir.names()? {
            if crate::parse_segment(&name).is_some() && !managed.contains(&name) {
                total = total.saturating_add(dir.size(&name)?);
            }
        }
        let mut sealed = Vec::new();
        for seg in m.segments {
            let name = seg.filename();
            if seg.removed && (mine || !active) {
                optional(dir.remove(&name, false))?;
            }
            let size = optional(dir.size(&name))?.unwrap_or_default();
            if current == Some(seg.number) {
                total += self.disk.limit.charge(size.max(seg.bytes));
            } else {
                total += size;
                if !seg.removed && size > 0 {
                    sealed.push(Candidate {
                        instance: self.owner.map(str::to_owned),
                        id: id.into(),
                        number: seg.number,
                        utc: seg.latest_utc,
                        bytes: size,
                    });
                }
            }
        }
        candidates.append(&mut sealed);
        Ok(total)
    }
}
fn visible_bytes(dir: &Dir) -> io::Result<u64> {
    let mut bytes = 0u64;
    for name in dir.names()? {
        if name == "run.json" || crate::parse_segment(&name).is_some() {
            bytes = bytes.saturating_add(dir.size(&name)?);
        }
    }
    Ok(bytes)
}
fn reclaim(root: &Dir, disk: &mut Disk, c: &Candidate) -> io::Result<bool> {
    if c.instance == disk.manifest.instance_id && c.id == disk.manifest.id {
        remove(&disk.dir, &mut disk.manifest, c.number)?;
    } else {
        let dir = root.runs(c.instance.as_deref(), false)?.sub(&c.id, false)?;
        let lock = dir.read("writer.lock")?;
        match lock.try_lock() {
            Ok(()) => {
                let mut m = manifest(&dir, &c.id, c.instance.as_deref())?;
                remove(&dir, &mut m, c.number)?;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let mut pending = requests(&dir)?;
                if !pending.contains(&c.number) {
                    pending.push(c.number);
                    dir.publish("prune.json", &serde_json::to_vec(&pending)?)?;
                }
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}
/// 创建新段时，调用者持有返回的协调锁直到第一条记录和索引都发布。
pub(crate) fn maintain(disk: &mut Disk, additional: Option<u64>) -> io::Result<File> {
    let (budget, mut guard) = coordinator(&disk.home)?;
    let req = requests(&disk.dir)?;
    for n in &req {
        if disk.active_segment() != Some(*n) {
            remove(&disk.dir, &mut disk.manifest, *n)?
        }
    }
    if !req.is_empty() {
        optional(disk.dir.remove("prune.json", false))?;
    }
    let now = chrono::DateTime::<Utc>::from(std::time::SystemTime::now());
    if additional.is_none() {
        // 空闲时所有收集进程共用一次周期扫描。每个写者仍及时接收自己的清理请求。
        let mut buf = [0; 8];
        if guard.read_exact(&mut buf).is_ok()
            && (0..60).contains(&now.timestamp().saturating_sub(i64::from_le_bytes(buf)))
        {
            return Ok(guard);
        }
        guard.seek(SeekFrom::Start(0))?;
        guard.write_all(&now.timestamp().to_le_bytes())?;
    }
    let cutoff = (now - chrono::Duration::days(disk.limit.days))
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let root = Dir::home(&disk.home)?;
    let mut ledger: Ledger = optional(budget.json("usage.json"))?.unwrap_or_default();
    let mut candidates = Vec::new();
    for scope in sources(&root, &ledger) {
        let owner = (!scope.is_empty()).then_some(scope.as_str());
        let runs = match root.runs(owner, false) {
            Ok(runs) => runs,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // 实例目录消失才释放整项。短暂失去 instance.json 或日志子目录时，
                // 以前预留的额度仍覆盖无法验证的内容。
                if owner.is_none()
                    || owner.is_some_and(|id| {
                        root.sub("instances", false)
                            .and_then(|d| d.sub(id, false))
                            .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
                    })
                {
                    ledger.remove(&scope);
                }
                continue;
            }
            Err(_) => continue,
        };
        let Ok(names) = runs.names() else { continue };
        let ids: BTreeSet<_> = names.into_iter().filter(|id| valid_run(id)).collect();
        let charges = ledger.entry(scope.clone()).or_default();
        charges.retain(|id, _| ids.contains(id));
        let scan = Inventory {
            disk,
            cutoff: &cutoff,
            owner,
            runs: &runs,
        };
        for id in ids {
            let Ok(dir) = runs.sub(&id, false) else {
                continue;
            };
            match scan.inspect(&id, &dir, &mut candidates) {
                Ok(bytes) => {
                    charges.insert(id, bytes);
                }
                Err(_) => {
                    // 个别清单、锁或段损坏时不接手清理。能 stat 的实际文件和最近
                    // 的保留额取较大值。单个来源的权限或路径故障不阻断其余来源。
                    let charge = charges.entry(id).or_default();
                    *charge = (*charge).max(visible_bytes(&dir).unwrap_or_default());
                }
            }
        }
    }
    let mut total = ledger
        .values()
        .flat_map(|runs| runs.values())
        .fold(0u64, |acc, n| acc.saturating_add(*n));
    let reservation = additional.unwrap_or_default();
    candidates.sort_unstable_by(|a, b| a.utc.cmp(&b.utc));
    for c in candidates {
        if c.utc >= cutoff && total.saturating_add(reservation) <= disk.limit.total {
            break;
        }
        if reclaim(&root, disk, &c).unwrap_or(false) {
            total = total.saturating_sub(c.bytes);
            if let Some(n) = ledger
                .get_mut(c.instance.as_deref().unwrap_or(""))
                .and_then(|runs| runs.get_mut(&c.id))
            {
                *n = n.saturating_sub(c.bytes)
            }
        }
    }
    let full = additional.is_some() && total.saturating_add(reservation) > disk.limit.total;
    if !full {
        let charge = ledger
            .entry(disk.manifest.instance_id.clone().unwrap_or_default())
            .or_default()
            .entry(disk.manifest.id.clone())
            .or_default();
        *charge = charge.saturating_add(reservation);
    }
    budget.publish("usage.json", &serde_json::to_vec(&ledger)?)?;
    if full {
        return Err(invalid("日志额度暂时被保留数据占用"));
    }
    Ok(guard)
}
