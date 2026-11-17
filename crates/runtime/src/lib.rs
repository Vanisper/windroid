//! 运行环境域：让一台 Android 实例存在且可用。
//!
//! 本 crate 实现 [contract::Backend]，是设备域触碰真实世界的那一侧。
//! 它只回答「实例在不在、可不可用」，不关心谁在用它、界面上显示什么。
//!
//! 分工：本 crate 认识 emulator 与 adb 的调用方式；[adb] 只负责发命令与取输出。
//! 状态推导不在这里——那是设备域的事。

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
}

impl EmulatorBackend {
    pub fn new(id: InstanceId, layout: Layout, port: u16) -> Self {
        Self { id, layout, port, process: None, started: None, created: false }
    }

    pub fn serial(&self) -> String {
        serial_for(self.port)
    }

    /// 模拟器可执行文件。真实产品会从自带的二级组件里取；M1a 先用系统 SDK。
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
            // 其余命令在 M1b 之后接入。到这里说明调用方越过了准入判断。
            other => {
                return Err(Rejection::Error {
                    code: contract::ErrorCode::Unsupported,
                    detail: format!("M1a 未实现: {other:?}"),
                });
            }
        }
        Ok(())
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
        // 三个核心服务分别探测，因为「哪一个是最后一个就绪的」决定了诊断信息。
        facts.services.package = adb::shell(&serial, "service check package")
            .map(|o| o.has("found"))
            .unwrap_or(false);
        facts.services.activity = adb::shell(&serial, "service check activity")
            .map(|o| o.has("found"))
            .unwrap_or(false);
        facts.services.window = adb::shell(&serial, "service check window")
            .map(|o| o.has("found"))
            .unwrap_or(false);
        facts.boot_completed = adb::shell(&serial, "getprop sys.boot_completed")
            .map(|o| o.stdout.trim() == "1")
            .unwrap_or(false);
    }
}
