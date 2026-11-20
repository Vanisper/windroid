//! 运行环境域：让一台 Android 实例存在且可用。
//!
//! 本 crate 实现 [contract::Backend]，是设备域触碰真实世界的那一侧。
//! 它只回答「实例在不在、可不可用」，不关心谁在用它、界面上显示什么。
//!
//! 分工：本 crate 认识 emulator 与 adb 的调用方式；[adb] 只负责发命令与取输出。
//! 状态推导不在这里——那是设备域的事。

pub mod admission;
pub mod catalog;
pub mod install;
pub mod instances;
pub mod labels;
pub mod provision;
pub mod running;
pub mod storage;
pub mod workspace;

/// 文件锁的生命周期用例需要独占进程。并行测试启动的其他子进程在 exec 前也会继承锁。
#[cfg(test)]
fn rerun_in_isolated_process(test: &str) -> bool {
    const SELECTED: &str = "WINDROID_ISOLATED_RUNTIME_TEST";
    if std::env::var(SELECTED).as_deref() == Ok(test) {
        return false;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(SELECTED, test)
        .status()
        .unwrap();
    assert!(status.success(), "{test}: {status}");
    true
}

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use contract::{
    Backend, CatalogStatus, Command as Cmd, ErrorCode, Facts, InstanceId, Rejection, Services,
};
use instances::{
    InstanceRecord, InstanceStore,
    process::{OwnedEmulator, StopMode},
};
use provision::RuntimePaths;

#[derive(Debug, Clone)]
pub struct Layout {
    /// 产品根目录，如 ~/.windroid。显式设置 WINDROID_HOME 可隔离多个库。
    pub root: PathBuf,
}

impl Layout {
    pub fn default_root() -> Self {
        let root = std::env::var_os("WINDROID_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::home_dir().unwrap_or_default().join(".windroid"));
        Self { root }
    }

    /// 展示或定位已登记实例的目录。实际文件操作统一经 InstanceStore 校验身份。
    pub fn instance(&self, id: &InstanceId) -> PathBuf {
        self.root.join("instances").join(id.as_str())
    }
}

/// Android 的传输地址在模拟器退出后可以复用。
pub fn serial_for(console_port: u16) -> String {
    format!("emulator-{console_port}")
}

/// 查询在一次 adb shell 中取回设备归属、启动状态和服务索引，避免多处独立轮询。
const HEALTH: &str = "getprop ro.boot.qemu.avd_name; getprop sys.boot_completed; service list";

fn core_services<'a>(lines: impl Iterator<Item = &'a str>) -> Services {
    let mut services = Services::default();
    // service list 行为「序号 名字: [接口]」。activity_task/package_native/windowinfo 都不是三个核心服务。
    for line in lines {
        let mut words = line.split_whitespace();
        if words.next().is_none_or(|n| n.parse::<u32>().is_err()) {
            continue;
        }
        match words.next() {
            Some("activity:") => services.activity = true,
            Some("package:") => services.package = true,
            Some("window:") => services.window = true,
            _ => (),
        }
    }
    services
}

fn clear_device(facts: &mut Facts) {
    facts.adb_visible = false;
    facts.services = Services::default();
    facts.boot_completed = false;
}

/// 模拟器、短命令与窗口共享启动时解析出的绝对路径及版本读锁。
#[derive(Debug)]
pub struct SessionResources {
    pub paths: RuntimePaths,
    _leases: Vec<File>,
}

impl SessionResources {
    pub fn device<'a>(&'a self, serial: &'a str) -> adb::Device<'a> {
        adb::Device::new(&self.paths.adb, serial)
    }
}

type ResourceResolver = fn(&Path, &contract::Image) -> io::Result<(RuntimePaths, Vec<File>)>;

#[derive(Clone)]
pub struct ActiveRuntime {
    current: Arc<Mutex<Weak<SessionResources>>>,
    resolve: ResourceResolver,
    preview: ResourceResolver,
    admission: admission::Admission,
}

impl Default for ActiveRuntime {
    fn default() -> Self {
        Self {
            preview: storage::preview_instance_versions,
            ..Self::new(storage::instance_lease)
        }
    }
}

impl ActiveRuntime {
    /// 集成测试可注入隔离工具。产品入口使用绑定工作区设置和官方清单的默认解析器。
    pub fn new(resolve: ResourceResolver) -> Self {
        Self {
            current: Default::default(),
            resolve,
            preview: resolve,
            admission: Default::default(),
        }
    }

    pub fn with_admission(mut self, admission: admission::Admission) -> Self {
        self.admission = admission;
        self
    }

    pub fn acquire(
        &self,
        home: &std::path::Path,
        image: &contract::Image,
    ) -> io::Result<Arc<SessionResources>> {
        let resources = self.resolve_versions(home, image)?;
        *self.current.lock().unwrap() = Arc::downgrade(&resources);
        Ok(resources)
    }

    fn resolve_versions(
        &self,
        home: &Path,
        image: &contract::Image,
    ) -> io::Result<Arc<SessionResources>> {
        let (paths, leases) = (self.resolve)(home, image)?;
        Ok(Arc::new(SessionResources {
            paths,
            _leases: leases,
        }))
    }

    pub fn current(&self) -> Option<Arc<SessionResources>> {
        self.current.lock().unwrap().upgrade()
    }
}

pub struct EmulatorBackend {
    id: InstanceId,
    port: u16,
    store: InstanceStore,
    active: ActiveRuntime,
    record: Option<InstanceRecord>,
    process: Option<OwnedEmulator>,
    next_probe: Option<Instant>,
    apps: Vec<contract::AppEntry>,
    desktop: contract::Desktop,
    labels: labels::Labels,
    catalog_status: CatalogStatus,
    apps_dirty: bool,
    apps_retry_at: Option<Instant>,
}

impl EmulatorBackend {
    pub fn new(id: InstanceId, layout: Layout, port: u16) -> Self {
        Self::with_runtime(id, layout, port, ActiveRuntime::default())
    }

    pub fn with_runtime(id: InstanceId, layout: Layout, port: u16, active: ActiveRuntime) -> Self {
        let labels = labels::Labels::new(&layout.root, &id);
        let store = InstanceStore::new(layout);
        let record = store.get(&id).ok().filter(|r| r.port == port);
        Self {
            id,
            port,
            store,
            active,
            record,
            process: None,
            next_probe: None,
            apps: Vec::new(),
            desktop: Default::default(),
            labels,
            catalog_status: CatalogStatus::Pending,
            apps_dirty: true,
            apps_retry_at: None,
        }
    }

    pub fn active_runtime(&self) -> ActiveRuntime {
        self.active.clone()
    }

    pub fn serial(&self) -> String {
        serial_for(self.port)
    }

    fn reset_catalog(&mut self) {
        self.catalog_status = CatalogStatus::Pending;
        self.apps_dirty = true;
        self.apps_retry_at = None;
    }

    fn catalog_failed(&mut self, message: &str) {
        self.apps_dirty = true;
        self.apps_retry_at = Some(Instant::now() + Duration::from_secs(2));
        self.catalog_status = CatalogStatus::Failed {
            message: message.to_owned(),
        };
    }

    fn refresh_catalog(&mut self) -> Result<(), String> {
        let serial = self.serial();
        let resources = self.active.current().ok_or("模拟器已退出")?;
        let device = adb::Device::new(&resources.paths.adb, &serial);
        // 完成同批读取后才发布。部分查询失败时保留上一次成功的目录和桌面状态。
        let result = (|| -> Result<_, String> {
            let mut apps = catalog::snapshot(device)?;
            let home = catalog::home_state(device)?;
            // 标签查询有整批时限，先处理桌面候选，再处理应用库中的其余包。
            let packages: Vec<_> = home
                .candidates
                .iter()
                .cloned()
                .chain(
                    apps.iter()
                        .filter(|app| !home.is_candidate(app.package.as_str()))
                        .map(|app| app.package.to_string()),
                )
                .collect();
            let mut labels = self
                .labels
                .resolve(device, &resources.paths.aapt, &packages);
            for app in &mut apps {
                app.label = labels.get(app.package.as_str()).cloned();
            }
            labels.retain(|package, _| home.is_candidate(package));
            Ok((
                apps,
                contract::Desktop {
                    candidates: home
                        .candidates
                        .into_iter()
                        .map(contract::PackageName::new)
                        .collect(),
                    candidate_labels: labels,
                    default: home.default.map(contract::PackageName::new),
                },
            ))
        })();
        match result {
            Ok((apps, desktop)) => {
                self.apps = apps;
                self.desktop = desktop;
                self.apps_dirty = false;
                self.apps_retry_at = None;
                self.catalog_status = CatalogStatus::Ready;
                Ok(())
            }
            Err(message) => {
                self.catalog_failed(&message);
                Err(message)
            }
        }
    }

    fn live(&mut self) -> io::Result<bool> {
        if let Some(process) = self.process.as_mut() {
            if process.alive()? {
                return Ok(true);
            }
            self.process.take();
            self.reset_catalog();
        }
        Ok(false)
    }

    fn start(&mut self, cold: bool, facts: &mut Facts) -> io::Result<()> {
        if self.live()? {
            return Ok(());
        }
        let process = OwnedEmulator::start(&self.store, &self.id, self.port, &self.active, cold)?;
        self.record = Some(process.record.clone());
        self.process = Some(process);
        self.reset_catalog();
        self.next_probe = None;
        *facts = Facts {
            dir_exists: true,
            process_alive: true,
            ever_launched: true,
            ..Default::default()
        };
        Ok(())
    }

    fn stop(&mut self, mode: StopMode, facts: &mut Facts) -> io::Result<()> {
        if let Some(process) = self.process.as_mut() {
            process.stop(mode)?;
        }
        self.process.take();
        self.reset_catalog();
        self.next_probe = None;
        facts.process_alive = false;
        facts.allocation = None;
        facts.session_count = 0;
        clear_device(facts);
        Ok(())
    }

    fn verify_target(&mut self) -> Result<String, Rejection> {
        if !self.live().map_err(io_rejection)? {
            return Err(adb_rejection("该实例的模拟器进程已退出"));
        }
        let process = self.process.as_ref().unwrap();
        process
            .record
            .verify_device(&process.resources.paths.adb)
            .map_err(adb_rejection)?;
        Ok(process.record.serial())
    }

    fn device_command(&mut self, cmd: &Cmd) -> Result<(), Rejection> {
        let serial = self.verify_target().inspect_err(|reason| {
            if matches!(cmd, Cmd::RefreshApps)
                && let Rejection::Error { detail, .. } = reason
            {
                self.catalog_failed(detail);
            }
        })?;
        let resources = self.process.as_ref().unwrap().resources.clone();
        let device = adb::Device::new(&resources.paths.adb, &serial);
        match cmd {
            Cmd::RefreshApps => self.refresh_catalog().map_err(adb_rejection)?,
            Cmd::Install { apk_path } => {
                match install::install(device, apk_path, &resources.paths.aapt)
                    .map_err(adb_rejection)?
                {
                    install::Outcome::Installed => self.apps_dirty = true,
                    install::Outcome::Unverified { detail } => {
                        self.apps_dirty = true;
                        return Err(adb_rejection(detail));
                    }
                    install::Outcome::Failed { code, detail } => {
                        return Err(Rejection::Error { code, detail });
                    }
                }
            }
            Cmd::Uninstall { package, keep_data } => {
                if !install::uninstall(device, package.as_str(), *keep_data)
                    .map_err(adb_rejection)?
                {
                    return Err(adb_rejection(format!("未能确认卸载 {package}")));
                }
                self.apps_dirty = true;
            }
            Cmd::SetDefaultHome { package } => {
                if !catalog::set_default_home(device, package.as_str()).map_err(adb_rejection)? {
                    return Err(adb_rejection(format!("未能将 {package} 设置为默认桌面")));
                }
                self.desktop.default = Some(package.clone());
                self.apps_dirty = true;
            }
            Cmd::ForceStop { package } => {
                running::force_stop(device, package.as_str()).map_err(adb_rejection)?
            }
            Cmd::ClearData { package } => {
                catalog::require_package(package.as_str()).map_err(adb_rejection)?;
                let output =
                    catalog::checked_shell(device, &format!("pm clear --user 0 {package}"))
                        .map_err(adb_rejection)?;
                if output.stdout.trim() != "Success" {
                    return Err(adb_rejection(format!("未能确认已清除 {package} 的数据")));
                }
                self.apps_dirty = true;
            }
            Cmd::Launch { package } => {
                if !catalog::launch(device, package.as_str()).map_err(adb_rejection)? {
                    return Err(Rejection::Error {
                        code: ErrorCode::PackageNotFound,
                        detail: format!("找不到可启动的 {package}"),
                    });
                }
            }
            Cmd::Create { .. } | Cmd::Delete | Cmd::Start | Cmd::Restart { .. } | Cmd::Stop => {
                unreachable!()
            }
        }
        Ok(())
    }
}

fn io_rejection(error: io::Error) -> Rejection {
    let code = match error.kind() {
        io::ErrorKind::AddrInUse => ErrorCode::NoFreePort,
        io::ErrorKind::StorageFull => ErrorCode::OutOfSpace,
        io::ErrorKind::NotFound if error.to_string().contains("镜像") => ErrorCode::ImageMissing,
        io::ErrorKind::NotFound => ErrorCode::ComponentMissing,
        _ => ErrorCode::Io,
    };
    Rejection::Error {
        code,
        detail: error.to_string(),
    }
}
fn adb_rejection(error: impl std::fmt::Display) -> Rejection {
    Rejection::Error {
        code: ErrorCode::AdbFailure,
        detail: error.to_string(),
    }
}

impl Backend for EmulatorBackend {
    fn exec(&mut self, cmd: &Cmd, facts: &mut Facts) -> Result<(), Rejection> {
        match cmd {
            Cmd::Create { spec } => {
                self.record = Some(
                    self.store
                        .create_at(&self.id, self.port, spec.clone())
                        .map_err(io_rejection)?,
                );
                facts.dir_exists = true;
            }
            Cmd::Delete => {
                self.store.delete(&self.id).map_err(io_rejection)?;
                self.record = None;
                self.apps.clear();
                self.desktop = Default::default();
                self.reset_catalog();
                *facts = Facts::default();
            }
            Cmd::Start => self.start(false, facts).map_err(io_rejection)?,
            Cmd::Restart { cold } => {
                if self.live().map_err(io_rejection)? {
                    self.process
                        .as_ref()
                        .unwrap()
                        .check_replacement(&self.store, &self.active)
                        .map_err(io_rejection)?;
                    self.stop(StopMode::Restart { cold: *cold }, facts)
                        .map_err(io_rejection)?;
                    facts.stop_requested = true;
                }
                self.start(*cold, facts).map_err(io_rejection)?;
            }
            Cmd::Stop => {
                self.stop(StopMode::Graceful, facts).map_err(io_rejection)?;
                facts.stop_requested = true;
            }
            _ => self.device_command(cmd)?,
        }
        Ok(())
    }

    fn abort(&mut self, facts: &mut Facts) {
        if let Err(e) = self.stop(StopMode::Force, facts) {
            log::error!("无法终止模拟器 {}：{e}", self.id);
        }
    }

    fn observe(&mut self, facts: &mut Facts) {
        facts.dir_exists = self.record.is_some();
        facts.process_alive = self.live().unwrap_or_else(|e| {
            log::warn!("无法读取模拟器进程状态：{e}");
            false
        });
        if !facts.process_alive {
            facts.allocation = None;
            clear_device(facts);
            return;
        }
        if self.next_probe.is_some_and(|t| t > Instant::now()) {
            return;
        }
        let previously_ready = facts.services.ready();
        clear_device(facts);
        let serial = self.serial();
        if let Ok(output) = adb::shell(
            adb::Device::new(&self.process.as_ref().unwrap().resources.paths.adb, &serial),
            HEALTH,
        ) {
            let mut lines = output.stdout.lines().map(str::trim);
            if output.code == Some(0) && lines.next() == self.record.as_ref().map(|r| r.avd_name())
            {
                facts.adb_visible = true;
                facts.boot_completed = lines.next() == Some("1");
                facts.services = core_services(lines);
            }
        }
        let ready = facts.services.ready();
        if ready
            && facts
                .allocation
                .is_none_or(|a| a.cpu_cores.is_none() || a.ram_mib.is_none())
        {
            facts.allocation = self
                .process
                .as_mut()
                .and_then(|p| p.allocation(&self.active.admission));
        }
        self.next_probe = Some(
            Instant::now()
                + if ready {
                    Duration::from_secs(1)
                } else {
                    Duration::from_millis(350)
                },
        );
        // 先发布第一次就绪，任务观察器即可独立读取活动栈。目录与标签读取可能需要数秒。
        if ready
            && previously_ready
            && self.apps_dirty
            && self.apps_retry_at.is_none_or(|t| Instant::now() >= t)
        {
            let _ = self.refresh_catalog();
        }
    }

    fn apps(&self) -> Vec<contract::AppEntry> {
        self.apps.clone()
    }
    fn catalog_status(&self) -> CatalogStatus {
        self.catalog_status.clone()
    }
    fn desktop(&self) -> contract::Desktop {
        self.desktop.clone()
    }
}
