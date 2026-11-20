//! 自包含的运行时准备器。自检与快照不触网。用户查看大小、来源与条款并同意后，
//! 调用 [Provisioner::start]。下载前缀、导入和解压副本均位于目标版本所在卷。
//! 克隆的句柄共享同一工作线程，最后一个句柄释放时中断传输，保留可续传前缀。

mod archive;
mod catalog;
mod download;
mod import;
mod integrity;
mod manifest;
mod metadata;
pub(crate) use manifest::Verification;
pub use manifest::preview_seed;
mod paths;
#[cfg(test)]
mod tests;

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use catalog::{Artifact, CATALOG};
use integrity::{check_file, invalid};
pub use paths::RuntimePaths;
use paths::directory;
use windroid_locations::StorageLocations;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Component {
    pub id: &'static str,
    pub name: &'static str,
    pub version: &'static str,
    pub location: PathBuf,
    pub image: Option<contract::Image>,
    pub min_emulator: Option<&'static str>,
    pub source: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
    pub license_text: &'static str,
    pub size_bytes: u64,
    pub unpacked_bytes: u64,
    pub downloaded_bytes: u64,
    pub ready: bool,
    pub integrity: &'static str,
    pub required_bytes: u64,
    pub available_bytes: Option<u64>,
    pub writable: bool,
    pub issue: Option<String>,
    /// 确认重装对应的版本对象或中断发布记录。
    pub repair_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preparation {
    /// missing / downloading / verifying / installing / paused / failed / ready。
    pub phase: &'static str,
    pub components: Vec<Component>,
    pub components_ready: bool,
    pub images: Vec<Component>,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub current_component: Option<&'static str>,
    pub error: Option<String>,
    pub candidate_target: Option<PathBuf>,
    pub candidate_downloaded_bytes: Option<u64>,
}

impl Preparation {
    fn rows_mut(&mut self) -> impl Iterator<Item = &mut Component> {
        self.components.iter_mut().chain(&mut self.images)
    }
}

struct State {
    snapshot: Preparation,
    active: bool,
}

struct Shared {
    home: PathBuf,
    catalog: &'static [Artifact],
    supported: bool,
    state: Mutex<State>,
    cancel: AtomicBool,
    /// 阻塞的 DNS/网络读取也能在 pause 时立刻被唤醒。子进程只由工作线程回收。
    transfer: Mutex<Option<Child>>,
}

struct Owner {
    worker: Arc<Shared>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.worker.request_pause();
    }
}

#[derive(Clone)]
pub struct Provisioner {
    owner: Arc<Owner>,
}

const SUPPORTED: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));
const UNSUPPORTED: &str = "当前运行时包适用于 Apple 芯片的 Mac";

impl Provisioner {
    /// 配置来自工作区，重新打开不自动继续未完成的传输。
    pub fn new(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let paths = RuntimePaths::discover(&home);
        Self::configured(home, paths, &CATALOG, SUPPORTED)
    }

    /// 强制使用指定私有目录，适用于隔离配置和集成测试。
    pub fn managed(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let paths = RuntimePaths::managed(&home);
        Self::configured(home, paths, &CATALOG, SUPPORTED)
    }

    fn configured(
        home: PathBuf,
        paths: RuntimePaths,
        catalog: &'static [Artifact],
        supported: bool,
    ) -> Self {
        let snapshot = scan(&home, &paths, catalog, supported);
        let worker = Arc::new(Shared {
            home,
            catalog,
            supported,
            state: Mutex::new(State {
                snapshot,
                active: false,
            }),
            cancel: AtomicBool::new(false),
            transfer: Mutex::new(None),
        });
        Self {
            owner: Arc::new(Owner { worker }),
        }
    }

    pub fn paths(&self) -> RuntimePaths {
        RuntimePaths::discover(&self.owner.worker.home)
    }

    pub fn snapshot(&self) -> Preparation {
        self.owner.worker.state.lock().unwrap().snapshot.clone()
    }

    /// 仅在用户已同意所列许可并主动点击时调用。重复点击不会启动两个 worker。
    pub fn start(&self) -> Result<(), String> {
        let packages = self
            .owner
            .worker
            .catalog
            .iter()
            .copied()
            .filter(|p| p.image_spec.is_none())
            .collect();
        self.launch(packages, Task::Missing, None)
    }

    pub fn start_image(&self, id: &str) -> Result<(), String> {
        self.launch(vec![self.image_package(id)?], Task::Missing, None)
    }

    /// 可能读取数 GB；宿主在后台执行预览，确认前不创建或安装任何资源。
    pub fn inspect_image_zip(&self, path: &Path) -> Result<Component, String> {
        let pkg = import::inspect(path, self.owner.worker.catalog).map_err(|e| e.to_string())?;
        self.snapshot()
            .images
            .into_iter()
            .find(|row| row.id == pkg.id)
            .ok_or_else(|| "镜像不在当前清单中".into())
    }

    pub fn import_image(&self, id: &str, path: PathBuf) -> Result<(), String> {
        self.launch(vec![self.image_package(id)?], Task::Import(path), None)
    }

    fn image_package(&self, id: &str) -> Result<Artifact, String> {
        self.owner
            .worker
            .catalog
            .iter()
            .copied()
            .find(|p| p.id == id && p.image_spec.is_some())
            .ok_or_else(|| "镜像不在当前官方清单中".into())
    }

    pub fn start_image_at(&self, id: &str, settings: &StorageLocations) -> Result<(), String> {
        self.launch(
            vec![self.image_package(id)?],
            Task::Missing,
            Some(RuntimePaths::for_locations(settings)),
        )
    }

    pub fn verify(&self, id: &str) -> Result<(), String> {
        self.launch(vec![self.package(id)?], Task::Verify, None)
    }

    pub fn repair(&self, id: &str) -> Result<(), String> {
        self.recheck()?;
        let snapshot = self.snapshot();
        let row = snapshot
            .components
            .iter()
            .chain(&snapshot.images)
            .find(|r| r.id == id)
            .ok_or_else(|| "版本不在发布清单中".to_owned())?;
        self.repair_at(id, &row.location)
    }

    pub fn repair_at(&self, id: &str, confirmed: &Path) -> Result<(), String> {
        let row = self
            .snapshot()
            .rows_mut()
            .find(|row| row.id == id)
            .cloned()
            .ok_or_else(|| "版本不在发布清单中".to_owned())?;
        let token = row
            .repair_token
            .ok_or_else(|| "请重新检查需要修复的版本".to_owned())?;
        self.repair_confirmed(id, confirmed, &token)
    }

    pub fn repair_confirmed(&self, id: &str, confirmed: &Path, token: &str) -> Result<(), String> {
        self.launch(
            vec![self.package(id)?],
            Task::Repair(confirmed.into(), token.into()),
            None,
        )
    }

    fn package(&self, id: &str) -> Result<Artifact, String> {
        self.owner
            .worker
            .catalog
            .iter()
            .copied()
            .find(|p| p.id == id)
            .ok_or_else(|| "版本不在发布清单中".into())
    }

    fn launch(
        &self,
        mut packages: Vec<Artifact>,
        task: Task,
        candidate: Option<RuntimePaths>,
    ) -> Result<(), String> {
        let shared = &self.owner.worker;
        let mut state = shared.state.lock().unwrap();
        let target = candidate
            .as_ref()
            .map(|paths| paths.destination(packages[0]));
        if state.active {
            if matches!(task, Task::Missing)
                && target == state.snapshot.candidate_target
                && packages
                    .iter()
                    .any(|p| Some(p.id) == state.snapshot.current_component)
                && !shared.cancel.load(Ordering::Acquire)
            {
                return Ok(());
            }
            return Err("另一项资源操作正在进行，请稍后再试".into());
        }
        if !shared.supported {
            return Err(UNSUPPORTED.into());
        }
        let components = packages.iter().any(|p| p.image_spec.is_none());
        let images = packages.iter().any(|p| p.image_spec.is_some());
        let candidate_job = candidate.is_some();
        let paths = candidate.unwrap_or_else(|| self.paths());
        if !candidate_job {
            paths
                .check_configuration(&shared.home, components, images)
                .map_err(|e| e.to_string())?;
        }
        // 选中已核验的版本不要求资源卷可写。批量准备只补缺失项，损坏项须逐项确认。
        // 完整核验的第一遍大文件读取也在 worker 中，按钮回调立即返回可见进度。
        if !matches!(task, Task::Verify) {
            state.snapshot = shared.scan();
        }
        let mut selected = Vec::new();
        for pkg in packages.drain(..) {
            let row = if candidate_job {
                component(&shared.home, &paths, pkg)
            } else {
                state
                    .snapshot
                    .rows_mut()
                    .find(|row| row.id == pkg.id)
                    .unwrap()
                    .clone()
            };
            match task {
                Task::Verify => {
                    fs::symlink_metadata(paths.destination(pkg))
                        .map_err(|e| format!("无法打开待核验版本：{e}"))?;
                }
                Task::Repair(ref confirmed, ref token)
                    if *confirmed != paths.destination(pkg)
                        || row.repair_token.as_ref() != Some(token) =>
                {
                    return Err("资源位置已变化，请重新确认此版本及路径".into());
                }
                Task::Repair(..) if row.integrity == "damaged" => (),
                Task::Repair(..) if row.ready => continue,
                Task::Repair(..) => {
                    return Err(row.issue.unwrap_or_else(|| "此版本不能替换".into()));
                }
                _ if row.ready => {
                    if candidate_job {
                        state.snapshot.candidate_target = target.clone();
                        state.snapshot.candidate_downloaded_bytes = Some(pkg.bytes);
                        state.snapshot.current_component = Some(pkg.id);
                        state.snapshot.phase = "ready";
                    }
                    continue;
                }
                _ if row.integrity == "missing" => (),
                _ if components && matches!(task, Task::Missing) => continue,
                _ => {
                    return Err(row
                        .issue
                        .unwrap_or_else(|| "请先处理版本冲突或确认修复".into()));
                }
            }
            selected.push(pkg);
        }
        let Some(first) = selected.first() else {
            return Ok(());
        };
        // 用户确认时绑定目标卷，覆盖后台线程稍后启动及批量下载的后续版本。
        let volume = if matches!(task, Task::Verify) {
            None
        } else {
            Some(
                crate::storage::volume_id(paths.installation_root(*first))
                    .map_err(|e| e.to_string())?,
            )
        };
        let leases = crate::storage::resource_lease(&shared.home, components, images)
            .map_err(|e| e.to_string())?;
        if !candidate_job {
            paths
                .check_configuration(&shared.home, components, images)
                .map_err(|e| e.to_string())?;
        }
        state.snapshot.current_component = Some(first.id);
        state.snapshot.candidate_downloaded_bytes =
            target.as_ref().map(|_| prefix_bytes(&paths, *first));
        state.snapshot.candidate_target = target;
        state.snapshot.error = None;
        state.snapshot.phase = if matches!(task, Task::Verify | Task::Import(_)) {
            "verifying"
        } else {
            "downloading"
        };
        state.active = true;
        shared.cancel.store(false, Ordering::Release);
        let worker = Arc::clone(shared);
        if let Err(e) = std::thread::Builder::new()
            .name("windroid-provision".into())
            .spawn(move || {
                let _leases = leases;
                worker.run(&selected, task, &paths, candidate_job, volume);
            })
        {
            state.active = false;
            state.snapshot.phase = "failed";
            state.snapshot.error = Some(e.to_string());
            return Err(e.to_string());
        }
        Ok(())
    }

    pub fn pause(&self) -> Result<(), String> {
        let shared = &self.owner.worker;
        let state = shared.state.lock().unwrap();
        if state.active {
            shared.request_pause();
        }
        Ok(())
    }

    /// 未占用目录可在另一类下载期间迁移。保留活动作业及其进度，仅更新目录与就绪状态。
    pub fn refresh_locations(&self) {
        let shared = &self.owner.worker;
        let mut state = shared.state.lock().unwrap();
        let mut fresh = shared.scan();
        if state.active {
            let current = &state.snapshot;
            fresh.phase = current.phase;
            fresh.candidate_target.clone_from(&current.candidate_target);
            fresh.candidate_downloaded_bytes = current.candidate_downloaded_bytes;
            fresh.current_component = current.current_component;
            for row in fresh.rows_mut() {
                if Some(row.id) == current.current_component
                    && let Some(old) = current
                        .components
                        .iter()
                        .chain(&current.images)
                        .find(|c| c.id == row.id)
                {
                    row.downloaded_bytes = old.downloaded_bytes;
                }
            }
            fresh.downloaded_bytes = fresh.components.iter().map(|c| c.downloaded_bytes).sum();
        }
        state.snapshot = fresh;
    }

    pub fn recheck(&self) -> Result<(), String> {
        let shared = &self.owner.worker;
        let mut state = shared.state.lock().unwrap();
        if state.active {
            return Err("请先暂停当前准备操作".into());
        }
        state.snapshot = shared.scan();
        Ok(())
    }
}

fn partial(stage: &Path, pkg: Artifact) -> PathBuf {
    stage.join(format!("{}-{}.part", pkg.id, pkg.digest.hex()))
}

enum Task {
    Missing,
    Repair(PathBuf, String),
    Verify,
    Import(PathBuf),
}

fn prefix_bytes(paths: &RuntimePaths, pkg: Artifact) -> u64 {
    let stage = archive::stage_path(&paths.destination(pkg), pkg);
    if directory(paths.installation_root(pkg), &stage, false).is_err()
        || !archive::is_workspace(&stage)
    {
        return 0;
    }
    download::part_len(&partial(&stage, pkg))
        .ok()
        .filter(|n| *n <= pkg.bytes)
        .unwrap_or(0)
}

fn repair_token(dest: &Path, pkg: Artifact) -> io::Result<String> {
    let stage = archive::stage_path(dest, pkg);
    let journal = stage.join("publish.json");
    let interrupted = archive::is_workspace(&stage) && journal.try_exists()?;
    let identity = crate::storage::StableObject::at(if interrupted { &journal } else { dest })?;
    Ok(serde_json::to_string(&(interrupted, identity))?)
}

fn check_repair(dest: &Path, pkg: Artifact, expected: &str) -> io::Result<()> {
    if repair_token(dest, pkg)? != expected {
        return Err(invalid("待修复版本或存储卷已更换，请重新检查并确认"));
    }
    Ok(())
}

fn component(home: &Path, paths: &RuntimePaths, pkg: Artifact) -> Component {
    let location = paths.destination(pkg);
    let stage = archive::stage_path(&location, pkg);
    let checked = (|| {
        if let Some(error) = &paths.config_error {
            return Err(io::Error::new(ErrorKind::InvalidInput, error.clone()));
        }
        crate::storage::check_selected_volume(home, paths.installation_root(pkg))?;
        if archive::is_workspace(&stage) && stage.join("publish.json").try_exists()? {
            for existing in [&location, &stage.join("previous")] {
                if fs::symlink_metadata(existing).is_ok() {
                    manifest::repairable(existing, pkg)?;
                }
            }
            return Err(invalid("此版本的发布中断，请确认恢复此版本"));
        }
        directory(
            paths.installation_root(pkg),
            location.parent().unwrap(),
            false,
        )?;
        let lease = crate::storage::version_read_lease(&location)?;
        let result = match manifest::empty_skeleton(&location, pkg) {
            Ok(true) => Ok("missing"),
            Ok(false) => manifest::check(home, &location, pkg, Verification::Cached, || Ok(())),
            Err(e) => Err(e),
        };
        let _ = lease.unlock();
        result
    })();
    let (integrity, issue) = match checked {
        Ok(mode) => (mode, None),
        Err(e)
            if e.kind() == ErrorKind::NotFound
                && fs::symlink_metadata(&location)
                    .is_err_and(|e| e.kind() == ErrorKind::NotFound)
                && crate::storage::check_selected_volume(home, paths.installation_root(pkg))
                    .is_ok() =>
        {
            ("missing", None)
        }
        Err(e)
            if e.kind() == ErrorKind::AlreadyExists
                || !fs::symlink_metadata(&location).is_ok_and(|m| m.is_dir())
                    && !stage.join("publish.json").is_file() =>
        {
            ("conflict", Some(e.to_string()))
        }
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::InvalidData | ErrorKind::NotFound | ErrorKind::UnexpectedEof
            ) =>
        {
            ("damaged", Some(e.to_string()))
        }
        Err(e) => ("conflict", Some(e.to_string())),
    };
    let ready = matches!(integrity, "verified" | "quick");
    let downloaded_bytes = if ready {
        pkg.bytes
    } else {
        prefix_bytes(paths, pkg)
    };
    let (license, license_url, license_text) = pkg.license();
    let volume = crate::storage::check_selected_volume(home, paths.installation_root(pkg))
        .and_then(|_| volume_info(location.parent().unwrap()))
        .ok();
    let repair_token = if integrity == "damaged" {
        repair_token(&location, pkg).ok()
    } else {
        None
    };
    Component {
        repair_token,
        id: pkg.id,
        name: pkg.name,
        version: pkg.version,
        location,
        image: pkg.image(),
        min_emulator: pkg.image_spec.map(|spec| spec.min_emulator),
        source: pkg.url,
        license,
        license_url,
        license_text,
        size_bytes: pkg.bytes,
        unpacked_bytes: pkg.unpacked,
        downloaded_bytes,
        ready,
        issue,
        integrity,
        required_bytes: if ready {
            0
        } else {
            required_bytes(pkg, downloaded_bytes)
        },
        available_bytes: volume.map(|v| v.0),
        writable: volume.is_some_and(|v| v.1),
    }
}

fn required_bytes(pkg: Artifact, prefix: u64) -> u64 {
    pkg.bytes
        .saturating_sub(prefix)
        .saturating_add(pkg.unpacked)
        .saturating_add(4 * 1024 * 1024)
}

fn check_capacity(parent: &Path, pkg: Artifact, prefix: u64) -> io::Result<()> {
    let (available, writable) = volume_info(parent)?;
    if !writable {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("目标位置不可写：{}", parent.display()),
        ));
    }
    let required = required_bytes(pkg, prefix);
    if required > available {
        return Err(io::Error::new(
            ErrorKind::StorageFull,
            format!(
                "目标卷空间不足，需要 {required} 字节，可用 {available} 字节：{}",
                parent.display()
            ),
        ));
    }
    Ok(())
}

fn volume_info(path: &Path) -> io::Result<(u64, bool)> {
    use std::ffi::CString;
    crate::storage::check_volume(path)?;
    let mut parent = path;
    while !parent.try_exists()? {
        parent = parent.parent().ok_or_else(|| invalid("找不到目标卷"))?;
    }
    if !fs::metadata(parent)?.is_dir() {
        return Err(invalid(format!("目标不是目录：{}", parent.display())));
    }
    let name = CString::new(parent.as_os_str().as_encoded_bytes())?;
    let (_, available, read_only) = crate::storage::volume_space(parent)?;
    // SAFETY: access 只在系统调用期间读取 NUL 结尾的路径。
    let writable = unsafe { libc::access(name.as_ptr(), libc::W_OK | libc::X_OK) == 0 };
    Ok((available, !read_only && writable))
}

fn scan(home: &Path, paths: &RuntimePaths, catalog: &[Artifact], supported: bool) -> Preparation {
    let (images, components): (Vec<_>, Vec<_>) = catalog
        .iter()
        .map(|&pkg| component(home, paths, pkg))
        .partition(|row| row.image.is_some());
    let components_ready = supported && components.iter().all(|c| c.ready);
    let phase = if !supported {
        "failed"
    } else if components_ready {
        "ready"
    } else if components
        .iter()
        .any(|c| !c.ready && c.downloaded_bytes > 0)
    {
        "paused"
    } else {
        "missing"
    };
    Preparation {
        phase,
        downloaded_bytes: components.iter().map(|c| c.downloaded_bytes).sum(),
        total_bytes: components.iter().map(|c| c.size_bytes).sum(),
        components,
        images,
        components_ready,
        current_component: None,
        error: (!supported).then(|| UNSUPPORTED.into()),
        candidate_target: None,
        candidate_downloaded_bytes: None,
    }
}

impl Shared {
    fn scan(&self) -> Preparation {
        scan(
            &self.home,
            &RuntimePaths::discover(&self.home),
            self.catalog,
            self.supported,
        )
    }

    fn request_pause(&self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(child) = self.transfer.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    fn check_pause(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(io::Error::new(ErrorKind::Interrupted, "准备已暂停"))
        } else {
            Ok(())
        }
    }

    fn progress(&self, pkg: Artifact, bytes: u64) {
        let mut state = self.state.lock().unwrap();
        let snapshot = &mut state.snapshot;
        if snapshot.candidate_target.is_some() {
            snapshot.candidate_downloaded_bytes = Some(bytes);
            return;
        }
        if let Some(c) = snapshot.rows_mut().find(|c| c.id == pkg.id) {
            c.downloaded_bytes = bytes;
        }
        snapshot.downloaded_bytes = snapshot
            .components
            .iter()
            .map(|row| row.downloaded_bytes)
            .sum();
    }

    fn phase(&self, pkg: Artifact, phase: &'static str) {
        let mut state = self.state.lock().unwrap();
        state.snapshot.phase = phase;
        state.snapshot.current_component = Some(pkg.id);
    }

    fn run(
        &self,
        packages: &[Artifact],
        task: Task,
        paths: &RuntimePaths,
        candidate: bool,
        volume: Option<[u8; 16]>,
    ) {
        let result = packages
            .iter()
            .try_for_each(|&pkg| self.prepare(pkg, &task, paths, candidate, volume));
        let mut snap = self.scan();
        let result = result.and_then(|()| {
            let unexpected = if candidate {
                let row = component(&self.home, paths, packages[0]);
                (!row.ready).then_some(row)
            } else {
                snap.components
                    .iter()
                    .chain(&snap.images)
                    .find(|row| !row.ready && packages.iter().any(|p| p.id == row.id))
                    .cloned()
            };
            match unexpected {
                Some(row) => Err(invalid(format!(
                    "发布后复查 {} 失败：{}",
                    row.location.display(),
                    row.issue.as_deref().unwrap_or("找不到所选版本")
                ))),
                None => Ok(()),
            }
        });
        let mut state = self.state.lock().unwrap();
        let current = state.snapshot.current_component;
        if candidate {
            snap.current_component = current;
            snap.candidate_target = state.snapshot.candidate_target.clone();
            let pkg = packages[0];
            snap.candidate_downloaded_bytes = Some(if result.is_ok() {
                pkg.bytes
            } else {
                prefix_bytes(paths, pkg)
            });
            snap.phase = "ready";
        } else if result.is_ok() {
            for row in snap
                .rows_mut()
                .filter(|row| row.ready && packages.iter().any(|p| p.id == row.id))
            {
                row.integrity = "verified";
            }
        }
        if let Err(e) = result {
            snap.current_component = current;
            snap.phase =
                if e.kind() == ErrorKind::Interrupted && self.cancel.load(Ordering::Acquire) {
                    "paused"
                } else {
                    "failed"
                };
            if snap.phase == "failed" {
                let reason = match e.raw_os_error() {
                    Some(libc::EIO | libc::ENXIO | libc::ENODEV | libc::ESTALE) => {
                        format!("目标卷读写失败，请重新挂载后手动继续。{e}")
                    }
                    Some(libc::ENOSPC | libc::EDQUOT) => {
                        format!("目标卷可用空间不足，请释放空间后手动继续。{e}")
                    }
                    _ => e.to_string(),
                };
                snap.error = Some(reason);
            }
        }
        state.active = false;
        state.snapshot = snap;
    }

    fn prepare(
        &self,
        pkg: Artifact,
        task: &Task,
        paths: &RuntimePaths,
        candidate: bool,
        volume: Option<[u8; 16]>,
    ) -> io::Result<()> {
        self.check_pause()?;
        self.phase(
            pkg,
            if matches!(task, Task::Import(_) | Task::Verify) {
                "verifying"
            } else {
                "downloading"
            },
        );
        let dest = paths.destination(pkg);
        let parent = dest.parent().expect("version directory");
        if matches!(task, Task::Verify) {
            self.phase(pkg, "verifying");
            let lease = crate::storage::version_read_lease(&dest)?;
            let result = manifest::check(&self.home, &dest, pkg, Verification::Full, || {
                self.check_pause()
            });
            let _ = lease.unlock();
            return result.map(|_| ());
        }
        let check = || {
            self.check_pause()?;
            crate::storage::check_volume(&dest)
        };
        check()?;
        if !candidate {
            paths.check_configuration(
                &self.home,
                pkg.image_spec.is_none(),
                pkg.image_spec.is_some(),
            )?;
        }
        let selected_volume = || {
            if Some(crate::storage::volume_id(parent)?) != volume {
                return Err(io::Error::new(
                    ErrorKind::NotConnected,
                    "目标卷已更换，请重新选择并确认",
                ));
            }
            if candidate {
                crate::storage::check_volume(&dest)
            } else {
                crate::storage::check_selected_volume(&self.home, paths.installation_root(pkg))
            }
        };
        selected_volume()?;
        let prefix = if matches!(task, Task::Import(_)) {
            0
        } else {
            prefix_bytes(paths, pkg)
        };
        let stage_path = archive::stage_path(&dest, pkg);
        let recovering = archive::is_workspace(&stage_path)
            && (stage_path.join("publish.json").try_exists()?
                || stage_path.join("unpack").try_exists()?);
        // 已经换入的新版本只需复查和回收备份，不要求再腾出一份完整解压空间。
        if !recovering {
            check_capacity(parent, pkg, prefix)?;
        }
        selected_volume()?;
        if let Task::Repair(_, expected) = task {
            check_repair(&dest, pkg, expected)?;
        }
        directory(paths.installation_root(pkg), parent, true)?;
        let mut leases =
            crate::storage::OperationLocks(crate::storage::version_write_lease(&dest)?);
        let (stage, _lock) = archive::workspace(&dest, pkg)?;
        use std::os::unix::fs::MetadataExt;
        let owner = _lock.metadata()?;
        // 持有的文件描述符固定原卷上的对象。强制弹出后即使同名挂载点重现，也不能继续发布。
        let guard = || {
            check()?;
            if Some(crate::storage::volume_id(&stage)?) != volume {
                return Err(io::Error::new(
                    ErrorKind::NotConnected,
                    "安装暂存所在卷已更换",
                ));
            }
            let current = fs::symlink_metadata(stage.join("owner"))?;
            if (owner.dev(), owner.ino()) != (current.dev(), current.ino()) {
                return Err(io::Error::new(
                    ErrorKind::NotConnected,
                    "目标卷或暂存位置已更换",
                ));
            }
            Ok(())
        };
        guard()?;
        selected_volume()?;
        if let Task::Repair(_, expected) = task {
            check_repair(&dest, pkg, expected)?;
        }
        if stage.join("publish.json").try_exists()? && !matches!(task, Task::Repair(..)) {
            return Err(invalid("此版本有中断的发布，请重新检查并确认恢复"));
        }
        if let Some(restored) = archive::recover(&self.home, &stage, &dest, pkg, &guard)? {
            leases.0.push(restored);
        }
        let original = archive::object(&dest)?;
        if original.is_some() {
            if manifest::check(&self.home, &dest, pkg, Verification::Cached, || {
                self.check_pause()
            })
            .is_ok()
            {
                guard()?;
                let part = partial(&stage, pkg);
                if download::part_len(&part)? > 0 {
                    fs::remove_file(part)?;
                }
                self.progress(pkg, pkg.bytes);
                return Ok(());
            }
            manifest::repairable(&dest, pkg)?;
            if !matches!(task, Task::Repair(..)) && !manifest::empty_skeleton(&dest, pkg)? {
                return Err(invalid(format!(
                    "{} 已损坏，请确认重新安装此版本",
                    dest.display()
                )));
            }
        }
        if recovering {
            guard()?;
            archive::discard_unpack(&stage)?;
            check_capacity(parent, pkg, prefix)?;
        }
        let part = partial(&stage, pkg);
        let input = if let Task::Import(source) = task {
            self.phase(pkg, "verifying");
            import::capture(self, &stage, source, pkg)?
        } else {
            self.phase(pkg, "downloading");
            download::fetch(self, pkg, &part, &guard)?;
            self.progress(pkg, pkg.bytes);
            fs::File::open(&part)?
        };
        self.phase(pkg, "verifying");
        guard()?;
        if let Err(e) = check_file(&input, pkg, || self.check_pause()) {
            if !matches!(task, Task::Import(_)) && e.kind() == ErrorKind::InvalidData {
                guard()?;
                fs::remove_file(&part)?;
            }
            return Err(e);
        }
        self.phase(pkg, "installing");
        // 下载、逐块解压与复制的写错误直接返回；发布前再次检查挂载，防止弹盘后落在系统盘。
        guard()?;
        selected_volume()?;
        if archive::object(&dest)? != original {
            return Err(invalid("待发布版本已变化，请重新检查并确认"));
        }
        archive::install(
            &self.home,
            &stage,
            &input,
            &dest,
            pkg,
            if matches!(task, Task::Repair(..)) {
                original.as_ref()
            } else {
                None
            },
            guard,
        )?;
        guard()?;
        if part.try_exists()? {
            fs::remove_file(part)?;
        }
        self.progress(pkg, pkg.bytes);
        Ok(())
    }
}
