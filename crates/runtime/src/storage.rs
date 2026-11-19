//! 受管资源的迁移事务。所有包复制到目标卷后才切换设置；原实例目录始终留在工作区。
mod ownership;

use crate::{Layout, instances::InstanceStore, provision::RuntimePaths};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use windroid_locations::SETTINGS;
pub use windroid_locations::StorageLocations;

const JOURNAL: &str = ".storage-migration.json";
const LOCK: &str = ".storage.lock";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationFile {
    pub source: PathBuf,
    pub target: PathBuf,
    pub bytes: u64,
    #[serde(skip)]
    stamp: (u64, u64, i64, i64),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MigrationPlan {
    pub before: StorageLocations,
    pub after: StorageLocations,
    pub files: Vec<MigrationFile>,
    #[serde(skip)]
    packages: Vec<Package>,
    #[serde(skip)]
    changed: (bool, bool),
}

impl MigrationPlan {
    /// (运行组件, 镜像) 两个有效目录的变化，已合并路径别名。
    pub fn changed_roots(&self) -> (bool, bool) {
        self.changed
    }
}

pub(crate) fn check_locations(
    home: &Path,
    config: &StorageLocations,
    components: bool,
    images: bool,
    claim: bool,
) -> io::Result<()> {
    ownership::access(home, config, components, images, claim)
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

fn lock(home: &Path, name: &str) -> io::Result<File> {
    fs::create_dir_all(home)?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(home.join(name))
}

/// 下载仅持所选资源的锁，模拟器持组件和镜像锁。固定加锁顺序防止交叉等待。
pub(crate) fn resource_lease(home: &Path, components: bool, images: bool) -> io::Result<Vec<File>> {
    resource_locks(home, components, images, false)
}

fn resource_locks(
    home: &Path,
    components: bool,
    images: bool,
    moving: bool,
) -> io::Result<Vec<File>> {
    let mut leases = Vec::new();
    for (required, name, label) in [
        (components, ".components.lock", "运行组件"),
        (images, ".images.lock", "系统镜像"),
    ] {
        if required {
            let file = lock(home, name)?;
            let result = if moving {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            result.map_err(|_| {
                io::Error::new(
                    ErrorKind::WouldBlock,
                    format!("{label}正被使用，请等待准备完成、实例停止或迁移结束后重试"),
                )
            })?;
            leases.push(file);
        }
    }
    Ok(leases)
}

fn migration_lease(
    home: &Path,
    before: &StorageLocations,
    after: &StorageLocations,
) -> io::Result<Vec<File>> {
    let before = normalized(home, before)?;
    resource_locks(
        home,
        before.runtime_root != after.runtime_root,
        before.images() != after.images(),
        true,
    )
}

fn exclusive(home: &Path) -> io::Result<File> {
    let file = lock(home, LOCK)?;
    file.try_lock()
        .map_err(|_| io::Error::new(ErrorKind::WouldBlock, "已有存储迁移正在进行"))?;
    Ok(file)
}

// macOS 的临时目录可能经 /var、/tmp 别名进入。对已有祖先取真实地址，
// 也能在两个输入名称实际指向同一个目录时识别路径重叠。
fn absolute(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute()
        || path.components().any(|p| p == Component::ParentDir)
        || path
            .to_str()
            .is_none_or(|s| s.chars().any(char::is_control))
    {
        return Err(invalid("请选择没有控制字符的绝对目录"));
    }
    // 未挂载的外置盘不能被误判为空目录，或在系统卷上创建同名挂载点。
    if let Ok(suffix) = path.strip_prefix("/Volumes")
        && let Some(Component::Normal(name)) = suffix.components().next()
        && !Path::new("/Volumes").join(name).is_dir()
    {
        return Err(invalid(format!(
            "请先挂载存储卷 {}",
            name.to_string_lossy()
        )));
    }
    for ancestor in path.ancestors() {
        if let Some(meta) = metadata(ancestor)? {
            if !meta.is_dir() && !(ancestor != path && meta.is_symlink() && ancestor.is_dir()) {
                return Err(invalid(format!("需要普通目录：{}", ancestor.display())));
            }
            return Ok(ancestor
                .canonicalize()?
                .join(path.strip_prefix(ancestor).unwrap()));
        }
    }
    Err(invalid("无法确定目标卷"))
}

pub(crate) fn same_location(a: &Path, b: &Path) -> bool {
    a == b || matches!((absolute(a), absolute(b)), (Ok(left), Ok(right)) if left == right)
}

fn normalized(home: &Path, config: &StorageLocations) -> io::Result<StorageLocations> {
    let home = absolute(home)?;
    let result = StorageLocations {
        runtime_root: absolute(&config.runtime_root)?,
        image_root: config.image_root.as_deref().map(absolute).transpose()?,
    };
    for path in std::iter::once(&result.runtime_root).chain(&result.image_root) {
        if home.starts_with(path)
            || path.starts_with(home.join("instances"))
            || path.starts_with(home.join("staging"))
            || path.starts_with(home.join("cache"))
            || path.starts_with(home.join("logs"))
        {
            return Err(invalid("资源目录与主数据区冲突，请选择独立目录"));
        }
    }
    if let Some(images) = &result.image_root
        && (result.runtime_root.starts_with(images)
            || images.starts_with(result.runtime_root.join("sdk"))
                && *images != result.runtime_root.join("sdk/system-images")
            || images.starts_with(result.runtime_root.join("scrcpy-server.jar")))
    {
        return Err(invalid("镜像目录与运行组件占用的位置重叠"));
    }
    Ok(result)
}

fn inventory(source: &Path, target: &Path, files: &mut Vec<MigrationFile>) -> io::Result<()> {
    let meta = fs::symlink_metadata(source)?;
    if meta.is_dir() {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            inventory(&entry.path(), &target.join(entry.file_name()), files)?;
        }
    } else if meta.is_file() {
        files.push(MigrationFile {
            source: source.into(),
            target: target.into(),
            bytes: meta.len(),
            stamp: (meta.dev(), meta.ino(), meta.mtime(), meta.mtime_nsec()),
        });
    } else {
        return Err(invalid(format!(
            "组件中有无法迁移的链接或特殊文件：{}",
            source.display()
        )));
    }
    Ok(())
}

pub fn preview(home: &Path, requested: &StorageLocations) -> io::Result<MigrationPlan> {
    let before = StorageLocations::load(home)?;
    let current = normalized(home, &before)?;
    let old = RuntimePaths::managed_packages(&current);
    let after = normalized(home, requested)?;
    let changed = (
        current.runtime_root != after.runtime_root,
        current.images() != after.images(),
    );
    ownership::access(home, &current, changed.0, changed.1, false)?;
    ownership::access(home, &after, changed.0, changed.1, false)?;
    let new = RuntimePaths::managed_packages(&after);
    let mut files = Vec::new();
    let mut packages = Vec::new();
    for (package, dest) in old.iter().zip(&new) {
        let (source, target) = (&package.path, &dest.path);
        if source == target {
            continue;
        }
        if old
            .iter()
            .any(|pkg| target.starts_with(&pkg.path) || pkg.path.starts_with(target))
            || new
                .iter()
                .any(|pkg| source.starts_with(&pkg.path) || pkg.path.starts_with(source))
        {
            return Err(invalid("新旧资源目录重叠，请选择其他位置"));
        }
        // 子目录需同样拒绝链接（例如目标 SDK 下已有的 emulator 或 build-tools）。
        if absolute(target.parent().unwrap())? != target.parent().unwrap()
            || absolute(source.parent().unwrap())? != source.parent().unwrap()
        {
            return Err(invalid("资源目录中存在外部链接"));
        }
        if metadata(target)?.is_some() {
            return Err(invalid(format!("目标位置已存在资源：{}", target.display())));
        }
        if metadata(source)?.is_some() && package.owned()? {
            inventory(source, target, &mut files)?;
            packages.push(Package::planned(source, target)?);
        }
    }
    files.sort_by(|a, b| a.source.cmp(&b.source));
    Ok(MigrationPlan {
        before,
        after,
        files,
        packages,
        changed,
    })
}

// journal 在复制之前落盘。临时副本与最终包在同一个卷上；记录 inode 后原子发布。
// 异常退出时根据当前设置决定回滚副本或清理旧源，仅操作事务确实拥有的对象。
#[derive(Serialize, Deserialize)]
struct Journal {
    before: StorageLocations,
    after: StorageLocations,
    nonce: String,
    packages: Vec<Package>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Package {
    source: PathBuf,
    target: PathBuf,
    source_stamp: String,
    staged_stamp: Option<String>,
}

impl Package {
    fn planned(source: &Path, target: &Path) -> io::Result<Self> {
        Ok(Self {
            source: source.into(),
            target: target.into(),
            source_stamp: tree_stamp(source)?,
            staged_stamp: None,
        })
    }
}

/// 绑定目录树中每一项的名称、类型、权限、设备、inode、长度和修改时刻。
/// 比较用相对名称，因此在同卷改名、异常重开后仍可核对。
fn tree_stamp(path: &Path) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    fn visit(path: &Path, hash: &mut Sha256) -> io::Result<()> {
        let meta = fs::symlink_metadata(path)?;
        hash.update(meta.mode().to_le_bytes());
        for value in [
            meta.dev(),
            meta.ino(),
            meta.len(),
            meta.mtime() as u64,
            meta.mtime_nsec() as u64,
        ] {
            hash.update(value.to_le_bytes());
        }
        if meta.is_dir() {
            let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            hash.update((entries.len() as u64).to_le_bytes());
            for entry in entries {
                let name = entry.file_name();
                let bytes = name.as_encoded_bytes();
                hash.update((bytes.len() as u64).to_le_bytes());
                hash.update(bytes);
                visit(&entry.path(), hash)?;
            }
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    visit(path, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn identity(meta: &fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

fn scratch(target: &Path, nonce: &str) -> PathBuf {
    target.with_file_name(format!(
        ".windroid-{nonce}-{}",
        target.file_name().unwrap().to_string_lossy()
    ))
}

fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let temp = path.with_file_name(format!(".storage-writing-{}", crate::instances::nonce()?));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        serde_json::to_writer(&mut file, value)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(source)?;
    if meta.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &target.join(entry.file_name()))?;
        }
        fs::set_permissions(target, meta.permissions())?;
    } else if meta.is_file() {
        if fs::copy(source, target)? != meta.len() {
            return Err(io::Error::other(format!(
                "资源复制不完整：{}",
                source.display()
            )));
        }
    } else {
        return Err(invalid("资源类型在复制时改变，请重新预览"));
    }
    Ok(())
}

fn remove(path: &Path, expected: Option<(u64, u64)>) -> io::Result<()> {
    if let Some(meta) = metadata(path)? {
        if expected != Some(identity(&meta)) {
            return Err(invalid(format!(
                "文件已变更，保留在 {} ，请核对后重试",
                path.display()
            )));
        }
        if meta.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// 先核对整棵树，再把已确认的包改名到本事务的私有位置。随后在原路径
/// 新增的文件不会进入清理范围。若程序在删除中途退出，保留残余副本并提示。
fn retire(path: &Path, stamp: &str, nonce: &str, retained: &mut Vec<PathBuf>) -> io::Result<()> {
    let retired = scratch(path, &format!("retired-{nonce}"));
    if metadata(&retired)?.is_none() {
        if metadata(path)?.is_none() {
            return Ok(());
        }
        if tree_stamp(path)? != stamp {
            retained.push(path.into());
            return Ok(());
        }
        rename_exclusive(path, &retired)?;
    }
    if tree_stamp(&retired)? == stamp {
        return remove(&retired, Some(identity(&fs::symlink_metadata(&retired)?)));
    }
    if metadata(path)?.is_none() {
        rename_exclusive(&retired, path)?;
        retained.push(path.into());
    } else {
        retained.push(retired);
    }
    Ok(())
}

fn finish(home: &Path, journal: &Journal) -> io::Result<()> {
    let current = StorageLocations::load(home)?;
    let committed = current == journal.after;
    if !committed && current != journal.before {
        return Err(invalid("存储设置已被更改，无法确定未完成迁移的状态"));
    }
    let old = RuntimePaths::managed_packages(&normalized(home, &journal.before)?);
    let new = RuntimePaths::managed_packages(&normalized(home, &journal.after)?);
    if journal.nonce.len() != 32 || !journal.nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("迁移事务标识无效"));
    }
    let mut retained = Vec::new();
    for pkg in &journal.packages {
        if !old
            .iter()
            .zip(&new)
            .any(|(a, b)| a.path == pkg.source && b.path == pkg.target && a.path != b.path)
        {
            return Err(invalid("迁移事务的包路径无效"));
        }
        let stage = scratch(&pkg.target, &journal.nonce);
        if committed {
            if !pkg.target.try_exists()? {
                return Err(invalid(format!(
                    "新资源 {} 不可用，已保留原资源，请检查存储卷",
                    pkg.target.display()
                )));
            }
            if pkg.staged_stamp.as_deref() != Some(tree_stamp(&pkg.target)?.as_str()) {
                retained.push(pkg.source.clone());
            } else {
                retire(
                    &pkg.source,
                    &pkg.source_stamp,
                    &journal.nonce,
                    &mut retained,
                )?;
            }
        } else if let Some(stamp) = &pkg.staged_stamp {
            retire(&pkg.target, stamp, &journal.nonce, &mut retained)?;
        }
        if let Some(meta) = metadata(&stage)? {
            remove(&stage, Some(identity(&meta)))?;
        }
    }
    let inactive = if committed {
        &journal.before
    } else {
        &journal.after
    };
    ownership::release(home, inactive, &current)?;
    fs::remove_file(home.join(JOURNAL))?;
    if retained.is_empty() {
        Ok(())
    } else {
        let paths = retained
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("、");
        Err(invalid(format!(
            "目录在迁移期间发生变化，已保留副本供核对：{paths}"
        )))
    }
}

fn recover_locked(home: &Path) -> io::Result<()> {
    if let Some(meta) = metadata(&home.join(JOURNAL))? {
        if !meta.is_file() || meta.len() > 256 * 1024 {
            return Err(invalid("迁移记录无效"));
        }
        let journal: Journal = serde_json::from_reader(File::open(home.join(JOURNAL))?)?;
        let _leases = migration_lease(home, &journal.before, &journal.after)?;
        finish(home, &journal)?;
    }
    Ok(())
}

/// 冷打开时只在存在未完成事务时获取写锁；空工作区无需创建任何文件。
pub fn recover(home: &Path) -> io::Result<()> {
    if metadata(&home.join(JOURNAL))?.is_some() {
        let _lock = exclusive(home)?;
        recover_locked(home)?;
    }
    Ok(())
}

fn verify_writable(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let probe = path.join(format!(".windroid-write-{}", crate::instances::nonce()?));
    let _file = File::create_new(&probe)?;
    fs::remove_file(probe)
}

pub fn apply(home: &Path, confirmed: &MigrationPlan) -> io::Result<()> {
    let _lock = exclusive(home)?;
    recover_locked(home)?;
    let _resources = migration_lease(home, &confirmed.before, &confirmed.after)?;
    let plan = preview(home, &confirmed.after)?;
    if plan != *confirmed {
        return Err(invalid("资源或目录设置已变化，请重新预览后确认"));
    }
    if plan.before == plan.after {
        return Ok(());
    }
    if !_resources.is_empty() {
        InstanceStore::new(Layout { root: home.into() }).ensure_stopped()?;
    }
    let old = normalized(home, &plan.before)?;
    if old.runtime_root != plan.after.runtime_root {
        verify_writable(&plan.after.runtime_root)?;
    }
    if old.images() != plan.after.images() {
        verify_writable(&plan.after.images())?;
    }
    let mut journal = Journal {
        before: plan.before,
        after: plan.after,
        nonce: crate::instances::nonce()?,
        packages: plan.packages,
    };
    let record = home.join(JOURNAL);
    write_json(&record, &journal)?;
    let result = (|| {
        ownership::access(
            home,
            &old,
            plan.changed.0 && old.runtime_root.is_dir(),
            plan.changed.1 && old.images().is_dir(),
            true,
        )?;
        ownership::access(home, &journal.after, plan.changed.0, plan.changed.1, true)?;
        for index in 0..journal.packages.len() {
            let pkg = &journal.packages[index];
            fs::create_dir_all(pkg.target.parent().unwrap())?;
            let stage = scratch(&pkg.target, &journal.nonce);
            copy_tree(&pkg.source, &stage)?;
            journal.packages[index].staged_stamp = Some(tree_stamp(&stage)?);
        }
        write_json(&record, &journal)?;
        for pkg in &journal.packages {
            rename_exclusive(&scratch(&pkg.target, &journal.nonce), &pkg.target)?;
        }
        for pkg in &journal.packages {
            if tree_stamp(&pkg.source)? != pkg.source_stamp
                || Some(tree_stamp(&pkg.target)?) != pkg.staged_stamp
            {
                return Err(invalid("资源在复制期间改变，已保留原位置，请重新预览"));
            }
        }
        write_json(&home.join(SETTINGS), &journal.after)
    })();
    // 无论哪一步出错，都立即按当前设置执行相同的恢复路径。
    finish(home, &journal)?;
    result
}

pub(crate) fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        // RENAME_EXCL 保证用户在确认后新增的同名文件永远不会被覆盖。
        if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        if metadata(to)?.is_some() {
            return Err(invalid("目标位置已被占用"));
        }
        fs::rename(from, to)
    }
}

#[cfg(test)]
mod tests;
