//! 两个资源根独立选址。选择和预览只读，确认后仅复制缺失的完整版本。
mod identity;
pub(crate) use identity::{StableObject, volume_id};
mod locks;
mod mounts;
pub(crate) use mounts::check_selected_volume;
mod transaction;

pub(crate) use locks::{
    instance_lease, lock_staged_version, resource_lease, version_read_lease, version_write_lease,
};
pub(crate) use transaction::CLEANUP;
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
    #[serde(skip)]
    source_object: Option<StableObject>,
    #[serde(skip)]
    target_object: Option<StableObject>,
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
    #[serde(skip)]
    source_mounts: mounts::Mounts,
    #[serde(skip)]
    target_mounts: mounts::Mounts,
}
impl MigrationPlan {
    pub fn check_candidate_image_root(&self) -> io::Result<()> {
        self.target_mounts.require(&self.after.image_root, true)
    }

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
pub fn same_location(a: &Path, b: &Path) -> bool {
    a == b || matches!((absolute(a),absolute(b)),(Ok(a),Ok(b)) if a==b)
}
pub(crate) fn check_volume(path: &Path) -> io::Result<()> {
    if let Ok(suffix) = path.strip_prefix("/Volumes")
        && let Some(Component::Normal(name)) = suffix.components().next()
    {
        let point = Path::new("/Volumes").join(name);
        if !point.is_dir() || point.canonicalize()? != mounts::at(&point)? {
            return Err(io::Error::new(
                ErrorKind::NotConnected,
                format!("请先挂载存储卷 {}", name.to_string_lossy()),
            ));
        }
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
    let (device, available, readonly, _) = volume(path)?;
    Ok((device, available, readonly))
}

fn volume(path: &Path) -> io::Result<(u64, u64, bool, u64)> {
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
        (stat.f_frsize as u64).max(1),
    ))
}

/// 按目标卷的分配单元向上取整，并为每个目录计入一个块。
/// 同卷克隆、稀疏文件、压缩和文件系统内部元数据会影响实际增量。
fn copy_space(source: &Path, block: u64) -> io::Result<u64> {
    let meta = fs::symlink_metadata(source)?;
    if meta.is_dir() {
        let mut total = block;
        for child in fs::read_dir(source)? {
            total = total.saturating_add(copy_space(&child?.path(), block)?);
        }
        Ok(total)
    } else if meta.is_file() {
        Ok(meta.len().div_ceil(block).saturating_mul(block))
    } else {
        Err(invalid("版本在预览期间出现特殊文件，请重新预览"))
    }
}

#[derive(Debug)]
enum Contents {
    Absent,
    Skeleton(Option<StableObject>),
    Complete(String, Option<StableObject>),
    Conflict(String),
    Offline(String),
}

impl Contents {
    fn object(&self) -> Option<StableObject> {
        match self {
            Self::Complete(_, object) | Self::Skeleton(object) => object.clone(),
            _ => None,
        }
    }
    fn missing(&self) -> bool {
        matches!(self, Self::Absent | Self::Skeleton(_))
    }
}

fn check_version_path(root: &Path, path: &Path) -> io::Result<()> {
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
    Ok(())
}

fn version_root(settings: &StorageLocations, image: bool) -> &Path {
    if image {
        &settings.image_root
    } else {
        &settings.runtime_root
    }
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
        check_version_path(root, path)?;
        let Some(meta) = metadata(path)? else {
            return Ok(Contents::Absent);
        };
        if meta.is_dir() && skeleton(path)? {
            return Ok(Contents::Skeleton(StableObject::at(path).ok()));
        }
        let reader = version_read_lease(path)?;
        let id = StableObject::from_file(&reader).ok();
        let digest = verify(path)?;
        if let Some(id) = &id {
            id.require(path)?;
        }
        Ok(Contents::Complete(digest, id))
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
        (Complete(a, _), Complete(b, _)) if a != b && mode == SwitchMode::Migrate => {
            (Status::Conflict, "同一版本的展开内容不同".into())
        }
        (_, Complete(..)) => (Status::Reuse, "目标已通过完整校验".into()),
        (Complete(..), Absent | Skeleton(_)) if mode == SwitchMode::Migrate => {
            (Status::Copy, "复制源位置的完整版本".into())
        }
        (Offline(issue), Absent | Skeleton(_)) if required => {
            (Status::Missing, format!("{issue}，目标缺少此版本"))
        }
        (_, Absent | Skeleton(_)) if required => (Status::Missing, "目标缺少所需版本".into()),
        (_, Absent | Skeleton(_)) => (Status::Unused, "当前无需此版本".into()),
    }
}

fn private_name(home: &Path, name: &std::ffi::OsStr) -> io::Result<bool> {
    const PRIVATE: &[&str] = &[
        "instances",
        "preferences",
        "logs",
        "cache",
        "workspace-format",
        windroid_locations::SETTINGS,
        CLEANUP,
    ];
    let bytes = name.as_encoded_bytes();
    if bytes.starts_with(b".") || PRIVATE.iter().any(|entry| entry.as_bytes() == bytes) {
        return Ok(true);
    }
    if bytes.is_ascii()
        && !PRIVATE
            .iter()
            .any(|entry| entry.as_bytes().eq_ignore_ascii_case(bytes))
    {
        return Ok(false);
    }
    if let Some(candidate) = metadata(&home.join(name))? {
        for reserved in PRIVATE {
            if let Some(other) = metadata(&home.join(reserved))?
                && (candidate.dev(), candidate.ino()) == (other.dev(), other.ino())
            {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    // 私有目录可能尚未建立，采用所在卷的大小写规则及系统的 Unicode 比较。
    #[cfg(target_os = "macos")]
    {
        use objc2_core_foundation::{CFComparisonResult, CFString, CFStringCompareFlags as Flags};
        let parent = existing_ancestor(home)?;
        let path = std::ffi::CString::new(parent.as_os_str().as_encoded_bytes())?;
        let mut flags = Flags::CompareNonliteral;
        if unsafe { libc::pathconf(path.as_ptr(), libc::_PC_CASE_SENSITIVE) } == 0 {
            flags |= Flags::CompareCaseInsensitive;
        }
        let Some(text) = name.to_str() else {
            return Ok(false);
        };
        let name = CFString::from_str(text);
        Ok(PRIVATE.iter().any(|entry| {
            name.compare(Some(&CFString::from_str(entry)), flags)
                == CFComparisonResult::CompareEqualTo
        }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let a = metadata(&home.join("workspace-format"))?;
        let b = metadata(&home.join("WORKSPACE-FORMAT"))?;
        Ok(
            matches!((a, b), (Some(a), Some(b)) if (a.dev(), a.ino()) == (b.dev(), b.ino()))
                && PRIVATE
                    .iter()
                    .any(|entry| entry.as_bytes().eq_ignore_ascii_case(bytes)),
        )
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
    let home = absolute(home)?;
    let ancestor = existing_ancestor(&home)?;
    let info = fs::metadata(ancestor)?;
    let home_id = (ancestor == home).then(|| (info.dev(), info.ino()));
    let local_devices = [info.dev(), fs::metadata("/")?.dev()];
    let mut candidate = root.to_owned();
    // 只读本机路径元数据。逐段展开链接可在跨卷挂载点停下，不进入外卷搜索资源。
    'links: for _ in 0..=40 {
        let (mut path, rest) = match candidate.strip_prefix(&home) {
            Ok(relative) => (home.clone(), relative),
            Err(_) => (PathBuf::from("/"), candidate.as_path()),
        };
        let mut at_home = path == home;
        let mut parts = rest.components();
        while let Some(part) = parts.next() {
            match part {
                Component::Normal(name) => {
                    if at_home && private_name(&home, name)? {
                        return Err(invalid("资源根与工作区私有目录或设置冲突"));
                    }
                    path.push(name);
                }
                Component::ParentDir => {
                    path.pop();
                }
                _ => continue,
            }
            let Ok(info) = fs::symlink_metadata(&path) else {
                return Ok(());
            };
            if !local_devices.contains(&info.dev()) {
                return Ok(());
            }
            if info.is_symlink() {
                let target = fs::read_link(&path)?;
                path.pop();
                candidate = path.join(target).join(parts.as_path());
                continue 'links;
            }
            at_home = path == home || home_id == Some((info.dev(), info.ino()));
        }
        return Ok(());
    }
    // 不可解析的外部根留待实际访问时报告。
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
        safe_root(home, out)?;
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
        source_mounts: mounts::load(home)?,
        target_mounts: mounts::load(home)?,
    };
    let changed = plan.changed_roots();
    for (moves, image) in [(changed.0, false), (changed.1, true)] {
        if !moves {
            continue;
        }
        plan.target_mounts
            .capture(version_root(&after, image), image)?;
        let source = version_root(&before, image);
        // 已登记的卷离线或被同名盘替换时保留原绑定。默认根在首次迁移预览时登记。
        if mode == SwitchMode::Migrate
            && plan.source_mounts.uuid(image).is_none()
            && plan.source_mounts.check(source, image).is_ok()
        {
            plan.source_mounts.capture(source, image)?;
        }
    }
    if mode == SwitchMode::Future {
        for (moves, root) in [
            (changed.0, &after.runtime_root),
            (changed.1, &after.image_root),
        ] {
            if moves && volume_space(root)?.2 {
                blockers.push(format!(
                    "{} 所在卷只读，可选择使用已备齐的目录",
                    root.display()
                ));
            }
        }
    }
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
            if let Err(e) = plan.source_mounts.check(old_root, image.is_some()) {
                Contents::Offline(e.to_string())
            } else {
                contents(
                    old_root,
                    &src.path,
                    |path| src.verify_at(path),
                    |path| src.empty_skeleton(path),
                )
            }
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
            (_, Contents::Complete(value, _)) | (Contents::Complete(value, _), _) => {
                Some(value.clone())
            }
            _ => None,
        };
        let source_object = source.object();
        let target_object = target.object();
        let clean = cleanup
            && matches!(source, Contents::Complete(..))
            && matches!(status, VersionStatus::Copy | VersionStatus::Reuse);
        if (clean || status == VersionStatus::Copy)
            && (plan.target_mounts.uuid(image.is_some()).is_none()
                || source_object.is_none()
                || (!matches!(target, Contents::Absent) && target_object.is_none()))
        {
            blockers.push(format!(
                "{} 无法核实持久卷或版本对象身份，已保留原文件",
                dst.label()
            ));
        }
        let bytes = dst.unpacked_bytes();
        if status == VersionStatus::Copy {
            let (dev, available, read_only, block) = volume(&dst.path)?;
            let extra = copy_space(&src.path, block)?.saturating_add(2 * block);
            if read_only {
                blockers.push(format!("目标卷为只读，无法复制 {}", dst.label()));
            }
            let totals = disks.entry(dev).or_insert((0, available));
            totals.0 = totals.0.saturating_add(extra);
            plan.required_bytes = plan.required_bytes.saturating_add(extra);
        }
        let downloadable = image.is_some() && target.missing() && required;
        let download = downloadable.then(|| dst.download_space(&after));
        plan.versions.push(Version {
            id: dst.id().into(),
            name: dst.label(),
            source: src.path.clone(),
            target: dst.path.clone(),
            status,
            detail,
            bytes,
            cleanup: clean,
            source_object,
            target_object,
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
