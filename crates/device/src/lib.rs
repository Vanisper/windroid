//! Windroid 设备域：一台 Android 实例的唯一写者。
//!
//! 三条约束在本 crate 里落地：
//!
//! 1. **单一写者**。所有状态变更都发生在 actor 线程内，外部只能投递命令。
//! 2. **状态由事实派生**。状态不是字段，是 [contract::State::of] 对事实的即时计算。
//!    本 crate 里没有任何一行代码给状态赋值——搜不到 "state =" 这样的写入。
//! 3. **不依赖 GUI**。本 crate 不引用任何窗口或渲染类型，因此可以在没有窗口的
//!    情况下完整测试状态机。
//!
//! 副作用通过 [contract::Backend] 抽象隔离。actor 与契约不依赖任何具体实现，
//! 因此同一套状态机可以跑在假后端（无模拟器）与真实后端上。

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub use contract::{Backend, Timeouts};

use contract::{
    Ack, AppEntry, Command, Event, Facts, InstanceId, LogStream, PackageName, Phase, Rejection, Seq,
    Snapshot, State,
};

// ---------------------------------------------------------------------------
// Backend：副作用边界
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 句柄：外部唯一的入口
// ---------------------------------------------------------------------------

/// 命令、事件、快照三件套的外部句柄。可跨线程克隆。
///
/// 接口只传可序列化的值，不传闭包或共享可变引用——这样将来把设备域拆成
/// 独立守护进程时只需换传输层。
#[derive(Clone)]
pub struct Handle {
    id: InstanceId,
    tx: Sender<Msg>,
    shared: Arc<Shared>,
}

struct Shared {
    snapshot: Mutex<Snapshot>,
}

enum Msg {
    Command(Command, Sender<Ack>),
}

impl Handle {
    /// 实例标识。
    pub fn id(&self) -> &InstanceId {
        &self.id
    }

    /// 投递命令，等待受理回执。
    ///
    /// 回执只说明是否被受理，不说明动作完成。完成通过事件告知。
    pub fn send(&self, cmd: Command) -> Ack {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Msg::Command(cmd, tx)).is_err() {
            return Ack::Rejected {
                reason: Rejection::Error {
                    code: contract::ErrorCode::Other,
                    detail: "设备域 actor 已退出".into(),
                },
            };
        }
        rx.recv().unwrap_or(Ack::Rejected {
            reason: Rejection::Error {
                code: contract::ErrorCode::Other,
                detail: "设备域 actor 未回执即退出".into(),
            },
        })
    }

    /// 取当前完整事实集合。
    ///
    /// 先取快照再订阅事件时，用快照的 seq 丢弃已被覆盖的旧事件。
    pub fn snapshot(&self) -> Snapshot {
        self.shared.snapshot.lock().expect("snapshot poisoned").clone()
    }
}

// ---------------------------------------------------------------------------
// Actor
// ---------------------------------------------------------------------------

/// 事件订阅端。事件必须按序、可重放，因此是单一出站的接收端。
pub struct Events {
    rx: Receiver<Event>,
}

impl Events {
    /// 阻塞取下一条事件。
    pub fn recv(&self) -> Option<Event> {
        self.rx.recv().ok()
    }

    /// 带超时取下一条事件。
    pub fn recv_timeout(&self, d: Duration) -> Option<Event> {
        self.rx.recv_timeout(d).ok()
    }

    /// 非阻塞取，取不到即返回 None。
    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// 在超时窗口内收集所有事件。
    pub fn drain(&self, idle: Duration) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(e) = self.recv_timeout(idle) {
            out.push(e);
        }
        out
    }
}

/// 启动 actor 线程，返回句柄与事件订阅端。
///
/// 事件通道是单一出站：actor 是唯一发送方，因此事件天然有序。
pub fn spawn<B: Backend>(
    id: InstanceId,
    serial: String,
    mut backend: B,
    timeouts: Timeouts,
) -> (Handle, Events) {
    let (tx, rx) = mpsc::channel::<Msg>();
    let (ev_tx, ev_rx) = mpsc::channel::<Event>();

    let initial = Snapshot {
        id: id.clone(),
        serial: serial.clone(),
        state: State::Absent,
        phase: None,
        facts: Facts::default(),
        apps: Vec::new(),
        desktop: contract::Desktop::default(),
        seq: 0,
    };
    let shared = Arc::new(Shared {
        snapshot: Mutex::new(initial),
    });

    let handle = Handle { id: id.clone(), tx, shared: Arc::clone(&shared) };

    thread::Builder::new()
        .name(format!("device-{id}"))
        .spawn(move || {
            let mut actor = Actor {
                id,
                serial,
                facts: Facts::default(),
                apps: Vec::<AppEntry>::new(),
                seq: 0,
                last_state: State::Absent,
                last_phase: None,
                started_at: None,
                timeouts,
                shared,
                events: ev_tx,
                aborted_once: false,
                pending_abort: false,
                last_apps: Vec::new(),
                desktop: contract::Desktop::default(),
            };
            actor.run(rx, &mut backend);
        })
        .expect("spawn device actor");

    (handle, Events { rx: ev_rx })
}

struct Actor {
    id: InstanceId,
    serial: String,
    facts: Facts,
    apps: Vec<AppEntry>,
    seq: Seq,
    last_state: State,
    last_phase: Option<Phase>,
    /// 进程启动时刻，用于两层超时。None 表示没在启动中。
    started_at: Option<Instant>,
    timeouts: Timeouts,
    shared: Arc<Shared>,
    events: Sender<Event>,
    /// 上一次发布的清单，用于判断是否需要通知界面。
    last_apps: Vec<AppEntry>,
    /// 桌面状态。与清单同源。
    desktop: contract::Desktop,
    /// 已因硬上限强制终止过，避免重复触发。
    aborted_once: bool,
    /// 本轮需要调用后端终止。放在字段里而不是当场调用：`apply_timeouts`
    /// 只读 `&mut self`，需要 `&mut B` 才能碰副作用。
    pending_abort: bool,
}

impl Actor {
    fn run<B: Backend>(&mut self, rx: Receiver<Msg>, backend: &mut B) {
        let mut next_observe = Instant::now();
        loop {
            // 1) 观测事实。这是状态的唯一输入。
            // 应用清单也在这里同步：它是按需刷新的目录，不参与状态推导，
            // 但快照要带上它，否则界面拿不到列表。
            //
            // 观测是有代价的：真实后端每次 observe 要发若干条 adb 命令，
            // 单条实测约 20ms。固定 50ms 轮询等于每秒 20 次外部调用、常驻
            // 占用一个核，而实例停在 Stopped 时那个数字毫无意义。
            // 所以按状态定观测节奏：启动中盯得紧，其余时段松。
            let now = Instant::now();
            if now >= next_observe {
                backend.observe(&mut self.facts);
                next_observe = now + self.observe_interval();
            }
            // 清单随观测同步。它不参与状态推导，但快照要带上，否则界面拿不到。
            self.apps = backend.apps();
            self.desktop = backend.desktop();
            self.apply_timeouts();
            // 终止必须发生在观测之后：先看到最新事实，再决定收手。
            if std::mem::take(&mut self.pending_abort) {
                backend.abort(&mut self.facts);
                backend.observe(&mut self.facts);
            }
            self.publish_state();

            // 2) 等命令，同时保持观测节奏。
            //
            // 等待时长必须与下一次观测的时刻对齐：固定 50ms 会让「本该 5ms
            // 后观测」的状态等到 50ms，测试随之变慢，而生产里这只是白等。
            let wait = next_observe
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            match rx.recv_timeout(wait) {
                Ok(Msg::Command(cmd, reply)) => {
                    let ack = self.handle_command(&cmd, backend);
                    let _ = reply.send(ack);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// 当前状态下的观测间隔。
    ///
    /// 启动中与停止中是过渡态，变化快，需要盯紧；就绪后与未创建时是稳定态，
    /// 只有外部变化（用户手动 kill、模拟器自己崩溃）才需要发现，可以松。
    fn observe_interval(&self) -> Duration {
        let base = self.timeouts.poll;
        match State::of(&self.facts) {
            State::Starting | State::Stalled | State::Stopping => base,
            State::Ready | State::Streaming => base * 10,
            State::Stopped | State::Failed => base * 20,
            // 没有实例就没有可观测的对象，放得更松。
            State::Absent => base * 40,
        }
    }

    /// 把时间吸收成事实。
    ///
    /// 超时本是时间现象，不是可观测事实。在这里把它折叠成 boot_stalled，
    /// 状态推导函数才能保持纯函数。
    fn apply_timeouts(&mut self) {
        let Some(start) = self.started_at else {
            self.facts.boot_stalled = false;
            return;
        };
        if !self.facts.process_alive || self.facts.services.ready() {
            self.started_at = None;
            self.facts.boot_stalled = false;
            return;
        }
        let waited = start.elapsed();

        // 硬上限是最后一道闸：到点必须真正终止，否则端口与磁盘被无限期占用。
        // 只置 boot_stalled 而不杀进程，等于把「防止无限占用」写成了一句注释。
        if waited >= self.timeouts.hard && !self.aborted_once {
            self.aborted_once = true;
            self.facts.aborted = true;
            self.pending_abort = true;
            log::warn!("device {} 启动超过硬上限，强制终止", self.id);
        }

        // 两层超时收敛到同一个事实，但诊断阶段不同：
        // 未到 adb 超时前卡在 waiting-adb，之后卡在服务层。
        let stalled = if self.facts.adb_visible {
            waited >= self.timeouts.services
        } else {
            waited >= self.timeouts.adb
        };
        self.facts.boot_stalled = stalled && !self.aborted_once;
    }

    fn handle_command<B: Backend>(&mut self, cmd: &Command, backend: &mut B) -> Ack {
        let state = State::of(&self.facts);

        // 命令准入：在错误的状态下拒绝，并说明当前是什么状态。
        // 这不是「错误」，是预期内的状况，用户能理解并重试。
        if let Some(wanted) = guard(cmd, state) {
            return Ack::Rejected {
                reason: Rejection::WrongState { current: state, wanted: wanted.to_string() },
            };
        }

        // 启动是一次新的意图，先清掉上一次的停止请求。
        //
        // 放在这里而不是各后端里：stop_requested 记录的是「用户想让它停」，
        // 它的生命周期归设备域，不归某个后端的实现细节。留着不清，状态推导
        // 会在进程存活判断之前命中 Stopping，表现为「点了启动但一直显示停止中」。
        if matches!(cmd, Command::Start) {
            self.facts.stop_requested = false;
        }

        if let Err(reason) = backend.exec(cmd, &mut self.facts) {
            return Ack::Rejected { reason };
        }

        // 启动/停止的时刻由 actor 记录，后端不必关心超时。
        match cmd {
            Command::Start => {
                self.started_at = Some(Instant::now());
                self.facts.boot_stalled = false;
            }
            Command::Stop => {
                self.facts.stop_requested = true;
                self.started_at = None;
            }
            _ => {}
        }

        self.seq += 1;
        let seq = self.seq;
        backend.observe(&mut self.facts);
        self.publish_state();
        Ack::Accepted { seq }
    }

    /// 重算状态，若有变化则发事件。
    ///
    /// 这是整个设备域唯一的「状态产生」路径：不是赋值，是比对。
    fn publish_state(&mut self) {
        let state = State::of(&self.facts);
        let phase = Phase::of(&self.facts);

        // 应用清单变化也要通知界面。它不是状态事实，但界面同样需要知道：
        // 留着不发，界面只会在首次快照里看到一份空清单，之后永远不再取。
        if self.apps != self.last_apps {
            log::debug!("应用清单发布: {} 个应用", self.apps.len());
            self.seq += 1;
            self.last_apps = self.apps.clone();
            self.emit(Event::AppsChanged {
                seq: self.seq,
                apps: self.apps.clone(),
            });
        }
        if state != self.last_state || phase != self.last_phase {
            self.seq += 1;
            let from = Some(self.last_state);
            self.last_state = state;
            self.last_phase = phase;
            self.emit(Event::StateChanged {
                seq: self.seq,
                from,
                to: state,
                phase,
                facts: self.facts.clone(),
            });
        }
        self.publish_snapshot(state, phase);
    }

    fn publish_snapshot(&self, state: State, phase: Option<Phase>) {
        let mut g = self.shared.snapshot.lock().expect("snapshot poisoned");
        g.state = state;
        g.phase = phase;
        g.facts = self.facts.clone();
        g.apps = self.apps.clone();
        g.desktop = self.desktop.clone();
        g.seq = self.seq;
        g.id = self.id.clone();
        g.serial = self.serial.clone();
    }

    fn emit(&self, e: Event) {
        if let Event::Log { stream, line, .. } = &e {
            let which = match stream {
                LogStream::Stdout => "out",
                LogStream::Stderr => "err",
                LogStream::Internal => "int",
            };
            log::debug!("device {}/{}: {}", self.id, which, line);
        }
        let _ = self.events.send(e);
    }
}

/// 命令准入规则。返回 Some(wanted) 表示拒绝，并说明需要什么状态。
///
/// 判断取自 [contract::Capabilities]：界面按钮的禁用与这里的准入是同一份
/// 结果。两侧各写一遍会漂移，而漂移的表现是「按钮能点，点了报错」。
///
/// Stop 是幂等的：已停止时再点一次不是错误，也不弹提示。拒绝的是「没有实例
/// 时停止」，那说明界面与设备域脱节了。
fn guard(cmd: &Command, state: State) -> Option<&'static str> {
    use State::*;
    let caps = contract::Capabilities::of(state);
    match cmd {
        Command::Create { .. } => (!caps.create).then_some("不存在的实例"),
        Command::Delete => (!caps.delete).then_some("已停止"),
        Command::Start => (!caps.start).then_some("已停止"),
        Command::Stop => matches!(state, Absent).then_some("已创建的实例"),
        Command::Install { .. } => (!caps.install).then_some("系统就绪"),
        Command::Uninstall { .. } => (!caps.install).then_some("系统就绪"),
        Command::Launch { .. } => (!caps.launch_app).then_some("系统就绪"),
        Command::RefreshApps => (!caps.refresh_apps).then_some("系统就绪"),
        // 这三个都是对已装应用的操作，与启动同样的前提：设备完全可用。
        Command::SetDefaultHome { .. }
        | Command::ForceStop { .. }
        | Command::ClearData { .. } => {
            (!caps.launch_app).then_some("系统就绪")
        }
    }
}

// ---------------------------------------------------------------------------
// 假后端：按真实时序推进，验证状态机而不碰模拟器
// ---------------------------------------------------------------------------

/// 模拟一次启动过程的后端。
///
/// 它按真实时序推进：进程先活，然后 adb 可见，然后服务逐个出现（package 先、
/// window 最后），最后 boot_completed。这个顺序来自实测，不是随便排的。
pub struct FakeBackend {
    /// 当前推进到第几步。
    step: u8,
    /// 走完一步需要的 observe 次数，用于把过程拉慢到可观察。
    ticks_per_step: u8,
    counter: u8,
    /// adb 可见后不再推进，用于测 Stalled。
    stall_after_adb: bool,
    /// 进程在启动后立刻死亡，用于测 Failed。
    die_on_start: bool,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self {
            step: 0,
            ticks_per_step: 1,
            counter: 0,
            stall_after_adb: false,
            die_on_start: false,
        }
    }

    /// 把每一步拉长到 ticks 次观测，便于在测试里观察中间态。
    pub fn slowed(mut self, ticks: u8) -> Self {
        self.ticks_per_step = ticks.max(1);
        self
    }

    /// adb 可见后不再推进，用于验证 Stalled 与诊断阶段。
    pub fn stalling(mut self) -> Self {
        self.stall_after_adb = true;
        self
    }

    /// 启动后进程立即死亡，用于验证 Failed 与其和 Stopped 的区别。
    pub fn dying(mut self) -> Self {
        self.die_on_start = true;
        self
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for FakeBackend {
    /// 两个应用：一个用户安装、一个系统自带。
    /// 「能否作桌面」不在这里——那是设备的一类能力查询，不是应用的身份。
    fn apps(&self) -> Vec<AppEntry> {
        vec![
            AppEntry {
                package: PackageName::new("com.example.reader"),
                system: false,
                launcher: true,
                label: Some("阅读器".into()),
                icon_key: None,
            },
            AppEntry {
                package: PackageName::new("com.example.launcher"),
                system: true,
                launcher: true,
                label: Some("系统桌面".into()),
                icon_key: None,
            },
        ]
    }

    fn exec(&mut self, cmd: &Command, facts: &mut Facts) -> Result<(), Rejection> {
        match cmd {
            Command::Create { .. } => {
                facts.dir_exists = true;
                self.step = 0;
            }
            Command::Delete => {
                *facts = Facts::default();
                self.step = 0;
            }
            Command::Start => {
                facts.process_alive = true;
                facts.ever_launched = true;
                facts.stop_requested = false;
                facts.boot_stalled = false;
                self.step = 1;
                self.counter = 0;
            }
            Command::Stop => {
                // 优雅停止：进程退出。真实后端会先关流会话，再 emu kill。
                facts.process_alive = false;
                facts.adb_visible = false;
                facts.services = Default::default();
                facts.boot_completed = false;
                facts.session_count = 0;
                self.step = 0;
            }
            Command::RefreshApps
            | Command::Install { .. }
            | Command::Uninstall { .. }
            | Command::Launch { .. }
            | Command::SetDefaultHome { .. }
            | Command::ForceStop { .. }
            | Command::ClearData { .. } => {}
        }
        Ok(())
    }

    fn abort(&mut self, facts: &mut Facts) {
        // 强制终止：不走优雅流程，直接让进程消失。
        facts.process_alive = false;
        facts.adb_visible = false;
        facts.services = Default::default();
        facts.boot_completed = false;
        facts.session_count = 0;
        self.step = 0;
    }

    fn observe(&mut self, facts: &mut Facts) {
        if !facts.process_alive {
            return;
        }
        if self.die_on_start && self.step >= 1 {
            facts.process_alive = false;
            facts.adb_visible = false;
            facts.services = Default::default();
            return;
        }
        self.counter += 1;
        if self.counter < self.ticks_per_step {
            return;
        }
        self.counter = 0;
        match self.step {
            0 => {}
            1 => {
                facts.adb_visible = true;
                self.step = 2;
            }
            2 if self.stall_after_adb => {}
            2 => {
                facts.services.package = true;
                self.step = 3;
            }
            3 => {
                facts.services.activity = true;
                self.step = 4;
            }
            4 => {
                facts.services.window = true;
                self.step = 5;
            }
            5 => {
                facts.boot_completed = true;
                self.step = 6;
            }
            _ => {}
        }
    }
}
