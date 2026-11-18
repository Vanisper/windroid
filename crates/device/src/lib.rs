//! Windroid 设备域：一台 Android 实例的唯一写者。
//!
//! 三条约束在本 crate 里落地：
//!
//! 1. **单一写者**。所有状态变更都发生在 actor 线程内，外部只能投递命令。
//! 2. **状态由事实派生**。actor 通过 [contract::State::of] 计算状态，发布时将结果写入快照。
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
    Ack, AppEntry, Command, Event, Facts, InstanceId, PackageName, Phase, Rejection, Seq, Snapshot,
    State,
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
    SetSessionCount(u32, Sender<Ack>),
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
        self.request(|reply| Msg::Command(cmd, reply))
    }

    /// 流域按实例上报当前活跃会话总数。回执到达时快照已包含本次结果。
    /// 归零始终可上报。正数需要实例处于 Ready 或 Streaming。
    pub fn set_session_count(&self, count: u32) -> Ack {
        self.request(|reply| Msg::SetSessionCount(count, reply))
    }

    fn request(&self, message: impl FnOnce(Sender<Ack>) -> Msg) -> Ack {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(message(tx)).is_err() {
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
        self.shared
            .snapshot
            .lock()
            .expect("snapshot poisoned")
            .clone()
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
        serial,
        state: State::Absent,
        phase: None,
        facts: Facts::default(),
        apps: Vec::new(),
        catalog_status: contract::CatalogStatus::Pending,
        desktop: contract::Desktop::default(),
        seq: 0,
    };
    let shared = Arc::new(Shared {
        snapshot: Mutex::new(initial),
    });

    let handle = Handle {
        id: id.clone(),
        tx,
        shared: Arc::clone(&shared),
    };

    thread::Builder::new()
        .name(format!("device-{id}"))
        .spawn(move || {
            let mut actor = Actor {
                id,
                facts: Facts::default(),
                seq: 0,
                boot: None,
                timeouts,
                shared,
                events: ev_tx,
            };
            actor.run(rx, &mut backend);
        })
        .expect("spawn device actor");

    (handle, Events { rx: ev_rx })
}

/// 三个截止时间都属于一次成功发起的启动。服务层从首次看见 adb 开始计时。
struct BootAttempt {
    started: Instant,
    adb_seen: Option<Instant>,
}

struct Actor {
    id: InstanceId,
    facts: Facts,
    seq: Seq,
    boot: Option<BootAttempt>,
    timeouts: Timeouts,
    shared: Arc<Shared>,
    events: Sender<Event>,
}

impl Actor {
    fn run<B: Backend>(&mut self, rx: Receiver<Msg>, backend: &mut B) {
        let mut next_observe = Instant::now();
        loop {
            if Instant::now() >= next_observe {
                self.observe(backend);
                next_observe = Instant::now() + self.observe_interval();
            }
            match rx.recv_timeout(next_observe.saturating_duration_since(Instant::now())) {
                Ok(Msg::Command(cmd, reply)) => {
                    let ack = self.handle_command(&cmd, backend);
                    // Start 之后立即采用启动时的节奏。重复命令不能把观测一直往后推。
                    next_observe = next_observe.min(Instant::now() + self.observe_interval());
                    let _ = reply.send(ack);
                }
                Ok(Msg::SetSessionCount(count, reply)) => {
                    let state = State::of(&self.facts);
                    let ack = if count > 0 && !state.accepts_stream() {
                        Ack::Rejected {
                            reason: Rejection::NotReady { state },
                        }
                    } else {
                        if self.facts.session_count != count {
                            self.facts.session_count = count;
                            self.publish(backend);
                        }
                        Ack::Accepted { seq: self.seq }
                    };
                    let _ = reply.send(ack);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn observe<B: Backend>(&mut self, backend: &mut B) {
        backend.observe(&mut self.facts);
        if !self.facts.process_alive || self.facts.services.ready() {
            self.boot = None;
            self.facts.boot_stalled = false;
        } else if let Some(boot) = &mut self.boot {
            let now = Instant::now();
            if now.duration_since(boot.started) >= self.timeouts.hard {
                // 取走本轮计时器，同一轮只回收一次，下一次 Start 会新建计时器。
                self.boot = None;
                log::warn!("device {} 启动超过硬上限，强制终止", self.id);
                backend.abort(&mut self.facts);
                backend.observe(&mut self.facts);
                self.facts.aborted = true;
                self.facts.boot_stalled = self.facts.process_alive && !self.facts.services.ready();
            } else if self.facts.adb_visible {
                let seen = *boot.adb_seen.get_or_insert(now);
                self.facts.boot_stalled = now.duration_since(seen) >= self.timeouts.services;
            } else {
                self.facts.boot_stalled = now.duration_since(boot.started) >= self.timeouts.adb;
            }
        }
        if !self.facts.process_alive {
            self.facts.session_count = 0;
        }
        self.publish(backend);
    }

    /// 启动态按本轮最近的截止时间唤醒，其余状态降低开销。
    fn observe_interval(&self) -> Duration {
        let base = self.timeouts.poll.max(Duration::from_millis(1));
        let mut interval = match State::of(&self.facts) {
            State::Starting | State::Stalled | State::Stopping => base,
            State::Ready | State::Streaming => base * 10,
            State::Stopped | State::Failed => base * 20,
            State::Absent => base * 40,
        };
        if let Some(boot) = &self.boot {
            interval = interval.min(self.timeouts.hard.saturating_sub(boot.started.elapsed()));
            if !self.facts.boot_stalled {
                let (since, budget) = match (self.facts.adb_visible, boot.adb_seen) {
                    (true, Some(seen)) => (seen, self.timeouts.services),
                    _ => (boot.started, self.timeouts.adb),
                };
                interval = interval.min(budget.saturating_sub(since.elapsed()));
            }
        }
        interval
    }

    fn handle_command<B: Backend>(&mut self, cmd: &Command, backend: &mut B) -> Ack {
        let state = State::of(&self.facts);

        if matches!(cmd, Command::Stop) && matches!(state, State::Stopped | State::Stopping) {
            return Ack::Accepted { seq: self.seq };
        }

        // 命令准入：在错误的状态下拒绝，并说明当前是什么状态。
        // 这不是「错误」，是预期内的状况，用户能理解并重试。
        if let Some(wanted) = guard(cmd, state) {
            return Ack::Rejected {
                reason: Rejection::WrongState {
                    current: state,
                    wanted: wanted.to_string(),
                },
            };
        }

        // 保留上次诊断，直到后端实际受理本次请求。拒绝时也不泄露半写的事实。
        let mut facts = self.facts.clone();
        if matches!(cmd, Command::Start) {
            facts.stop_requested = false;
            facts.aborted = false;
            facts.boot_stalled = false;
        }
        if let Err(reason) = backend.exec(cmd, &mut facts) {
            return Ack::Rejected { reason };
        }
        self.facts = facts;
        match cmd {
            Command::Start => {
                self.boot = Some(BootAttempt {
                    started: Instant::now(),
                    adb_seen: None,
                });
            }
            Command::Stop => {
                self.facts.stop_requested = true;
                self.facts.boot_stalled = false;
                self.boot = None;
            }
            _ => {}
        }

        self.seq += 1;
        let seq = self.seq;
        self.observe(backend);
        Ack::Accepted { seq }
    }

    /// 把同一次观测的事实和目录一起写进快照，然后通知订阅者。
    fn publish<B: Backend>(&mut self, backend: &B) {
        let apps = backend.apps();
        let desktop = backend.desktop();
        let catalog_status = backend.catalog_status();
        let mut events = Vec::new();
        {
            let mut snapshot = self.shared.snapshot.lock().expect("snapshot poisoned");
            // 桌面与应用同属一个目录。单独更改默认桌面也要唤醒取快照的订阅者。
            if snapshot.apps != apps
                || snapshot.desktop != desktop
                || snapshot.catalog_status != catalog_status
            {
                snapshot.catalog_status = catalog_status;
                self.seq += 1;
                events.push(Event::AppsChanged {
                    seq: self.seq,
                    apps: apps.clone(),
                });
                snapshot.apps = apps;
                snapshot.desktop = desktop;
            }
            if snapshot.facts != self.facts {
                self.seq += 1;
                let from = Some(snapshot.state);
                snapshot.state = State::of(&self.facts);
                snapshot.phase = Phase::of(&self.facts);
                snapshot.facts = self.facts.clone();
                events.push(Event::StateChanged {
                    seq: self.seq,
                    from,
                    to: snapshot.state,
                    phase: snapshot.phase,
                    facts: self.facts.clone(),
                });
            }
            snapshot.seq = self.seq;
        }
        for event in events {
            let _ = self.events.send(event);
        }
    }
}

/// 命令准入规则。返回 Some(wanted) 表示拒绝，并说明需要什么状态。
///
/// 判断取自 [contract::Capabilities]：界面按钮的禁用与这里的准入是同一份
/// 结果。两侧各写一遍会漂移，而漂移的表现是「按钮能点，点了报错」。
///
/// 已停止或正停止时的重复 Stop 由调用方在准入前幂等处理。
fn guard(cmd: &Command, state: State) -> Option<&'static str> {
    let caps = contract::Capabilities::of(state);
    match cmd {
        Command::Create { .. } => (!caps.create).then_some("不存在的实例"),
        Command::Delete => (!caps.delete).then_some("已停止"),
        Command::Start => (!caps.start).then_some("已停止"),
        Command::Stop => (!caps.stop).then_some("运行中的实例"),
        Command::Install { .. } => (!caps.install).then_some("系统就绪"),
        Command::Uninstall { .. } => (!caps.install).then_some("系统就绪"),
        Command::Launch { .. } => (!caps.launch_app).then_some("系统就绪"),
        Command::RefreshApps => (!caps.refresh_apps).then_some("系统就绪"),
        // 这三个都是对已装应用的操作，与启动同样的前提：设备完全可用。
        Command::SetDefaultHome { .. } | Command::ForceStop { .. } | Command::ClearData { .. } => {
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
