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
//! 副作用通过 [Backend] 抽象隔离。M0 用假后端验证状态机；接真模拟器时只替换
//! 后端实现，actor 与契约不动。

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use contract::{
    Ack, AppEntry, Command, Event, Facts, InstanceId, LogStream, Phase, Rejection, Seq, Snapshot,
    State,
};

// ---------------------------------------------------------------------------
// Backend：副作用边界
// ---------------------------------------------------------------------------

/// 执行命令的真实副作用，并回报观测到的事实。
///
/// 把副作用收到这个 trait 后面，是为了让状态机在没有模拟器、没有子进程的
/// 情况下也能被完整测试。接真实现时，这里才是碰 emulator、adb、文件系统的地方。
pub trait Backend: Send + 'static {
    /// 执行命令中「发起动作」的部分。
    ///
    /// 返回值只表示**命令是否被受理**，不表示动作已完成。安装是否成功必须
    /// 通过后续的 [Backend::observe] 回读验证——adb 的退出码与 Success 文本
    /// 都不是证据（见 docs/架构/02-设备域.md 第七节）。
    fn exec(&mut self, cmd: &Command, facts: &mut Facts) -> Result<(), Rejection>;

    /// 观测当前事实，就地更新。
    ///
    /// actor 每次循环调用一次。真实实现会在这里看进程是否存活、adb devices、
    /// service list、getprop。实现方只能写观测到的东西，不要在这里推导状态。
    fn observe(&mut self, facts: &mut Facts);
}

/// 启动等待的两层超时。
///
/// 分层的价值是失败归因：第一层超时意味着 adb 层出了问题，第二层超时意味着
/// 系统层出了问题。混成一个超时，用户只能看到「启动失败」。
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// 等待 adb 可见。端口冲突、外部 adb 占用、adbd 未起都会卡在这里。
    pub adb: Duration,
    /// 等待三个核心服务齐备。镜像异常、磁盘慢、OOM 会卡在这里。
    pub services: Duration,
    /// 从进程启动累计的硬上限，防止长时间占用端口与磁盘。
    pub hard: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            adb: Duration::from_secs(60),
            services: Duration::from_secs(180),
            hard: Duration::from_secs(600),
        }
    }
}

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
pub fn spawn<B: Backend>(id: InstanceId, mut backend: B, timeouts: Timeouts) -> (Handle, Events) {
    let (tx, rx) = mpsc::channel::<Msg>();
    let (ev_tx, ev_rx) = mpsc::channel::<Event>();

    let initial = Snapshot {
        id: id.clone(),
        state: State::Absent,
        phase: None,
        facts: Facts::default(),
        apps: Vec::new(),
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
                facts: Facts::default(),
                apps: Vec::<AppEntry>::new(),
                seq: 0,
                last_state: State::Absent,
                last_phase: None,
                started_at: None,
                timeouts,
                shared,
                events: ev_tx,
            };
            actor.run(rx, &mut backend);
        })
        .expect("spawn device actor");

    (handle, Events { rx: ev_rx })
}

struct Actor {
    id: InstanceId,
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
}

impl Actor {
    fn run<B: Backend>(&mut self, rx: Receiver<Msg>, backend: &mut B) {
        loop {
            // 1) 观测事实。这是状态的唯一输入。
            backend.observe(&mut self.facts);
            self.apply_timeouts();
            self.publish_state();

            // 2) 等命令，同时保持观测节奏。
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Msg::Command(cmd, reply)) => {
                    let ack = self.handle_command(&cmd, backend);
                    let _ = reply.send(ack);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
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
        // 两层超时收敛到同一个事实，但诊断阶段不同：
        // 未到 adb 超时前卡在 waiting-adb，之后卡在服务层。
        let stalled = if self.facts.adb_visible {
            waited >= self.timeouts.services
        } else {
            waited >= self.timeouts.adb
        } || waited >= self.timeouts.hard;
        self.facts.boot_stalled = stalled;
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
        g.seq = self.seq;
        g.id = self.id.clone();
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
/// 规则只描述「什么状态能做这件事」，不描述怎么做。
fn guard(cmd: &Command, state: State) -> Option<&'static str> {
    use State::*;
    match cmd {
        Command::Create { .. } => (state != Absent).then_some("不存在的实例"),
        Command::Delete => (state != Stopped).then_some("已停止"),
        Command::Start => (!matches!(state, Stopped | Failed)).then_some("已停止"),
        // 重复 Stop 是幂等的用户意图，不该报错；但 Stopping 时不重复执行。
        Command::Stop => (!matches!(state, Starting | Stalled | Ready | Streaming)).then_some("运行中"),
        // 其余命令都要求系统已经能服务。
        _ if !state.accepts_stream() => Some("系统就绪"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 假后端：M0 用，验证状态机而不碰模拟器
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
            Command::RefreshApps | Command::Install { .. } | Command::Uninstall { .. } | Command::Launch { .. } => {}
        }
        Ok(())
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
