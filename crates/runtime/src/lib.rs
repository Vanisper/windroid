//! 运行环境域：让一台 Android 实例存在且可用。
//!
//! 本 crate 实现 [contract::Backend]，是设备域触碰真实世界的那一侧。
//! 它只回答「实例在不在、可不可用」，不关心谁在用它、界面上显示什么。
//!
//! 分工：本 crate 认识 emulator 与 adb 的调用方式；[adb] 只负责发命令与取输出。
//! 状态推导不在这里——那是设备域的事。

pub mod catalog;
pub mod install;
pub mod labels;
pub mod prepare;

use std::io;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Instant;

use contract::{Backend, Command as Cmd, Facts, InstanceId, Rejection, Spec};

/// 实例在主机上的落点。
#[derive(Debug, Clone)]
pub struct Layout {
    /// 根目录，如 `~/.windroid`。
    pub root: PathBuf,
}

impl Layout {
    /// 默认布局。环境变量 `WINDROID_HOME` 可覆盖，测试需要它。
    pub fn default_root() -> Self {
        let root = std::env::var_os("WINDROID_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_default();
                PathBuf::from(home).join(".windroid")
            });
        Self { root }
    }

    /// 某实例的目录。
    pub fn instance(&self, id: &InstanceId) -> PathBuf {
        self.root.join("instances").join(id.as_str())
    }
}

/// 接管到外部实例时记一条。只记一次，避免每次观测都刷。
fn log_recovered(serial: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::Relaxed) {
        eprintln!("[runtime] 接管已在运行的实例 {serial}");
    }
}

/// adb serial 的命名规则：`emulator-<console port>`。
///
/// 端口由本 crate 分配并记录，因为它是需要全局协调的资源：
/// 同一端口不能被两个实例占用，而 console 与 adb 各占一个（N、N+1）。
pub fn serial_for(console_port: u16) -> String {
    format!("emulator-{console_port}")
}

/// 真实后端：一台模拟器实例。
pub struct EmulatorBackend {
    id: InstanceId,
    layout: Layout,
    /// console 端口。与 serial 一一对应。
    port: u16,
    /// 模拟器进程。None 表示没在运行。
    process: Option<Child>,
    /// 启动时刻，用于把超时折成事实。
    started: Option<Instant>,
    /// 待写入的事实：`Create` 之后才为 true。
    created: bool,
    /// 应用清单缓存。按需刷新，不在每次观测时重查——一次清单要跑五条
    /// adb 命令，放到观测节奏里等于每秒几十次外部调用。
    apps: Vec<contract::AppEntry>,
    /// 清单是否需要在下次可读时刷新。
    apps_dirty: bool,
    /// 清单读取失败后的下次重试时刻。
    ///
    /// 失败不能清除 dirty，否则一次早期失败会让列表永远为空；也不能每个
    /// 观测周期立刻重试，否则持续的 adb 故障会变成高频外部调用。
    apps_retry_at: Option<Instant>,
    /// 应用展示名缓存。
    labels: labels::Labels,
    /// 桌面状态缓存，与清单一并刷新。
    desktop: contract::Desktop,
    /// 该实例不是本进程启动的，而是接管了一个已在运行的实例。
    ///
    /// 用户可能先手动起了模拟器，或上一次退出时没来得及清理。设备域必须
    /// 如实反映「这台设备在运行」，否则界面会显示已停止、所有按钮禁用，
    /// 而实际画面就在旁边正常播放。
    adopted: bool,
}

impl EmulatorBackend {
    pub fn new(id: InstanceId, layout: Layout, port: u16) -> Self {
        let layout_cache = layout.root.join("cache");
        Self {
            id,
            layout,
            port,
            process: None,
            started: None,
            created: false,
            apps: Vec::new(),
            apps_dirty: true,
            apps_retry_at: None,
            adopted: false,
            labels: labels::Labels::new(&layout_cache),
            desktop: contract::Desktop::default(),
        }
    }

    pub fn serial(&self) -> String {
        serial_for(self.port)
    }

    /// 读取应用清单、展示名与桌面状态。
    ///
    /// 三项必须一起更新：界面用它们共同派生每个应用可执行的操作。
    fn refresh_catalog(&mut self, serial: &str) -> Result<(), String> {
        let mut apps = catalog::snapshot(serial)?;
        // package 服务已注册不代表包索引已构建完成。启动早期 pm 可能成功
        // 返回空集合；把空集合当成有效结果会让清单永久停在那里。
        // 正常 Android 至少有系统桌面与设置，因此空集合应按暂未就绪重试。
        if apps.is_empty() {
            return Err("设备暂未返回应用清单".to_owned());
        }
        let pkgs: Vec<String> = apps.iter().map(|a| a.package.to_string()).collect();
        let labels = self.labels.resolve(serial, &pkgs);
        for app in &mut apps {
            app.label = labels.get(app.package.as_str()).cloned();
        }
        let desktop = catalog::home_state(serial).map(|h| contract::Desktop {
            candidates: h.candidates.iter().map(contract::PackageName::new).collect(),
            default: h.default.as_deref().map(contract::PackageName::new),
        })?;
        self.apps = apps;
        self.desktop = desktop;
        log::debug!("应用清单已刷新: {} 个应用", self.apps.len());
        self.apps_dirty = false;
        self.apps_retry_at = None;
        Ok(())
    }

    /// 模拟器可执行文件。
    ///
    /// 发布版从应用自带的运行时组件取；开发期可用系统 SDK，因此留了环境变量入口。
    fn emulator_bin(&self) -> String {
        std::env::var("WINDROID_EMULATOR")
            .unwrap_or_else(|_| "emulator".to_owned())
    }

    /// AVD 名。M1a 阶段一个实例对应一个手工创建的 AVD。
    fn avd_name(&self) -> String {
        std::env::var("WINDROID_AVD").unwrap_or_else(|_| "tv14".to_owned())
    }

    fn dir(&self) -> PathBuf {
        self.layout.instance(&self.id)
    }

    /// 进程是否还活着。用 try_wait 而非 kill(0)：前者能回收僵尸。
    fn alive(&mut self) -> bool {
        match self.process.as_mut() {
            None => false,
            Some(child) => match child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) | Err(_) => {
                    self.process = None;
                    false
                }
            },
        }
    }

    fn start(&mut self, facts: &mut Facts) -> io::Result<()> {
        if self.alive() {
            return Ok(());
        }
        let dir = self.dir();
        std::fs::create_dir_all(&dir)?;
        let log = std::fs::File::create(dir.join("emulator.log"))?;
        let child = Command::new(self.emulator_bin())
            .arg("-avd")
            .arg(self.avd_name())
            .arg("-port")
            .arg(self.port.to_string())
            .args(["-no-window", "-no-audio", "-no-boot-anim"])
            // 无窗口不等于无渲染。AVD 可能配置 hw.gpu.enabled=no，此时
            // Android 仍会用 SwiftShader 在 CPU 上合成 1080p 画面，qemu
            // 实测可长期占用 7 个核。产品默认使用宿主机 GPU。
            .arg("-gpu")
            .arg(std::env::var("WINDROID_GPU").unwrap_or_else(|_| "host".to_owned()))
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        self.process = Some(child);
        self.started = Some(Instant::now());
        facts.process_alive = true;
        facts.ever_launched = true;
        facts.boot_stalled = false;
        facts.aborted = false;
        Ok(())
    }

    fn stop(&mut self, facts: &mut Facts) {
        // 先请 adb 优雅退出，再回收进程。emu kill 走 adb 是因为它对
        // 「进程已在但端口还没起」这种情况同样有效。
        let serial = self.serial();
        let _ = adb::run(&serial, &["emu", "kill"]);
        if let Some(mut child) = self.process.take() {
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
                    Err(_) => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        facts.process_alive = false;
        facts.adb_visible = false;
        facts.services = Default::default();
        facts.boot_completed = false;
    }
}

impl Backend for EmulatorBackend {
    fn exec(&mut self, cmd: &Cmd, facts: &mut Facts) -> Result<(), Rejection> {
        match cmd {
            Cmd::Create { spec: Spec { .. } } => {
                std::fs::create_dir_all(self.dir())
                    .map_err(|e| Rejection::Error { code: contract::ErrorCode::Io, detail: e.to_string() })?;
                self.created = true;
                facts.dir_exists = true;
            }
            Cmd::Delete => {
                let dir = self.dir();
                let _ = std::fs::remove_dir_all(&dir);
                self.created = false;
                *facts = Facts::default();
            }
            Cmd::Start => {
                self.start(facts).map_err(|e| Rejection::Error {
                    code: contract::ErrorCode::Io,
                    detail: e.to_string(),
                })?;
            }
            Cmd::Stop => {
                // 用户的停止意图必须记录成事实：State::of 靠它区分
                // 「预期停止」（Stopped）与「进程意外死亡」（Failed）。
                facts.stop_requested = true;
                self.stop(facts);
            }
            Cmd::RefreshApps => {
                self.refresh_catalog(&self.serial()).map_err(|e| Rejection::Error {
                    code: contract::ErrorCode::AdbFailure,
                    detail: e,
                })?;
            }
            Cmd::Install { apk_path } => {
                // 安装结果必须回读。失败要带上分类与原因——「安装失败」
                // 这四个字对用户没有任何用处。
                match install::install(&self.serial(), apk_path).map_err(|e| {
                    Rejection::Error {
                        code: contract::ErrorCode::AdbFailure,
                        detail: e,
                    }
                })? {
                    install::Outcome::Installed => self.apps_dirty = true,
                    install::Outcome::Unverified => self.apps_dirty = true,
                    install::Outcome::Failed { code, detail } => {
                        return Err(Rejection::Error { code, detail });
                    }
                }
            }
            Cmd::Uninstall { package, keep_data } => {
                let ok = install::uninstall(&self.serial(), package.as_str(), *keep_data)
                    .map_err(|e| Rejection::Error {
                        code: contract::ErrorCode::AdbFailure,
                        detail: e,
                    })?;
                if !ok {
                    return Err(Rejection::Error {
                        code: contract::ErrorCode::PackageNotFound,
                        detail: format!("未能卸载 {package}"),
                    });
                }
                self.apps_dirty = true;
            }
            Cmd::SetDefaultHome { package } => {
                let ok = catalog::set_default_home(&self.serial(), package.as_str())
                    .map_err(|e| Rejection::Error {
                        code: contract::ErrorCode::AdbFailure,
                        detail: e,
                    })?;
                if !ok {
                    return Err(Rejection::Error {
                        code: contract::ErrorCode::PackageNotFound,
                        detail: format!("{package} 不能作为桌面"),
                    });
                }
            }
            Cmd::ForceStop { package } => {
                let _ = adb::shell(&self.serial(), &format!("am force-stop {package}"));
                self.apps_dirty = true;
            }
            Cmd::ClearData { package } => {
                let out = adb::shell(&self.serial(), &format!("pm clear {package}")).map_err(
                    |e| Rejection::Error {
                        code: contract::ErrorCode::AdbFailure,
                        detail: e.to_string(),
                    },
                )?;
                // pm clear 失败时退出码仍是 0，只能看输出。
                if !out.has("Success") {
                    return Err(Rejection::Error {
                        code: contract::ErrorCode::AdbFailure,
                        detail: format!("清除 {package} 的数据失败"),
                    });
                }
                self.apps_dirty = true;
            }
            Cmd::Launch { package } => {
                let ok = catalog::launch(&self.serial(), package.as_str()).map_err(|e| {
                    Rejection::Error {
                        code: contract::ErrorCode::AdbFailure,
                        detail: e,
                    }
                })?;
                if !ok {
                    return Err(Rejection::Error {
                        code: contract::ErrorCode::PackageNotFound,
                        detail: format!("找不到可启动的 {package}"),
                    });
                }
            }
        }
        Ok(())
    }

    fn apps(&self) -> Vec<contract::AppEntry> {
        self.apps.clone()
    }

    fn desktop(&self) -> contract::Desktop {
        self.desktop.clone()
    }

    fn abort(&mut self, facts: &mut Facts) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        facts.process_alive = false;
        facts.adb_visible = false;
        facts.services = Default::default();
        facts.boot_completed = false;
    }

    fn observe(&mut self, facts: &mut Facts) {
        // 只写观测到的事实。这里不判断状态——那是 State::of 的事。
        facts.dir_exists = self.dir().is_dir();
        facts.process_alive = self.alive();

        // 不是我们启动的，但设备在应答：接管它。
        //
        // 这不是宽容，是如实反映：adb 能看到这台设备并且它已经引导完成，
        // 「实例正在运行」就是事实。隐瞒它会让界面显示已停止，而画面就在
        // 旁边正常播放——用户只会认为软件坏了。
        // 用户已请求停止时不再接管。接管是为了反映「设备在运行」这个事实，
        // 而停止途中设备会短暂地仍能应答——此时接管会让状态在 Stopping 与
        // Ready 之间来回跳，用户看到的是「停了但一直停不下来」。
        if !facts.process_alive && !facts.stop_requested {
            let serial = self.serial();
            if adb::is_ready(&serial).unwrap_or(false) {
                facts.process_alive = true;
                facts.ever_launched = true;
                self.adopted = true;
                log_recovered(&serial);
            }
        }
        if !facts.process_alive {
            facts.adb_visible = false;
            facts.services = Default::default();
            facts.boot_completed = false;
            return;
        }
        let serial = self.serial();
        facts.adb_visible = adb::is_ready(&serial).unwrap_or(false);
        if !facts.adb_visible {
            facts.services = Default::default();
            facts.boot_completed = false;
            return;
        }
        // 服务探测只在还没齐的时候做。
        //
        // 三个服务一旦就绪就不会变回未就绪（设备要挂是整台挂，那时
        // adb_visible 先变假）。每轮都查是三条 adb 命令，而观测在就绪态
        // 每 500ms 一次——实测这是空闲时的主要 CPU 消耗。
        if !facts.services.ready() {
            facts.services.package = adb::shell(&serial, "service check package")
                .map(|o| o.has("found"))
                .unwrap_or(false);
            facts.services.activity = adb::shell(&serial, "service check activity")
                .map(|o| o.has("found"))
                .unwrap_or(false);
            facts.services.window = adb::shell(&serial, "service check window")
                .map(|o| o.has("found"))
                .unwrap_or(false);
        }
        facts.boot_completed = adb::shell(&serial, "getprop sys.boot_completed")
            .map(|o| o.stdout.trim() == "1")
            .unwrap_or(!facts.services.ready());

        // 设备可用后按需刷新清单。刷新失败不影响状态判定——清单是目录，
        // 不是状态事实。
        let retry_due = self.apps_retry_at.is_none_or(|at| Instant::now() >= at);
        if self.apps_dirty
            && facts.services.ready()
            && retry_due
            && let Err(error) = self.refresh_catalog(&serial)
        {
            // 清单失败不改变设备状态，但必须保留 dirty 并退避重试。
            // 清除 dirty 会让一次早期失败变成永久空列表。
            log::warn!("读取应用清单失败，5 秒后重试: {error}");
            self.apps_retry_at = Some(Instant::now() + std::time::Duration::from_secs(5));
        }
    }
}
