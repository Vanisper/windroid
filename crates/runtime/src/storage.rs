//! 两个资源根独立选址。选择和预览只读，确认后仅复制缺失的完整版本。
mod locks;
mod transaction;

pub(crate) use locks::{
    instance_lease, lock_staged_version, resource_lease, version_read_lease, version_write_lease,
};
pub use transaction::{apply, pending_cleanup, recover, validate_journal};
pub use windroid_locations::StorageLocations;

use crate::{Layout, instances::InstanceStore, provision::RuntimePaths};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SwitchMode {
    Future,
    Use,
    Migrate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VersionStatus {
    Reuse,
    Copy,
    Missing,
    Conflict,
    Unused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Version {
    pub id: String,
    pub name: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub status: VersionStatus,
    pub detail: String,
    pub bytes: u64,
    pub cleanup: bool,
    pub downloadable: bool,
    #[serde(rename = "downloadRequiredBytes")]
    pub download_required_bytes: Option<u64>,
    #[serde(rename = "downloadAvailableBytes")]
    pub download_available_bytes: Option<u64>,
    #[serde(rename = "downloadWritable")]
    pub download_writable: bool,
    #[serde(skip)]
    digest: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationPlan {
    pub before: StorageLocations,
    pub after: StorageLocations,
    pub mode: SwitchMode,
    pub cleanup: bool,
    pub versions: Vec<Version>,
    pub affected_instances: Vec<String>,
    pub blockers: Vec<String>,
    pub required_bytes: u64,
    pub available_bytes: Option<u64>,
}
impl MigrationPlan {
    pub fn changed_roots(&self) -> (bool, bool) {
        (
            !same_location(&self.before.runtime_root, &self.after.runtime_root),
            !same_location(&self.before.image_root, &self.after.image_root),
        )
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, message.into())
}
fn metadata(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

// 源盘可离线，此时仍保留其绝对路径。只有实际向卷申请写入时才要求挂载点可用。
fn absolute(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute()
        || path.components().any(|p| p == Component::ParentDir)
        || path
            .to_str()
            .is_none_or(|p| p.chars().any(char::is_control))
    {
        return Err(invalid("请选择不含控制字符的绝对目录"));
    }
    for ancestor in path.ancestors() {
        if metadata(ancestor)?.is_some() {
            let resolved = fs::canonicalize(ancestor)?;
            if !resolved.is_dir() {
                return Err(invalid(format!("需要目录：{}", ancestor.display())));
            }
            return Ok(resolved.join(path.strip_prefix(ancestor).unwrap()));
        }
    }
    Err(invalid("无法定位资源目录所在卷"))
}
pub(crate) fn same_location(a: &Path, b: &Path) -> bool {
    a == b || matches!((absolute(a),absolute(b)),(Ok(a),Ok(b)) if a==b)
}
pub(crate) fn check_volume(path: &Path) -> io::Result<()> {
    if let Ok(suffix) = path.strip_prefix("/Volumes")
        && let Some(Component::Normal(name)) = suffix.components().next()
        && !Path::new("/Volumes").join(name).is_dir()
    {
        return Err(io::Error::new(
            ErrorKind::NotConnected,
            format!("请先挂载存储卷 {}", name.to_string_lossy()),
        ));
    }
    Ok(())
}

fn existing_ancestor(path: &Path) -> io::Result<&Path> {
    for parent in path.ancestors() {
        if metadata(parent)?.is_some() {
            return Ok(parent);
        }
    }
    Err(invalid("找不到目标卷"))
}

/// 返回目标卷的设备号、可用字节和只读标志，不创建探测文件。
pub(crate) fn volume_space(path: &Path) -> io::Result<(u64, u64, bool)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    check_volume(path)?;
    let ancestor = existing_ancestor(path)?;
    let dev = fs::metadata(ancestor)?.dev();
    let cpath = CString::new(ancestor.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let stat = unsafe {
        if libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        stat.assume_init()
    };
    let bytes = (stat.f_bavail as u128 * stat.f_frsize as u128).min(u64::MAX as u128) as u64;
    Ok((
        dev,
        bytes,
        stat.f_flag & libc::ST_RDONLY as libc::c_ulong != 0,
    ))
}

#[derive(Debug)]
enum Contents {
    Absent,
    Complete(String),
    Conflict(String),
    Offline(String),
}

fn contents(
    root: &Path,
    path: &Path,
    verify: impl FnOnce(&Path) -> io::Result<String>,
    skeleton: impl FnOnce(&Path) -> io::Result<bool>,
) -> Contents {
    if let Err(e) = check_volume(root) {
        return Contents::Offline(e.to_string());
    }
    let inspect = || -> io::Result<Contents> {
        let parent = path.parent().ok_or_else(|| invalid("版本缺少父目录"))?;
        // 用户选择的根可以是路径别名；版本内部的链接不能把发布或清理引向其他目录。
        if absolute(parent)?
            != absolute(root)?.join(
                parent
                    .strip_prefix(root)
                    .map_err(|_| invalid("资源路径超出所选根"))?,
            )
        {
            return Err(invalid(format!("版本层级含外部链接：{}", parent.display())));
        }
        let Some(meta) = metadata(path)? else {
            return Ok(Contents::Absent);
        };
        if meta.is_dir() && skeleton(path)? {
            return Ok(Contents::Absent);
        }
        let _reader = version_read_lease(path)?;
        Ok(Contents::Complete(verify(path)?))
    };
    inspect().unwrap_or_else(|e| Contents::Conflict(format!("{}：{e}", path.display())))
}

fn classify(
    mode: SwitchMode,
    required: bool,
    source: &Contents,
    target: &Contents,
) -> (VersionStatus, String) {
    use Contents::*;
    use VersionStatus as Status;
    match (source, target) {
        (_, Offline(issue) | Conflict(issue)) => (Status::Conflict, issue.clone()),
        (Conflict(issue), _) if mode == SwitchMode::Migrate => (Status::Conflict, issue.clone()),
        (Complete(a), Complete(b)) if a != b && mode == SwitchMode::Migrate => {
            (Status::Conflict, "同一版本的展开内容不同".into())
        }
        (_, Complete(_)) => (Status::Reuse, "目标已通过完整校验".into()),
        (Complete(_), Absent) if mode == SwitchMode::Migrate => {
            (Status::Copy, "复制源位置的完整版本".into())
        }
        (Offline(issue), Absent) if required => {
            (Status::Missing, format!("{issue}，目标缺少此版本"))
        }
        (_, Absent) if required => (Status::Missing, "目标缺少所需版本".into()),
        (_, Absent) => (Status::Unused, "当前无需此版本".into()),
    }
}

fn safe_root(home: &Path, root: &Path) -> io::Result<()> {
    if !root.is_absolute()
        || root.components().any(|part| part == Component::ParentDir)
        || root
            .to_str()
            .is_none_or(|name| name.chars().any(char::is_control))
    {
        return Err(invalid("无效的资源根路径"));
    }
    if let Ok(relative) = root.strip_prefix(home)
        && let Some(Component::Normal(first)) = relative.components().next()
    {
        let name = first.to_string_lossy();
        if name.starts_with('.')
            || matches!(
                name.as_ref(),
                "instances"
                    | "preferences"
                    | "logs"
                    | "cache"
                    | "workspace-format"
                    | "storage-v2.json"
                    | "storage-cleanup-v2"
                    | "staging"
                    | "storage-locations.json"
                    | "groups.json"
            )
        {
            return Err(invalid("资源根与工作区私有目录或设置冲突"));
        }
    }
    Ok(())
}

pub fn preview(home: &Path, requested: &StorageLocations) -> io::Result<MigrationPlan> {
    preview_switch(home, requested, SwitchMode::Migrate, false)
}

pub fn preview_switch(
    home: &Path,
    requested: &StorageLocations,
    mode: SwitchMode,
    cleanup: bool,
) -> io::Result<MigrationPlan> {
    if cleanup && mode != SwitchMode::Migrate {
        return Err(invalid("只有迁移可选择清理源版本"));
    }
    let before = StorageLocations::load(home)?;
    let mut after = before.clone();
    let mut blockers = Vec::new();
    for (new, old, out) in [
        (
            &requested.runtime_root,
            &before.runtime_root,
            &mut after.runtime_root,
        ),
        (
            &requested.image_root,
            &before.image_root,
            &mut after.image_root,
        ),
    ] {
        if new == old {
            continue;
        }
        check_volume(new)?;
        *out = absolute(new)?;
        safe_root(&absolute(home)?, out)?;
    }
    let mut plan = MigrationPlan {
        before: before.clone(),
        after: after.clone(),
        mode,
        cleanup,
        versions: vec![],
        affected_instances: vec![],
        blockers: vec![],
        required_bytes: 0,
        available_bytes: None,
    };
    let changed = plan.changed_roots();
    let instances = InstanceStore::new(Layout { root: home.into() }).list_readonly()?;
    if changed != (false, false) {
        for record in &instances {
            let image = &record.spec.image;
            plan.affected_instances.push(format!(
                "{} · API {} / {} / {} / r{}",
                record.name, image.api, image.tag, image.abi, image.revision
            ));
        }
    }
    let new = RuntimePaths::managed_packages(&after);
    let old = RuntimePaths::managed_packages(&before);
    let mut disks = std::collections::BTreeMap::<u64, (u64, u64)>::new();
    if changed.1 {
        for record in &instances {
            if !new
                .iter()
                .any(|pkg| pkg.image().as_ref() == Some(&record.spec.image))
            {
                blockers.push(format!(
                    "「{}」依赖的 API {} r{} 不在当前发布清单中",
                    record.name, record.spec.image.api, record.spec.image.revision
                ));
            }
        }
    }
    for (src, dst) in old.iter().zip(&new) {
        let image = dst.image();
        if !(if image.is_some() {
            changed.1
        } else {
            changed.0
        }) {
            continue;
        }
        // 两个根可以互为父子；实际版本树不能被放进另一版本内部。
        if old.iter().chain(&new).any(|other| {
            other.path != dst.path
                && (other.path.starts_with(&dst.path) || dst.path.starts_with(&other.path))
        }) {
            return Err(invalid(format!(
                "实际版本目录发生重叠：{}",
                dst.path.display()
            )));
        }
        let (old_root, new_root) = if image.is_some() {
            (&before.image_root, &after.image_root)
        } else {
            (&before.runtime_root, &after.runtime_root)
        };
        let dependents: Vec<_> = instances
            .iter()
            .filter(|record| image.as_ref().is_none_or(|i| *i == record.spec.image))
            .map(|i| i.name.as_str())
            .collect();
        let required = !dependents.is_empty() || mode == SwitchMode::Use && image.is_none();
        let source = if mode == SwitchMode::Migrate {
            contents(
                old_root,
                &src.path,
                |path| src.verify_at(path),
                |path| src.empty_skeleton(path),
            )
        } else {
            Contents::Absent
        };
        let target = contents(
            new_root,
            &dst.path,
            |path| dst.verify_at(path),
            |path| dst.empty_skeleton(path),
        );
        let (status, mut detail) = classify(mode, required, &source, &target);
        if status == VersionStatus::Missing && !dependents.is_empty() {
            detail.push_str(&format!("，实例「{}」需要它", dependents.join("、")));
        }
        if matches!(status, VersionStatus::Missing | VersionStatus::Conflict) {
            blockers.push(format!("{}：{detail}", dst.label()));
        }
        let digest = match (&source, &target) {
            (_, Contents::Complete(value)) | (Contents::Complete(value), _) => Some(value.clone()),
            _ => None,
        };
        let bytes = dst.bytes();
        if status == VersionStatus::Copy {
            let (dev, available, read_only) = volume_space(&dst.path)?;
            if read_only {
                blockers.push(format!("目标卷为只读，无法复制 {}", dst.label()));
            }
            let totals = disks.entry(dev).or_insert((0, available));
            totals.0 = totals.0.saturating_add(bytes);
            plan.required_bytes = plan.required_bytes.saturating_add(bytes);
        }
        let downloadable = image.is_some() && matches!(target, Contents::Absent) && required;
        let download = downloadable.then(|| dst.download_space(&after));
        plan.versions.push(Version {
            id: dst.id().into(),
            name: dst.label(),
            source: src.path.clone(),
            target: dst.path.clone(),
            status,
            detail,
            bytes,
            cleanup: cleanup
                && matches!(source, Contents::Complete(_))
                && matches!(status, VersionStatus::Copy | VersionStatus::Reuse),
            downloadable,
            download_required_bytes: download.map(|d| d.0),
            download_available_bytes: download.and_then(|d| d.1),
            download_writable: download.is_some_and(|d| d.2),
            digest,
        });
    }
    for (bytes, available) in disks.values() {
        plan.available_bytes = Some(plan.available_bytes.unwrap_or(0).saturating_add(*available));
        if bytes > available {
            blockers.push(format!(
                "目标卷可用 {available} 字节，复制需要 {bytes} 字节"
            ));
        }
    }
    plan.blockers = blockers;
    Ok(plan)
}

/// 同卷排他发布，任何情况下都不以未知同名物件为替换目标。
pub(crate) fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(io::Error::new(
        ErrorKind::Unsupported,
        "此平台不支持排他版本发布",
    ));
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(test)]
mod tests;
