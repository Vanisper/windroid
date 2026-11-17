//! Windroid 呈现域：设备的命令/事件/快照到界面的桥。
//!
//! 本 crate 的职责边界必须清楚：
//!
//! - **界面不直接触达设备域**。TSX 里的错误不能影响窗口与流，因此命令一律经过
//!   这里的 [Bridge]。
//! - **不阻塞渲染线程**。nsh 的 host handler 跑在引擎线程上，阻塞它等于卡住界面。
//!   所以命令交给一个独立的分发线程，事件由另一个线程推送。
//!
//! 窗口所有权与关闭拦截见 [surface]，那部分依赖平台。

use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use contract::{Ack, Command, Facts, Phase, Rejection, Snapshot, State};
use device::Handle;
use nsh_engine::{HostCall, HostChannel, JsonValue};

pub mod actions;
pub mod surface;
pub mod video;

/// 宿主与界面之间的消息名。改名即是破坏性变更：TSX 侧的字符串必须同步。
pub mod names {
    /// 界面调用，执行一条命令。参数是命令 JSON，返回值是回执 JSON。
    pub const COMMAND: &str = "windroid.command";
    /// 界面调用，取当前快照。无参数。
    pub const SNAPSHOT: &str = "windroid.snapshot";
    /// 宿主推送，状态或事实变化。载荷是事件 JSON。
    pub const EVENT: &str = "windroid.event";
    /// 界面调用，取运行时自检结果。无参数。
    pub const READINESS: &str = "windroid.readiness";
}

/// 运行时自检结果，序列化为界面可读的 JSON。
///
/// 界面拿到的是「缺什么、后果是什么」，而不是错误码——用户在首次启动时
/// 需要的是一句能照做的话。
fn readiness_view() -> ReadinessView {
    let r = runtime::prepare::check();
    ReadinessView {
        usable: r.usable(),
        avds: r.avds,
        components: r
            .components
            .iter()
            .map(|c| ComponentView {
                name: c.name,
                ok: c.ok,
                path: c.path.as_ref().map(|p| p.display().to_string()),
                remedy: c.remedy,
            })
            .collect(),
    }
}

/// 界面所需的能力声明，对应 nsh build 产出的 capabilities.json。
///
/// 必须显式声明：没有 capabilities 的 app，host 一项是空的——
/// 每个 host.call 都会被拒。用前缀 "windroid.*" 而不是 true，
/// 后者展开成 ["*"]，等于把整套宿主接口全开。
pub const CAPABILITIES_JSON: &str = r#"{"host":["windroid.*"]}"#;

/// 把设备域接到界面。
pub struct Bridge {
    handle: Handle,
    channel: HostChannel,
}

impl Bridge {
    /// 建立桥并启动后台线程。
    ///
    /// 两个线程：
    /// - 分发线程串行执行命令。串行是必要的：命令有先后依赖（先 Create 才能 Start），
    ///   并发执行会让准入判断与实际状态错位。
    /// - 转发线程把事件推给界面。
    ///
    /// 不用「每条命令一个线程」：用户连点时线程数无界，且顺序失去保证。
    pub fn attach(handle: Handle, channel: HostChannel, events: device::Events) -> Arc<Self> {
        let bridge = Arc::new(Self {
            handle: handle.clone(),
            channel: channel.clone(),
        });

        // 命令：handler 只入队，立即返回，引擎线程不受阻。
        let (tx, rx) = mpsc::channel::<HostCall>();
        channel.handle(names::COMMAND, move |call: HostCall| {
            if let Err(e) = tx.send(call) {
                // 分发线程已退出：直接拒绝，不让界面无限等待。
                e.0.reject("命令分发线程已退出");
            }
        });

        let dispatcher = handle.clone();
        thread::Builder::new()
            .name("windroid-dispatch".into())
            .spawn(move || {
                for call in rx {
                    let reply = execute(&dispatcher, call.data());
                    call.resolve(reply);
                }
            })
            .expect("spawn command dispatcher");

        // 快照：纯读，就地回复。
        let snap_handle = handle.clone();
        // 自检：读本机现状，不启动任何进程，因此可以直接在引擎线程上做。
        channel.handle(names::READINESS, move |call: HostCall| {
            call.resolve(to_json(&readiness_view()));
        });

        channel.handle(names::SNAPSHOT, move |call: HostCall| {
            let snap = snap_handle.snapshot();
            let apps = snap.apps.clone();
            let home = runtime::catalog::home_state(&snap.serial).unwrap_or_default();
            let mut view = SnapshotView::of(snap);
            view.readiness = Some(readiness_view());
            view = view.with_apps(&apps, &home);
            if std::env::var_os("WINDROID_DEBUG_APPS").is_some() {
                eprintln!(
                    "[snapshot] apps={} groups={} home={:?} id={}",
                    apps.len(),
                    view.groups.len(),
                    home.default,
                    view.snapshot.serial
                );
            }
            call.resolve(to_json(&view));
        });

        // 事件：独立线程转发。
        let ch = channel.clone();
        thread::Builder::new()
            .name("windroid-events".into())
            .spawn(move || {
                while let Some(e) = events.recv() {
                    if ch.send(names::EVENT, to_json(&e)).is_err() {
                        log::warn!("事件名被拒绝，转发停止");
                        break;
                    }
                }
            })
            .expect("spawn event forwarder");

        bridge
    }

    /// 当前快照。
    pub fn snapshot(&self) -> Snapshot {
        self.handle.snapshot()
    }

    /// 底层句柄，供不需要界面的调用方（例如集成测试）使用。
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// 通道，供测试断言推送内容。
    pub fn channel(&self) -> &HostChannel {
        &self.channel
    }
}

/// 把界面的调用翻译成命令，结果编码成回传 JSON。
fn execute(handle: &Handle, data: &JsonValue) -> JsonValue {
    // 两种输入形状：完整的命令（create/start/stop/install），或一条操作
    // （launch/setDefaultHome/...）。操作只带 kind 与包名，因此先试操作。
    let cmd = match translate_action(data) {
        Some(c) => c,
        None => match serde_json::from_value(data.clone()) {
            Ok(c) => c,
            Err(e) => {
                // 解析失败是界面侧的 bug，不能静默：回一个明确的拒绝。
                return to_json(&Ack::Rejected {
                    reason: Rejection::Error {
                        code: contract::ErrorCode::Other,
                        detail: format!("命令无法解析: {e}"),
                    },
                });
            }
        },
    };
    to_json(&handle.send(cmd))
}

/// 把界面回传的操作翻译成设备域命令。
///
/// 操作的形状由 [actions] 定义，界面原样回传 `kind` 与包名。新增操作时
/// 两侧各加一处，不涉及格式协商。
fn translate_action(data: &JsonValue) -> Option<Command> {
    let kind = data.get("kind")?.as_str()?;
    let package = contract::PackageName::new(data.get("package")?.as_str()?);
    Some(match kind {
        "launch" => Command::Launch { package },
        "setDefaultHome" => Command::SetDefaultHome { package },
        "forceStop" => Command::ForceStop { package },
        "clearData" => Command::ClearData { package },
        _ => return None,
    })
}

/// 序列化到通道格式。
///
/// 失败说明类型定义有 bug。用 null 兜底而非 panic：转发线程 panic 会静默丢掉
/// 后续所有事件，比一条坏消息严重得多。
fn to_json<T: serde::Serialize>(v: &T) -> JsonValue {
    serde_json::to_value(v).unwrap_or(JsonValue::Null)
}

// ---------------------------------------------------------------------------
// 状态展示
// ---------------------------------------------------------------------------

/// 状态的一句话说明，供界面直接显示。
///
/// 放在这里而不是 TSX：它依赖 [Facts] 的语义，写两遍必然漂移。
/// 界面拿到的是 `Snapshot` 加 `summary` 字段（见 [SnapshotView]），不自己拼。
pub fn describe(state: State, phase: Option<Phase>, facts: &Facts) -> String {
    match state {
        State::Absent => "实例尚未创建".into(),
        State::Stopped => "已停止".into(),
        State::Starting => match phase {
            Some(Phase::WaitingAdb) => "正在启动，等待 adb 识别".into(),
            Some(Phase::Warming) => "正在启动，系统服务尚未就绪".into(),
            _ => format!("正在启动，服务 {}/3", facts.services.count()),
        },
        State::Stalled => format!(
            "启动较慢，服务 {}/3，可继续等待或查看日志",
            facts.services.count()
        ),
        State::Ready => "就绪".into(),
        State::Streaming => format!("运行中，{} 个窗口", facts.session_count),
        State::Stopping => "正在停止".into(),
        State::Failed => "启动失败".into(),
    }
}

/// 发给界面的快照：设备域的快照加上由此派生的展示文案。
///
/// 为什么不在 TSX 里拼：状态说明依赖 [Facts] 的语义（三个服务分别意味着
/// 什么、卡在 adb 与服务层如何区分）。放在两侧各写一份，改一处忘另一处，
/// 用户看到的解释就会与真实原因不符。文案单点维护在这一侧。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotView {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    /// 一句话说明当前状态。
    pub summary: String,
    /// 卡点：启动中或 Stalled 时指出卡在哪一步，其余状态为 None。
    pub stuck_at: Option<&'static str>,
    /// 当前可执行的操作。界面按钮的禁用以此为准，不自己判断状态。
    pub capabilities: contract::Capabilities,
    /// 桌面状态。派生操作时需要，不直接下发。
    #[serde(skip)]
    pub home_state: Option<runtime::catalog::HomeState>,
    /// 运行时自检。null 表示还没查。
    pub readiness: Option<ReadinessView>,
    /// 应用库分组。界面直接渲染，不自己判断归属。
    pub groups: Vec<GroupView>,
    /// 桌面状态（哪些能当桌面、当前是谁）。界面用它在卡片上标记。
    ///
    /// 与 groups 分开：这是设备的一类能力状态，不是应用分组。
    pub desktop: Option<DesktopView>,
}

/// 桌面相关状态的界面形态。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopView {
    /// 当前默认桌面的包名。
    pub default_package: Option<String>,
}

/// 一个应用分组。
///
/// 分组规则留在宿主：它依赖 AppEntry 的三个判定（系统/能否启动/能否作桌面），
/// 界面复制这套判断必然漂移。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupView {
    pub id: &'static str,
    pub name: &'static str,
    /// 组内应用数。
    pub count: usize,
    pub apps: Vec<AppItemView>,
}

/// 应用在界面上的形态。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppItemView {
    pub package: String,
    /// 展示名。缺失时用包名，界面不显示空白项。
    pub label: String,
    pub system: bool,
    /// 能否打开独立窗口。
    pub launchable: bool,
    /// 可执行的操作。由应用事实、设备状态与能力查询共同派生。
    pub actions: Vec<crate::actions::Action>,
}

/// 自检结果的界面形态。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadinessView {
    pub usable: bool,
    pub avds: Vec<String>,
    pub components: Vec<ComponentView>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentView {
    pub name: &'static str,
    pub ok: bool,
    pub path: Option<String>,
    pub remedy: &'static str,
}

impl SnapshotView {
    pub fn of(snapshot: Snapshot) -> Self {
        let summary = describe(snapshot.state, snapshot.phase, &snapshot.facts);
        let stuck = stuck_at(snapshot.state, &snapshot.facts);
        let capabilities = contract::Capabilities::of(snapshot.state);
        Self {
            stuck_at: stuck,
            summary,
            capabilities,
            readiness: None,
            groups: Vec::new(),
            desktop: None,
            home_state: None,
            snapshot,
        }
    }

    /// 由应用清单派生分组。
    ///
    /// 三条规则：
    /// - **能否启动**由 LAUNCHER 决定，这是进「已安装 / 系统应用」的硬条件；
    /// - **桌面**由 HOME 决定，与系统/用户正交——用户装的第三方桌面是合格的；
    /// - 没有任何 LAUNCHER 但声明了 HOME 的（只作桌面），单独成组，
    ///   否则它们在界面上无处可去。
    pub fn with_apps(
        mut self,
        apps: &[contract::AppEntry],
        home: &runtime::catalog::HomeState,
    ) -> Self {
        self.home_state = Some(home.clone());
        self.desktop = Some(DesktopView {
            default_package: home.default.clone(),
        });
        let item = |a: &contract::AppEntry| AppItemView {
            package: a.package.to_string(),
            label: a.label.clone().unwrap_or_else(|| a.package.to_string()),
            system: a.system,
            launchable: a.launcher,
            actions: crate::actions::for_app(a, home, self.capabilities.launch_app),
        };
        let sorted = |v: Vec<_>| {
            let mut v = v;
            v.sort_by(|a: &contract::AppEntry, b| {
                a.label
                    .as_deref()
                    .unwrap_or(a.package.as_str())
                    .to_lowercase()
                    .cmp(&b.label.as_deref().unwrap_or(b.package.as_str()).to_lowercase())
            });
            v
        };

        let installed: Vec<_> = sorted(
            apps.iter()
                .filter(|a| a.launcher && !a.system)
                .cloned()
                .collect(),
        );
        let system: Vec<_> = sorted(
            apps.iter()
                .filter(|a| a.launcher && a.system)
                .cloned()
                .collect(),
        );

        let mut groups = Vec::new();
        let mut push = |id: &'static str, name: &'static str, list: Vec<contract::AppEntry>| {
            if list.is_empty() {
                return;
            }
            groups.push(GroupView {
                id,
                name,
                count: list.len(),
                apps: list.iter().map(item).collect(),
            });
        };
        push("installed", "已安装", installed);
        push("system", "系统应用", system);
        self.groups = groups;
        self
    }
}

/// 卡在哪一步。用户的应对随卡点不同而完全不同——这是 Stalled 界面的核心信息。
fn stuck_at(state: State, facts: &Facts) -> Option<&'static str> {
    if !matches!(state, State::Starting | State::Stalled) {
        return None;
    }
    if !facts.process_alive {
        return Some("process");
    }
    if !facts.adb_visible {
        return Some("adb");
    }
    if !facts.services.package {
        return Some("package");
    }
    if !facts.services.activity {
        return Some("activity");
    }
    if !facts.services.window {
        return Some("window");
    }
    None
}

#[cfg(test)]
mod tests;
