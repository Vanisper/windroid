//! 自包含的运行时准备器。自检与快照不触网。用户查看大小、来源与条款并同意后，
//! 调用 [Provisioner::start]。完成前的归档位于 WINDROID_HOME/staging/provision-v1。
//! 克隆的句柄共享同一工作线程，最后一个句柄释放时中断传输，保留可续传前缀。

mod archive;
mod catalog;
mod download;
mod import;
mod integrity;
mod metadata;
mod paths;
#[cfg(test)]
mod tests;

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use catalog::{Artifact, CATALOG};
use integrity::{check_file, invalid};
pub use paths::RuntimePaths;
use paths::{directory, installed};

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
    pub borrowed: bool,
    pub issue: Option<String>,
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
    pub borrowed: bool,
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
    stage: PathBuf,
    paths: RuntimePaths,
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
    /// 读取配置一次，后续设备操作与自检始终使用同一布局。
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
        let stage = home.join("staging/provision-v1");
        let snapshot = scan(&home, &stage, &paths, catalog, supported);
        let worker = Arc::new(Shared {
            home,
            stage,
            paths,
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

    pub fn paths(&self) -> &RuntimePaths {
        &self.owner.worker.paths
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
        self.launch(packages, None)
    }

    pub fn start_image(&self, id: &str) -> Result<(), String> {
        self.launch(vec![self.image_package(id)?], None)
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
        self.launch(vec![self.image_package(id)?], Some(path))
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

    fn launch(&self, packages: Vec<Artifact>, source: Option<PathBuf>) -> Result<(), String> {
        let shared = &self.owner.worker;
        let mut state = shared.state.lock().unwrap();
        if state.active {
            return if shared.cancel.load(Ordering::Acquire) {
                Err("正在暂停准备，请稍后继续".into())
            } else if packages
                .iter()
                .any(|p| Some(p.id) == state.snapshot.current_component)
            {
                Ok(())
            } else {
                Err("另一项资源正在准备，请稍后再试".into())
            };
        }
        if !shared.supported {
            return Err(UNSUPPORTED.into());
        }
        state.snapshot = shared.scan();
        let rows = state
            .snapshot
            .components
            .iter()
            .chain(&state.snapshot.images)
            .filter(|row| packages.iter().any(|p| p.id == row.id));
        let mut first_missing = None;
        for row in rows.filter(|r| !r.ready) {
            first_missing.get_or_insert(row.id);
            if row.borrowed {
                let msg = format!(
                    "开发者指定的组件为只读，请修正后重新检查。{}",
                    row.issue.as_deref().unwrap_or(row.name)
                );
                state.snapshot.current_component = Some(row.id);
                state.snapshot.phase = "failed";
                state.snapshot.error = Some(msg.clone());
                return Err(msg);
            }
        }
        if first_missing.is_none() {
            return Ok(());
        }
        state.snapshot.current_component = first_missing;
        shared.cancel.store(false, Ordering::Release);
        state.snapshot.error = None;
        state.snapshot.phase = if source.is_some() {
            "verifying"
        } else {
            "downloading"
        };
        state.active = true;
        let worker = Arc::clone(shared);
        if let Err(e) = std::thread::Builder::new()
            .name("windroid-provision".into())
            .spawn(move || worker.run(&packages, source.as_deref()))
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

fn scan(
    home: &Path,
    stage: &Path,
    paths: &RuntimePaths,
    catalog: &[Artifact],
    supported: bool,
) -> Preparation {
    let can_resume = !paths.borrowed && directory(home, stage, false).is_ok();
    let (images, components): (Vec<_>, Vec<_>) = catalog
        .iter()
        .map(|&pkg| {
            let location = paths.destination(pkg);
            let borrowed = paths.read_only(pkg.kind);
            let checked = installed(home, paths, pkg);
            let ready = checked.is_ok();
            let issue = checked.err().and_then(|e| {
                if e.kind() != ErrorKind::NotFound {
                    return Some(e.to_string());
                }
                if borrowed {
                    return Some(format!("开发者指定的位置缺少{}或其必要文件", pkg.name));
                }
                if fs::symlink_metadata(&location).is_err_and(|e| e.kind() == ErrorKind::NotFound) {
                    return None;
                }
                Some(format!("{} 安装不完整：{e}", pkg.name))
            });
            let downloaded_bytes = if ready {
                pkg.bytes
            } else if can_resume && !borrowed {
                download::part_len(&partial(stage, pkg))
                    .ok()
                    .filter(|&s| s <= pkg.bytes)
                    .unwrap_or(0)
            } else {
                0
            };
            let (license, license_url, license_text) = pkg.license();
            Component {
                id: pkg.id,
                name: pkg.name,
                version: if paths.explicit_adb(pkg.kind) {
                    "开发者指定"
                } else {
                    pkg.version
                },
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
                borrowed,
                issue,
            }
        })
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
        borrowed: paths.borrowed,
    }
}

impl Shared {
    fn scan(&self) -> Preparation {
        scan(
            &self.home,
            &self.stage,
            &self.paths,
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

    fn run(&self, packages: &[Artifact], source: Option<&Path>) {
        let result = self.prepare(packages, source);
        let mut state = self.state.lock().unwrap();
        let current = state.snapshot.current_component;
        let mut snap = self.scan();
        if let Err(e) = result {
            snap.current_component = current;
            if e.kind() == ErrorKind::Interrupted && self.cancel.load(Ordering::Acquire) {
                snap.phase = "paused";
            } else {
                snap.phase = "failed";
                snap.error = Some(e.to_string());
            }
        } else if snap
            .components
            .iter()
            .chain(&snap.images)
            .any(|c| !c.ready && packages.iter().any(|p| p.id == c.id))
        {
            snap.phase = "failed";
            snap.error = Some("安装完成后的自检未通过，请查看组件状态".into());
        }
        state.active = false;
        state.snapshot = snap;
    }

    fn prepare(&self, packages: &[Artifact], source: Option<&Path>) -> io::Result<()> {
        self.check_pause()?;
        directory(&self.home, &self.stage, true)?;
        let lock_path = self.stage.join("prepare.lock");
        download::part_len(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.try_lock().map_err(|e| {
            io::Error::other(format!("无法独占准备目录（可能有其他进程正在准备）：{e}"))
        })?;
        for &pkg in packages {
            self.check_pause()?;
            if let Err(e) = installed(&self.home, &self.paths, pkg) {
                if self.paths.read_only(pkg.kind) {
                    return Err(e);
                }
                let part = partial(&self.stage, pkg);
                let input = if let Some(source) = source {
                    self.phase(pkg, "verifying");
                    import::capture(self, source, pkg)?
                } else {
                    self.phase(pkg, "downloading");
                    download::fetch(self, pkg, &part)?;
                    self.progress(pkg, pkg.bytes);
                    fs::File::open(&part)?
                };
                self.phase(pkg, "verifying");
                let result = check_file(&input, pkg, || self.check_pause()).and_then(|()| {
                    self.phase(pkg, "installing");
                    archive::install(
                        self.paths.installation_root(pkg),
                        &input,
                        &self.paths.destination(pkg),
                        pkg,
                        || self.check_pause(),
                    )
                });
                // 导入失败应保留同版本此前下载的前缀；安装成功后即可释放它。
                if source.is_none()
                    && result.as_ref().is_err_and(|e| {
                        matches!(
                            e.kind(),
                            ErrorKind::InvalidData
                                | ErrorKind::InvalidInput
                                | ErrorKind::Unsupported
                        )
                    })
                {
                    fs::remove_file(&part)?;
                }
                result?;
                if part.try_exists()? {
                    fs::remove_file(&part)?;
                }
                installed(&self.home, &self.paths, pkg)
                    .map_err(|e| invalid(format!("{} 安装自检失败：{e}", pkg.name)))?;
            }
            self.progress(pkg, pkg.bytes);
            if let Some(c) = self
                .state
                .lock()
                .unwrap()
                .snapshot
                .rows_mut()
                .find(|c| c.id == pkg.id)
            {
                c.ready = true;
                c.issue = None;
            }
        }
        Ok(())
    }
}
