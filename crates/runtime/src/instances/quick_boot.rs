//! 只有本次优雅退出实际更新了快照，宿主才能向用户确认已冻结。
use super::{InstanceRecord, data_dir};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

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
) -> io::Result<()> {
    if let Some(after) = stamp(dir, record)?
        && before.as_ref() != Some(&after)
    {
        let mut file = File::create_new(dir.join(RECEIPT))?;
        file.write_all(after.as_bytes())?;
    }
    Ok(())
}

pub(super) fn saved(dir: &Path, record: &InstanceRecord) -> io::Result<bool> {
    let receipt = match fs::read_to_string(dir.join(RECEIPT)) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(stamp(dir, record)?.as_ref() == Some(&receipt))
}
