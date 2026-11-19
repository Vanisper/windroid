//! 只有本次优雅退出实际更新了快照，宿主才能向用户确认已冻结。
use super::{InstanceRecord, data_dir};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

// 用户修改硬件后，一直跳过旧快照，直至相同配置的一次优雅退出保存新快照。
const STALE: &str = ".hardware-snapshot-stale";
const DISK_SYNCED: &str = ".guest-disk-synced";

/// 跳过快照也会跳过 Android 内存中的脏页。旧版本只确认快照写完，未确认客户机刷盘。
pub(super) fn allow_skip(dir: &Path, record: &InstanceRecord) -> io::Result<()> {
    let Some(snapshot) = stamp(dir, record)? else {
        return Ok(());
    };
    // 已因改配明确失效的同一份快照不能再正常恢复。早期空标记亦沿用其原有的跳过语义。
    if requires_cold_start(dir, record)? {
        return Ok(());
    }
    let receipt = match super::hardware::read_ini(&dir.join(DISK_SYNCED), None) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if receipt.as_ref() != Some(&snapshot) {
        return Err(super::invalid(
            "当前快照可能包含尚未写入用户分区的数据。请先正常启动以恢复快照，等设备就绪后正常停止，再修改硬件或冷启动。",
        ));
    }
    Ok(())
}

pub(super) fn requires_cold_start(dir: &Path, record: &InstanceRecord) -> io::Result<bool> {
    let marker = match super::hardware::read_ini(&dir.join(STALE), None) {
        Ok(marker) => marker,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(stamp(dir, record)?.is_some_and(|snapshot| marker.is_empty() || marker == snapshot))
}

fn write_stamp(dir: &Path, name: &str, stamp: &str) -> io::Result<()> {
    let temporary = dir.join(format!(".snapshot-stamp-{}", super::nonce()?));
    let mut file = File::create_new(&temporary)?;
    let result = (|| {
        file.write_all(stamp.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join(name))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(super) fn invalidate(dir: &Path, record: &InstanceRecord) -> io::Result<()> {
    allow_skip(dir, record)?;
    if let Some(current) = stamp(dir, record)? {
        write_stamp(dir, STALE, &current)?;
    }
    clear(dir)
}

const RECEIPT: &str = ".quick-boot-saved";

pub(super) fn clear(dir: &Path) -> io::Result<()> {
    match fs::remove_file(dir.join(RECEIPT)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub(super) fn stamp(dir: &Path, record: &InstanceRecord) -> io::Result<Option<String>> {
    let path = data_dir(dir, record).join("snapshots/default_boot/snapshot.pb");
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && m.len() > 0 => Ok(Some(format!(
            "{} {} {} {}",
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec()
        ))),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(super) fn record_save(
    dir: &Path,
    record: &InstanceRecord,
    before: Option<String>,
    synced: bool,
) -> io::Result<()> {
    let Some(after) = stamp(dir, record)? else {
        return Ok(());
    };
    if synced {
        write_stamp(dir, DISK_SYNCED, &after)?;
    } else {
        match fs::remove_file(dir.join(DISK_SYNCED)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            result => result?,
        }
    }
    if before.as_ref() != Some(&after) {
        let mut file = File::create_new(dir.join(RECEIPT))?;
        file.write_all(after.as_bytes())?;
        if dir.join(STALE).try_exists()? {
            fs::remove_file(dir.join(STALE))?;
        }
    }
    Ok(())
}

pub(super) fn saved(dir: &Path, record: &InstanceRecord) -> io::Result<bool> {
    if requires_cold_start(dir, record)? {
        return Ok(false);
    }
    let receipt = match fs::read_to_string(dir.join(RECEIPT)) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(stamp(dir, record)?.as_ref() == Some(&receipt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::{Image, InstanceId, Spec};
    use std::path::PathBuf;

    #[test]
    fn skipping_uses_the_receipt_or_invalidation_for_this_exact_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "windroid-snapshot-{}",
            super::super::nonce().unwrap()
        ));
        let record = InstanceRecord {
            id: InstanceId::new("test"),
            name: "test".into(),
            port: 5554,
            avd_name: "wd-test".into(),
            spec: Spec {
                template: "phone-1080".into(),
                image: Image {
                    api: 34,
                    revision: 14,
                    abi: concat!("arm64", "-v8a").into(),
                    tag: "google_apis_playstore".into(),
                },
                disk_bytes: 6 << 30,
            },
        };
        let snapshot: PathBuf = data_dir(&dir, &record).join("snapshots/default_boot/snapshot.pb");
        fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        allow_skip(&dir, &record).unwrap();
        fs::write(&snapshot, "old snapshot").unwrap();
        assert!(invalidate(&dir, &record).is_err());
        assert!(!requires_cold_start(&dir, &record).unwrap());
        record_save(&dir, &record, None, true).unwrap();
        invalidate(&dir, &record).unwrap();
        assert!(requires_cold_start(&dir, &record).unwrap());
        allow_skip(&dir, &record).unwrap();

        // 宿主在新快照落盘与签发凭证之间退出。新快照需正常恢复，不能被旧标记跳过。
        fs::write(&snapshot, "a newly written snapshot").unwrap();
        assert!(!requires_cold_start(&dir, &record).unwrap());
        assert!(allow_skip(&dir, &record).is_err());
        record_save(&dir, &record, None, true).unwrap();
        allow_skip(&dir, &record).unwrap();
        // 兼容只记录「这份快照已因改配失效」的早期空标记。
        fs::remove_file(dir.join(DISK_SYNCED)).unwrap();
        fs::write(dir.join(STALE), "").unwrap();
        assert!(requires_cold_start(&dir, &record).unwrap());
        allow_skip(&dir, &record).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
