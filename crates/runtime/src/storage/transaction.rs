use super::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
#[cfg(test)]
use windroid_locations::SETTINGS;

const JOURNAL: &str = ".storage-migration-v2.json";
const CLEANUP: &str = "storage-cleanup-v2";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Entry {
    id: String,
    digest: String,
    copy: bool,
    cleanup: bool,
    // 仅用作本次发布对象的归属证据。不同卷的资源一致性由官方展开清单核验。
    created: Option<(u64, u64)>,
    #[serde(default)]
    retained: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Journal {
    schema: u32,
    workspace: PathBuf,
    nonce: String,
    before: StorageLocations,
    after: StorageLocations,
    source_mounts: mounts::Mounts,
    target_mounts: mounts::Mounts,
    entries: Vec<Entry>,
}

fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > 256 * 1024 {
        return Err(invalid("存储事务文件类型或长度无效"));
    }
    Ok(serde_json::from_reader(file)?)
}
fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let temp = path.with_file_name(format!(".storage-writing-{}", crate::instances::nonce()?));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        serde_json::to_writer(&file, value)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(path.parent().unwrap())?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    journal: Journal,
    checksum: String,
}

fn checksum(journal: &Journal) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    Ok(base16ct::lower::encode_string(&Sha256::digest(
        serde_json::to_vec(journal)?,
    )))
}

fn read_record(path: &Path) -> io::Result<Journal> {
    let record: Record = read_json(path)?;
    if checksum(&record.journal)? != record.checksum {
        return Err(invalid("存储事务校验和不符，已保留原文件"));
    }
    Ok(record.journal)
}

fn write_record(path: &Path, journal: &Journal) -> io::Result<()> {
    write_json(
        path,
        &Record {
            journal: journal.clone(),
            checksum: checksum(journal)?,
        },
    )
}
fn write_settings(
    home: &Path,
    settings: &StorageLocations,
    mounts: &mounts::Mounts,
) -> io::Result<()> {
    #[derive(Serialize)]
    struct Selection<'a> {
        #[serde(flatten)]
        roots: &'a StorageLocations,
        mounts: &'a mounts::Mounts,
    }
    write_json(
        &home.join(windroid_locations::SETTINGS),
        &Selection {
            roots: settings,
            mounts,
        },
    )
}
fn ledger_paths(home: &Path) -> io::Result<Vec<PathBuf>> {
    let root = home.join(CLEANUP);
    let Some(meta) = metadata(&root)? else {
        return Ok(vec![]);
    };
    if !meta.is_dir() {
        return Err(invalid("待清理记录目录类型无效"));
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(b".storage-writing-")
        {
            continue;
        }
        if !entry.file_type()?.is_file() {
            return Err(invalid("未知的待清理目录项"));
        }
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}
fn check_record(home: &Path, journal: &Journal) -> io::Result<()> {
    if journal.schema != 2
        || journal.workspace != absolute(home)?
        || journal.nonce.len() != 32
        || !journal.nonce.bytes().all(|b| b.is_ascii_hexdigit())
        || journal.before == journal.after
    {
        return Err(invalid("无法确认存储事务的格式或所属工作区"));
    }
    journal.source_mounts.validate()?;
    journal.target_mounts.validate()?;
    let ids = RuntimePaths::managed_packages(&journal.after);
    let sources = RuntimePaths::managed_packages(&journal.before);
    let mut seen = std::collections::BTreeSet::new();
    for root in [
        &journal.after.runtime_root,
        &journal.after.image_root,
        &journal.before.runtime_root,
        &journal.before.image_root,
    ] {
        safe_root(&journal.workspace, root)?;
    }
    for entry in &journal.entries {
        let valid_version = ids.iter().zip(&sources).any(|(dst, src)| {
            dst.id() == entry.id && src.path != dst.path && dst.content_digest() == entry.digest
        });
        if !seen.insert(&entry.id)
            || !valid_version
            || entry.digest.len() != 64
            || !entry.digest.bytes().all(|b| b.is_ascii_hexdigit())
            || !entry.copy && entry.created.is_some()
        {
            return Err(invalid("存储事务的版本记录无效"));
        }
    }
    Ok(())
}

/// 工作区格式门禁可调用此函数。只读事务小文件，不访问外部卷、版本内容或用户盘。
pub fn validate_journal(home: &Path) -> io::Result<()> {
    mounts::load(home)?;
    let mut records = ledger_paths(home)?;
    let primary = home.join(JOURNAL);
    if metadata(&primary)?.is_some() {
        records.push(primary.clone());
    }
    for path in records {
        let journal = read_record(&path)?;
        check_record(home, &journal)?;
        if path != primary
            && !["cleanup", "finish", "undo"].into_iter().any(|phase| {
                path.file_name()
                    == Some(std::ffi::OsStr::new(&format!(
                        "{}.{phase}.json",
                        journal.nonce
                    )))
            })
        {
            return Err(invalid("待清理记录文件名与事务不符"));
        }
    }
    Ok(())
}

fn coordinator(home: &Path) -> io::Result<File> {
    let lock = locks::lock(home, ".storage.lock")?;
    lock.try_lock()
        .map_err(|_| io::Error::new(ErrorKind::WouldBlock, "另一个目录切换正在进行"))?;
    Ok(lock)
}
fn sibling(target: &Path, nonce: &str, phase: &str) -> PathBuf {
    target.with_file_name(format!(
        ".windroid-move-{nonce}-{phase}-{}",
        target.file_name().unwrap().to_string_lossy()
    ))
}

pub(super) fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        fs::set_permissions(dst, meta.permissions())?;
    } else if meta.is_file() {
        if fs::copy(src, dst)? != meta.len() {
            return Err(invalid("复制期间文件长度变化"));
        }
    } else {
        return Err(invalid(format!(
            "不支持复制链接或特殊文件：{}",
            src.display()
        )));
    }
    Ok(())
}

fn remove_scratch(path: &Path) -> io::Result<()> {
    match metadata(path)? {
        Some(m) if m.is_dir() => fs::remove_dir_all(path),
        Some(_) => Err(invalid(format!("迁移暂存目录已变化：{}", path.display()))),
        None => Ok(()),
    }
}

// 目标卷暂存都在 settings 提交前准备完毕。先记录文件对象，再逐个发布；
// 持有目标父目录协调锁、旧对象和待发布对象锁，直到设置落盘。
fn publish(home: &Path, journal: &mut Journal, leases: &mut Vec<File>) -> io::Result<()> {
    let old = RuntimePaths::managed_packages(&journal.before);
    let new = RuntimePaths::managed_packages(&journal.after);
    for entry in &mut journal.entries {
        if !entry.copy {
            continue;
        }
        let src = old.iter().find(|pkg| pkg.id() == entry.id).unwrap();
        let dst = new.iter().find(|pkg| pkg.id() == entry.id).unwrap();
        journal
            .target_mounts
            .check(&dst.path, dst.image().is_some())?;
        let stage = sibling(&dst.path, &journal.nonce, "new");
        copy_tree(&src.path, &stage)?;
        leases.push(lock_staged_version(&stage)?);
        if dst.verify_at(&stage)? != entry.digest {
            return Err(invalid("复制后的完整版本摘要不符"));
        }
        let meta = fs::symlink_metadata(&stage)?;
        entry.created = Some((meta.dev(), meta.ino()));
    }
    write_record(&home.join(JOURNAL), journal)?;
    for entry in &journal.entries {
        if !entry.copy {
            continue;
        }
        let dst = new.iter().find(|pkg| pkg.id() == entry.id).unwrap();
        journal
            .target_mounts
            .check(&dst.path, dst.image().is_some())?;
        if metadata(&dst.path)?.is_some() {
            if !dst.empty_skeleton(&dst.path)? {
                return Err(invalid(format!(
                    "目标在复制期间已被占用：{}",
                    dst.path.display()
                )));
            }
            rename_exclusive(&dst.path, &sibling(&dst.path, &journal.nonce, "empty"))?;
        }
        rename_exclusive(&sibling(&dst.path, &journal.nonce, "new"), &dst.path)?;
        File::open(dst.path.parent().unwrap())?.sync_all()?;
    }
    write_settings(home, &journal.after, &journal.target_mounts)
}

fn settle(home: &Path, journal: &Journal, locked: bool) -> io::Result<()> {
    check_record(home, journal)?;
    let current = StorageLocations::load(home)?;
    let committed = current == journal.after;
    if !committed && current != journal.before {
        return Err(invalid("存储设置与待恢复事务不符，已保留文件"));
    }
    // 设置先决定回滚还是完成；随后把磁盘回收移出活跃事务。
    // 即使目标弹出或被另一工作区打开，当前配置也可继续使用并再次切换。
    let dir = home.join(CLEANUP);
    fs::create_dir_all(&dir)?;
    let phase = if committed { "finish" } else { "undo" };
    let ledger = dir.join(format!("{}.{phase}.json", journal.nonce));
    fs::rename(home.join(JOURNAL), &ledger)?;
    if let Err(error) = settle_files(home, journal, locked, committed, &ledger) {
        log::warn!("保留迁移副本，稍后重新核对：{error}");
    }
    Ok(())
}

fn settle_files(
    home: &Path,
    journal: &Journal,
    locked: bool,
    committed: bool,
    ledger: &Path,
) -> io::Result<()> {
    let current = StorageLocations::load(home)?;
    let active = RuntimePaths::managed_packages(&current);
    let new = RuntimePaths::managed_packages(&journal.after);
    for entry in journal.entries.iter().filter(|entry| entry.copy) {
        let dst = new.iter().find(|pkg| pkg.id() == entry.id).unwrap();
        check_version_path(
            version_root(&journal.after, dst.image().is_some()),
            &dst.path,
        )?;
        let stage = sibling(&dst.path, &journal.nonce, "new");
        let backup = sibling(&dst.path, &journal.nonce, "empty");
        journal
            .target_mounts
            .check(&dst.path, dst.image().is_some())?;
        let _lease = if locked {
            vec![]
        } else {
            version_write_lease(&dst.path)?
        };
        let adopted = active.iter().any(|pkg| same_location(&pkg.path, &dst.path));
        if !committed && !adopted {
            if let Some(meta) = metadata(&dst.path)?
                && Some((meta.dev(), meta.ino())) == entry.created
            {
                if dst.verify_at(&dst.path)? != entry.digest {
                    return Err(invalid(format!(
                        "已发布副本被更改，保留在 {}",
                        dst.path.display()
                    )));
                }
                fs::remove_dir_all(&dst.path)?;
            }
            if metadata(&backup)?.is_some() {
                rename_exclusive(&backup, &dst.path)?;
            }
        } else if metadata(&backup)?.is_some() {
            if !dst.empty_skeleton(&backup)? {
                return Err(invalid(format!(
                    "原空目录已有改动，保留在 {}",
                    backup.display()
                )));
            }
            fs::remove_dir_all(backup)?;
        }
        remove_scratch(&stage)?;
    }
    if committed && journal.entries.iter().any(|e| e.cleanup) {
        let cleanup = home
            .join(CLEANUP)
            .join(format!("{}.cleanup.json", journal.nonce));
        fs::rename(ledger, cleanup)?;
    } else {
        fs::remove_file(ledger)?;
    }
    Ok(())
}

// 默认保留原版本。主动请求的清理可在源卷重新挂载或其他会话退出后复查。
// 每次删除都重新核验已启用的目标、源版本内容和实际对象的使用权。
fn retry_cleanup(home: &Path) -> io::Result<()> {
    let current = StorageLocations::load(home)?;
    for ledger in ledger_paths(home)? {
        let mut record = read_record(&ledger)?;
        check_record(home, &record)?;
        let name = ledger.file_name().unwrap().to_string_lossy();
        if !name.ends_with(".cleanup.json") {
            let committed = name.ends_with(".finish.json");
            if let Err(e) = settle_files(home, &record, false, committed, &ledger) {
                log::warn!("存储卷恢复操作仍待完成：{e}");
            }
            continue;
        }
        let old = RuntimePaths::managed_packages(&record.before);
        let new = RuntimePaths::managed_packages(&record.after);
        let active = RuntimePaths::managed_packages(&current);
        for entry in &mut record.entries {
            if !entry.cleanup || entry.retained.is_some() {
                continue;
            }
            let src = old.iter().find(|p| p.id() == entry.id).unwrap();
            let dst = new.iter().find(|p| p.id() == entry.id).unwrap();
            // 后来已经切回原位置。以前的清理授权不能删除当前版本。
            if active.iter().any(|p| same_location(&p.path, &src.path)) {
                entry.cleanup = false;
                continue;
            }
            if record
                .source_mounts
                .check(&src.path, src.image().is_some())
                .is_err()
                || record
                    .target_mounts
                    .check(&dst.path, dst.image().is_some())
                    .is_err()
            {
                continue;
            }
            if check_version_path(
                version_root(&record.before, src.image().is_some()),
                &src.path,
            )
            .is_err()
                || check_version_path(
                    version_root(&record.after, dst.image().is_some()),
                    &dst.path,
                )
                .is_err()
            {
                entry.retained = Some("版本目录所在路径已变化".into());
                continue;
            }
            let Ok(_target) = version_read_lease(&dst.path) else {
                continue;
            };
            if dst.verify_at(&dst.path).as_deref().ok() != Some(entry.digest.as_str()) {
                entry.retained = Some("目标完整性已变化".into());
                continue;
            }
            let src_root = if src.image().is_some() {
                &record.before.image_root
            } else {
                &record.before.runtime_root
            };
            if !src_root.try_exists()? {
                continue;
            }
            let retired = sibling(&src.path, &record.nonce, "cleanup");
            if !src.path.try_exists()? && !retired.try_exists()? {
                entry.cleanup = false;
                continue;
            }
            let Ok(_writer) = version_write_lease(&src.path) else {
                continue;
            };
            if !retired.try_exists()? {
                if src.verify_at(&src.path).as_deref().ok() != Some(entry.digest.as_str()) {
                    entry.retained = Some("源内容已变化".into());
                    continue;
                }
                rename_exclusive(&src.path, &retired)?;
            }
            // 版本内发生任何变化都保留。改名后在原路径新增的对象不参与清理。
            if src.verify_at(&retired).as_deref().ok() != Some(entry.digest.as_str()) {
                if !src.path.try_exists()? {
                    rename_exclusive(&retired, &src.path)?;
                }
                entry.retained = Some("清理中断或源副本发生变化".into());
                continue;
            }
            fs::remove_dir_all(retired)?;
            entry.cleanup = false;
        }
        if record.entries.iter().any(|e| e.cleanup) {
            write_record(&ledger, &record)?;
        } else {
            fs::remove_file(ledger)?;
        }
    }
    Ok(())
}
pub fn pending_cleanup(home: &Path) -> io::Result<Vec<String>> {
    let mut notes = Vec::new();
    for ledger in ledger_paths(home)? {
        let record = read_record(&ledger)?;
        check_record(home, &record)?;
        if !ledger
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".cleanup.json")
        {
            let packages = RuntimePaths::managed_packages(&record.after);
            for entry in record.entries.iter().filter(|e| e.copy) {
                let pkg = packages.iter().find(|p| p.id() == entry.id).unwrap();
                notes.push(format!(
                    "等待回收事务副本（{}；请挂载并释放该版本，若内容已更改请先核对）",
                    pkg.path.display()
                ));
            }
            continue;
        }
        let old = RuntimePaths::managed_packages(&record.before);
        for entry in record.entries.iter().filter(|e| e.cleanup) {
            let pkg = old.iter().find(|p| p.id() == entry.id).unwrap();
            let reason = entry
                .retained
                .as_deref()
                .unwrap_or("等待源卷或目标卷可用并释放占用，重新检查时重试");
            notes.push(format!("保留 {}（{}）", pkg.path.display(), reason));
        }
    }
    Ok(notes)
}

fn recover_locked(home: &Path) -> io::Result<()> {
    validate_journal(home)?;
    if metadata(&home.join(JOURNAL))?.is_some() {
        let journal = read_record(&home.join(JOURNAL))?;
        let changed = (
            !same_location(&journal.before.runtime_root, &journal.after.runtime_root),
            !same_location(&journal.before.image_root, &journal.after.image_root),
        );
        let _work = locks::workspace_locks(home, changed, true)?;
        settle(home, &journal, false)?;
    }
    retry_cleanup(home)
}
pub fn recover(home: &Path) -> io::Result<()> {
    if metadata(&home.join(JOURNAL))?.is_none() && ledger_paths(home)?.is_empty() {
        return Ok(());
    }
    let _lock = coordinator(home)?;
    recover_locked(home)
}

pub fn apply(home: &Path, confirmed: &MigrationPlan) -> io::Result<()> {
    let _lock = coordinator(home)?;
    recover_locked(home)?;
    if !confirmed.blockers.is_empty() {
        return Err(invalid(confirmed.blockers.join("；")));
    }
    let _work = locks::workspace_locks(home, confirmed.changed_roots(), true)?;
    let changed = confirmed.changed_roots();
    if changed.0 {
        confirmed
            .target_mounts
            .check(&confirmed.after.runtime_root, false)?;
    }
    if changed.1 {
        confirmed
            .target_mounts
            .check(&confirmed.after.image_root, true)?;
    }
    let mut leases = Vec::new();
    let destinations = RuntimePaths::managed_packages(&confirmed.after);
    for row in &confirmed.versions {
        let dst = destinations
            .iter()
            .find(|p| p.id() == row.id)
            .ok_or_else(|| invalid("版本已不在清单中，请重新预览"))?;
        check_version_path(
            version_root(&confirmed.after, dst.image().is_some()),
            &row.target,
        )?;
        match row.status {
            VersionStatus::Copy => {
                leases.push(version_read_lease(&row.source)?);
                leases.extend(version_write_lease(&row.target)?);
            }
            VersionStatus::Reuse => leases.push(version_read_lease(&row.target)?),
            _ => (),
        }
    }
    let plan = preview_switch(home, &confirmed.after, confirmed.mode, confirmed.cleanup)?;
    if plan.before != confirmed.before
        || plan.target_mounts != confirmed.target_mounts
        || plan.versions.len() != confirmed.versions.len()
        || plan.versions.iter().zip(&confirmed.versions).any(|(a, b)| {
            a.id != b.id
                || a.status != b.status
                || a.source != b.source
                || a.target != b.target
                || a.digest != b.digest
                || a.cleanup != b.cleanup
        })
        || plan.affected_instances != confirmed.affected_instances
        || !plan.blockers.is_empty()
    {
        return Err(invalid("资源、实例依赖或可用空间已变化，请重新预览"));
    }
    if plan.changed_roots() == (false, false) {
        return Ok(());
    }
    let mut journal = Journal {
        schema: 2,
        workspace: absolute(home)?,
        nonce: crate::instances::nonce()?,
        before: plan.before,
        after: plan.after,
        source_mounts: plan.source_mounts,
        target_mounts: plan.target_mounts,
        entries: plan
            .versions
            .into_iter()
            .filter(|row| row.cleanup || row.status == VersionStatus::Copy)
            .map(|row| Entry {
                id: row.id,
                digest: row.digest.expect("confirmed complete version"),
                copy: row.status == VersionStatus::Copy,
                cleanup: row.cleanup,
                created: None,
                retained: None,
            })
            .collect(),
    };
    if journal.entries.is_empty() {
        return write_settings(home, &journal.after, &journal.target_mounts);
    }
    write_record(&home.join(JOURNAL), &journal)?;
    let result = publish(home, &mut journal, &mut leases);
    settle(home, &journal, true)?;
    // 其他线程 fork 后可能暂时继承这些 CLOEXEC 描述符。显式释放后再尝试源清理。
    for lease in &leases {
        lease.unlock()?;
    }
    drop(leases);
    retry_cleanup(home)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "windroid-storage-journal-{}",
                crate::instances::nonce().unwrap()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn journal(home: &Path, after: StorageLocations, cleanup: bool) -> Journal {
        let src = RuntimePaths::managed_packages(&after)
            .into_iter()
            .find(|p| p.id() == "scrcpy-server")
            .unwrap();
        Journal {
            schema: 2,
            nonce: crate::instances::nonce().unwrap(),
            workspace: home.into(),
            before: StorageLocations::defaults(home),
            after,
            source_mounts: mounts::Mounts::default(),
            target_mounts: mounts::Mounts::default(),
            entries: vec![Entry {
                id: src.id().into(),
                digest: src.content_digest(),
                copy: true,
                cleanup,
                created: None,
                retained: None,
            }],
        }
    }

    #[test]
    fn malformed_transaction_is_rejected_without_creating_or_removing_any_file() {
        let home = Temp::new();
        let target = StorageLocations {
            runtime_root: home.0.join("target"),
            ..StorageLocations::defaults(&home.0)
        };
        let settings = home.0.join(SETTINGS);
        let mut damaged = serde_json::to_value(&target).unwrap();
        damaged["mounts"] = serde_json::json!({"runtime":"../invalid"});
        write_json(&settings, &damaged).unwrap();
        assert!(validate_journal(&home.0).is_err());
        fs::remove_file(settings).unwrap();
        let original = journal(&home.0, target.clone(), true);
        let primary = home.0.join(JOURNAL);
        write_record(&primary, &original).unwrap();
        let mut raw: serde_json::Value = read_json(&primary).unwrap();
        raw["journal"]["after"]["runtimeRoot"] = serde_json::json!("/elsewhere");
        write_json(&primary, &raw).unwrap();
        assert!(validate_journal(&home.0).is_err());
        assert!(primary.exists());
        assert!(!target.runtime_root.exists());
        let mut bad = original.clone();
        bad.entries[0].digest = "f".repeat(64);
        write_record(&primary, &bad).unwrap();
        assert!(validate_journal(&home.0).is_err());
        bad = original;
        bad.after.runtime_root = home.0.join("instances");
        write_record(&primary, &bad).unwrap();
        assert!(validate_journal(&home.0).is_err());
        fs::remove_file(&primary).unwrap();
        let fifo = std::ffi::CString::new(primary.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(validate_journal(&home.0).is_err());
    }

    // 通过固定的同卷事务节点模拟程序直接退出，使用完整的真实 JAR 复验。
    #[test]
    #[ignore = "set WINDROID_TEST_VERSIONED_RUNTIME to official components"]
    fn recovers_interrupted_publication_and_preserves_a_changed_or_disconnected_source() {
        let official = PathBuf::from(std::env::var_os("WINDROID_TEST_VERSIONED_RUNTIME").unwrap())
            .join("scrcpy-server/5.0.1/scrcpy-server.jar");
        for checkpoint in ["copying", "published", "committed", "modified", "offline"] {
            let temp = Temp::new();
            let home = temp.0.join("home");
            fs::create_dir(&home).unwrap();
            let after = StorageLocations {
                runtime_root: temp.0.join("target"),
                ..StorageLocations::defaults(&home)
            };
            let mut record = journal(&home, after, true);
            let src = record.before.runtime_root.join("scrcpy-server/5.0.1");
            let dst = record.after.runtime_root.join("scrcpy-server/5.0.1");
            fs::create_dir_all(&src).unwrap();
            fs::copy(&official, src.join("scrcpy-server.jar")).unwrap();
            fs::create_dir_all(&dst).unwrap();
            let stage = sibling(&dst, &record.nonce, "new");
            fs::create_dir(&stage).unwrap();
            if checkpoint == "copying" {
                fs::write(stage.join("scrcpy-server.jar"), b"partial").unwrap();
            } else {
                fs::copy(&official, stage.join("scrcpy-server.jar")).unwrap();
                let meta = fs::metadata(&stage).unwrap();
                record.entries[0].created = Some((meta.dev(), meta.ino()));
                rename_exclusive(&dst, &sibling(&dst, &record.nonce, "empty")).unwrap();
                rename_exclusive(&stage, &dst).unwrap();
            }
            if matches!(checkpoint, "committed" | "modified" | "offline") {
                write_json(&home.join(SETTINGS), &record.after).unwrap();
            }
            write_record(&home.join(JOURNAL), &record).unwrap();
            if checkpoint == "modified" {
                use std::io::Write;
                let binary = src.join("scrcpy-server.jar");
                let modified = binary.metadata().unwrap().modified().unwrap();
                let mut f = OpenOptions::new().write(true).open(&binary).unwrap();
                f.write_all(b"X").unwrap();
                f.set_modified(modified).unwrap();
            }
            if checkpoint == "offline" {
                fs::rename(&record.before.runtime_root, temp.0.join("detached-source")).unwrap();
            }
            recover(&home).unwrap();
            recover(&home).unwrap();
            assert!(
                !home.join(JOURNAL).exists() && !stage.exists(),
                "{checkpoint}"
            );
            match checkpoint {
                "copying" | "published" => {
                    assert_eq!(StorageLocations::load(&home).unwrap(), record.before);
                    assert!(src.join("scrcpy-server.jar").exists());
                    assert!(dst.is_dir() && !dst.join("scrcpy-server.jar").exists());
                }
                "committed" => {
                    assert_eq!(StorageLocations::load(&home).unwrap(), record.after);
                    assert!(!src.exists() && dst.join("scrcpy-server.jar").exists());
                }
                "modified" => {
                    assert!(
                        pending_cleanup(&home)
                            .unwrap()
                            .join("")
                            .contains("源内容已变化")
                    );
                    assert_eq!(fs::read(src.join("scrcpy-server.jar")).unwrap()[0], b'X');
                    assert_eq!(
                        fs::read(dst.join("scrcpy-server.jar")).unwrap(),
                        fs::read(&official).unwrap()
                    );
                }
                "offline" => {
                    assert_eq!(StorageLocations::load(&home).unwrap(), record.after);
                    assert!(!pending_cleanup(&home).unwrap().is_empty());
                    fs::rename(temp.0.join("detached-source"), &record.before.runtime_root)
                        .unwrap();
                    recover(&home).unwrap();
                    assert!(!src.exists() && pending_cleanup(&home).unwrap().is_empty());
                }
                _ => unreachable!(),
            }
        }
    }
}
