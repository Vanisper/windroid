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

use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(test)]
use contract::Ack;
use contract::{Command, Facts, Phase, Rejection, Snapshot, State};
use device::Handle;
use nsh_engine::{HostChannel, JsonValue};

pub mod actions;
mod library;
pub use library::Library;
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

/// 界面绑定的宿主能力。许可页只使用 HTTPS。
pub const CAPABILITIES_JSON: &str = r#"{"host":["windroid.*"],"openUrl":{"schemes":["https"]}}"#;

/// 为宿主测试挂载一个设备，使用与应用相同的快照和命令通道。
pub struct Bridge {
    _library: Arc<Library>,
    handle: Handle,
    channel: HostChannel,
}

impl Bridge {
    pub fn attach(handle: Handle, channel: HostChannel, events: device::Events) -> Arc<Self> {
        let library = Library::fixture(channel.clone(), handle.clone(), events);
        Arc::new(Self {
            _library: library,
            handle,
            channel,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.handle.snapshot()
    }
    pub fn handle(&self) -> &Handle {
        &self.handle
    }
    pub fn channel(&self) -> &HostChannel {
        &self.channel
    }
}

fn device_command(data: &JsonValue) -> Result<Command, Rejection> {
    if let Some(cmd) = translate_action(data) {
        return Ok(cmd);
    }
    if data["kind"] == "install" {
        let path = data["apkPath"]
            .as_str()
            .or_else(|| data["apk_path"].as_str())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| library::error("请选择 APK 文件"))?;
        return Ok(Command::Install {
            apk_path: path.to_owned(),
        });
    }
    serde_json::from_value(data.clone()).map_err(|e| library::error(format!("命令无法解析: {e}")))
}

#[cfg(test)]
fn execute(handle: &Handle, data: &JsonValue) -> JsonValue {
    to_json(&match device_command(data) {
        Ok(cmd) => handle.send(cmd),
        Err(reason) => Ack::Rejected { reason },
    })
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
    /// 按 Android 包来源划分的应用目录，省略没有成员的类别。
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
    pub candidates: Vec<String>,
    pub candidate_labels: BTreeMap<String, String>,
}

/// 应用目录中的一个来源类别。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupView {
    pub id: String,
    pub name: String,
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

impl SnapshotView {
    pub fn of(snapshot: Snapshot) -> Self {
        let summary = describe(snapshot.state, snapshot.phase, &snapshot.facts);
        let stuck = stuck_at(snapshot.state, &snapshot.facts);
        let capabilities = contract::Capabilities::of(snapshot.state);
        Self {
            stuck_at: stuck,
            summary,
            capabilities,
            groups: Vec::new(),
            desktop: None,
            snapshot,
        }
    }

    /// 由应用清单派生分组。
    ///
    /// 三条规则：
    /// - **能否启动**由 LAUNCHER 决定，这是进「已安装 / 系统应用」的硬条件；
    /// - **桌面**由 HOME 决定，与系统/用户正交——用户装的第三方桌面是合格的；
    /// - 只声明 HOME 的应用出现在桌面选择器。
    pub fn with_apps(mut self, apps: &[contract::AppEntry], home: &contract::Desktop) -> Self {
        self.desktop = Some(DesktopView {
            default_package: home.default.as_ref().map(|p| p.to_string()),
            candidates: home.candidates.iter().map(ToString::to_string).collect(),
            candidate_labels: home.candidate_labels.clone(),
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
                id: id.to_owned(),
                name: name.to_owned(),
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
