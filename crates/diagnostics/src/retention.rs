//! 全库共用一把预算锁，按完整段预留写入空间。活跃周期的清单只有该周期的
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

fn sources(root: &Dir) -> io::Result<Vec<Option<String>>> {
    let mut owners = vec![None];
    if let Some(instances) = optional(root.sub("instances", false))? {
        owners.extend(
            instances
                .names()?
                .into_iter()
                .filter(|n| valid_instance(n) && root.instance(n).is_ok())
                .map(Some),
        )
    }
    Ok(owners)
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
fn coordinator(home: &Path) -> io::Result<File> {
    let logs = Dir::home(home)?.sub("logs", true)?;
    let f = logs.file(".retention.lock", OFlags::RDWR | OFlags::CREATE)?;
    f.lock()?;
    Ok(f)
}
/// 创建新段时，调用者持有返回的协调锁直到第一条记录和索引都发布。
pub(crate) fn maintain(disk: &mut Disk, additional: Option<u64>) -> io::Result<File> {
    let mut guard = coordinator(&disk.home)?;
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
    let mut total = 0;
    let mut candidates = Vec::new();
    for owner in sources(&root)? {
        let Some(runs) = optional(root.runs(owner.as_deref(), false))? else {
            continue;
        };
        for id in runs.names()?.into_iter().filter(|id| valid_run(id)) {
            let Some(dir) = optional(runs.sub(&id, false))? else {
                continue;
            };
            let Some(lock) = optional(dir.read("writer.lock"))? else {
                continue;
            };
            let mine = id == disk.manifest.id && owner == disk.manifest.instance_id;
            let active = if mine {
                true
            } else {
                match lock.try_lock() {
                    Ok(()) => false,
                    Err(std::fs::TryLockError::WouldBlock) => true,
                    Err(e) => return Err(e.into()),
                }
            };
            // 清单尚未发布的启动没有占用段预留。已完成周期取得写者锁之后才接手。
            let m = if mine {
                disk.manifest.clone()
            } else if active {
                let Some(m) = optional(manifest(&dir, &id, owner.as_deref()))? else {
                    continue;
                };
                m
            } else {
                // 崩溃可能发生在段创建与发布清单之间。封存索引后，后续预算检查
                // 可以读索引计费，且失去宿主的退出结果仍保持未知。
                let Some(mut m) = optional(sealed_manifest(&dir, &id, owner.as_deref()))? else {
                    continue;
                };
                if !m.sealed && m.ended.is_none() {
                    m.sealed = true;
                    dir.publish("run.json", &serde_json::to_vec(&m)?)?;
                }
                m
            };
            if let Some(index) = optional(dir.read("run.json"))? {
                total += index.metadata()?.len();
            }
            let current = if mine {
                disk.active_segment()
            } else if active {
                m.segments.last().map(|s| s.number)
            } else {
                None
            };
            if !active
                && m.segments.iter().all(|s| s.removed)
                && m.ended.as_ref().unwrap_or(&m.started).utc < cutoff
            {
                expired(&dir, &runs, &m)?;
                continue;
            }
            for seg in m.segments {
                if seg.removed {
                    // 上一次清理可在发布清单和 unlink 之间中断。
                    if mine || !active {
                        optional(dir.remove(&seg.filename(), false))?;
                    }
                    continue;
                }
                if current == Some(seg.number) {
                    total += disk.limit.charge(seg.bytes);
                    continue;
                }
                total += seg.bytes;
                candidates.push(Candidate {
                    instance: owner.clone(),
                    id: id.clone(),
                    number: seg.number,
                    utc: seg.latest_utc,
                    bytes: seg.bytes,
                });
            }
        }
    }
    let reservation = additional.unwrap_or_default();
    candidates.sort_unstable_by(|a, b| a.utc.cmp(&b.utc));
    for c in candidates {
        if c.utc >= cutoff && total + reservation <= disk.limit.total {
            break;
        }
        if c.instance == disk.manifest.instance_id && c.id == disk.manifest.id {
            remove(&disk.dir, &mut disk.manifest, c.number)?;
        } else {
            let Some(dir) = optional(root.runs(c.instance.as_deref(), false)?.sub(&c.id, false))?
            else {
                continue;
            };
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
                        dir.publish("prune.json", &serde_json::to_vec(&pending)?)?
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
        total = total.saturating_sub(c.bytes);
    }
    if additional.is_some() && total + reservation > disk.limit.total {
        return Err(invalid("日志额度暂时被正在写入的段占用"));
    }
    Ok(guard)
}
