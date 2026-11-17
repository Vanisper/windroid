//! Windroid 的域间契约：命令、事件、快照与失败分类。
//!
//! 本 crate 只有数据与纯函数：不依赖 GUI、进程、平台 API，也不依赖其他域。
//! 这条约束是「设备域将来可能拆成独立守护进程」的前提——契约必须可序列化，
//! 见 docs/架构/02-设备域.md 第二节。
//!
//! 状态不由赋值产生，而由事实派生：[Facts] 是唯一输入，[State::of] 是唯一推导，
//! 没有其他代码路径能写状态。

mod backend;
mod capability;

pub use backend::{Backend, Timeouts};
pub use capability::Capabilities;

use serde::{Deserialize, Serialize};
use std::fmt;

#[cfg(test)]
mod tests;

/// 事件游标：单调递增，用于让快照与事件对齐。
pub type Seq = u64;

// ---------------------------------------------------------------------------
// 身份
// ---------------------------------------------------------------------------

/// 实例标识。全局唯一，创建时生成，此后不变。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstanceId(String);

impl InstanceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Android 包名。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageName(String);

impl PackageName {
    pub fn new(p: impl Into<String>) -> Self {
        Self(p.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// 事实
// ---------------------------------------------------------------------------

/// 三个核心服务。判据不是「pm 可用」，而是这三个齐备——见 docs/实测/02-设备域.md。
///
/// 选取理由不是照抄 Appium，而是它们各对应一件我们要做的事：
/// package 出应用清单，activity 供 am start，window 供 scrcpy 出画面。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Services {
    pub activity: bool,
    pub package: bool,
    pub window: bool,
}

impl Services {
    /// 全部齐备。这是进入 Ready 的门限。
    pub fn ready(self) -> bool {
        self.activity && self.package && self.window
    }

    /// 已就绪的服务数量，供 Stalled 诊断展示。
    pub fn count(self) -> u8 {
        u8::from(self.activity) + u8::from(self.package) + u8::from(self.window)
    }

    /// 是否有任一服务已起。用于区分 warming 与 starting。
    pub fn any(self) -> bool {
        self.activity || self.package || self.window
    }
}

/// 从一个实例上能观测到的事实。状态是它的纯函数，别无其他来源。
///
/// 注意 [Facts::boot_stalled]：超时本是时间现象，不是可观测事实。
/// 由 actor 的定时器在超时触发时把它置位，于是时间被吸收成事实，
/// [State::of] 得以保持纯函数——这是「状态从事实派生」能成立的关键一步。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Facts {
    /// 实例目录存在。
    pub dir_exists: bool,
    /// 模拟器进程存活。
    pub process_alive: bool,
    /// 是否曾成功启动过进程（用于区分「从未启动」与「异常退出」）。
    pub ever_launched: bool,
    /// adb devices 中该 serial 处于 device。
    pub adb_visible: bool,
    /// 三个核心服务。
    pub services: Services,
    /// sys.boot_completed == 1。只用于标记最终稳定，不作为操作门限。
    pub boot_completed: bool,
    /// 活跃流会话数，由流域上报。
    pub session_count: u32,
    /// 用户已请求停止。
    pub stop_requested: bool,
    /// 启动等待已超过第二层超时。
    pub boot_stalled: bool,
    /// 超过硬上限后已被强制终止。
    ///
    /// 与 `stop_requested` 分开：那是用户的意图，这是系统的决定。两者都导致
    /// 进程不再存活，但前者是 `Stopped`（预期），后者是 `Failed`（需告知用户）。
    pub aborted: bool,
}

// ---------------------------------------------------------------------------
// 状态：由事实派生
// ---------------------------------------------------------------------------

/// 实例状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// 无实例目录。
    Absent,
    /// 有目录，进程未运行。
    Stopped,
    /// 进程存活，服务未齐备，未超时。
    Starting,
    /// 进程仍活但启动超时——不等于失败，慢机器上可能只是还需等待。
    Stalled,
    /// 服务齐备，无流会话。这是「能实际干活」的最早时刻。
    Ready,
    /// 服务齐备，有流会话。
    Streaming,
    /// 收到停止请求，进程仍在退出中。
    Stopping,
    /// 进程已退出且非预期。
    Failed,
}

impl State {
    /// 唯一的推导入口。
    ///
    /// 判断顺序有语义：进程是否存活优先于服务就绪（进程死了，服务观测就是陈旧值），
    /// 停止请求优先于就绪（用户在停，不该报 Ready）。
    pub fn of(f: &Facts) -> State {
        if !f.dir_exists {
            return State::Absent;
        }
        if !f.process_alive {
            // 硬上限触发的终止是失败：用户等了几分钟，必须知道它没成功，
            // 而不是看到「已停止」以为一切正常。
            // 其余情形：从未启动过不是失败；请求停止后退出是预期行为。
            return if f.aborted || (f.ever_launched && !f.stop_requested) {
                State::Failed
            } else {
                State::Stopped
            };
        }
        if f.stop_requested {
            return State::Stopping;
        }
        if f.services.ready() {
            return if f.session_count > 0 {
                State::Streaming
            } else {
                State::Ready
            };
        }
        if f.boot_stalled {
            return State::Stalled;
        }
        State::Starting
    }

    /// 是否允许建立流会话。
    pub fn accepts_stream(self) -> bool {
        matches!(self, State::Ready | State::Streaming)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            State::Absent => "absent",
            State::Stopped => "stopped",
            State::Starting => "starting",
            State::Stalled => "stalled",
            State::Ready => "ready",
            State::Streaming => "streaming",
            State::Stopping => "stopping",
            State::Failed => "failed",
        };
        f.write_str(s)
    }
}

/// 启动过程中的子阶段。只用于诊断展示，不扩展状态枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// 进程活了，但 adb 还看不见。
    WaitingAdb,
    /// adb 看得见，但 package 服务未起。
    Warming,
    /// 服务部分就绪（如 package 已起、window 未起）。
    Starting,
}

impl Phase {
    /// 仅在 Starting 与 Stalled 下有值——这两个状态下用户最需要知道卡在哪。
    pub fn of(f: &Facts) -> Option<Phase> {
        match State::of(f) {
            State::Starting | State::Stalled => {}
            _ => return None,
        }
        if !f.adb_visible {
            Some(Phase::WaitingAdb)
        } else if !f.services.package {
            Some(Phase::Warming)
        } else {
            Some(Phase::Starting)
        }
    }
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 创建实例所需参数，由运行环境域提供。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    /// 设备模板名，如 phone-1080。
    pub template: String,
    pub image: Image,
    /// 用户数据分区大小，字节。创建时固定，之后不可调整。
    pub disk_bytes: u64,
}

/// 系统镜像。第三级组件：用户可选。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    pub api: u32,
    pub tag: String,
    pub abi: String,
}

/// 发往单个实例 actor 的命令。
///
/// 不携带实例标识：actor 本身就是某个实例的唯一写者，身份由收件方确定。
/// 若命令带 id，就会出现「id 与 actor 身份不符」这种本不该存在的错误态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Command {
    /// 建立实例目录与配置。不启动进程。
    Create { spec: Spec },
    /// 删除实例。要求当前为 Stopped。
    Delete,
    /// 启动模拟器进程并进入就绪等待。
    Start,
    /// 优雅停止：先关流会话，再 emu kill。
    Stop,
    /// 安装 APK。路径为主机侧文件。
    Install { apk_path: String },
    /// 卸载。keepData 为真时保留数据与 UID。
    Uninstall { package: PackageName, keep_data: bool },
    /// 重新拉取应用清单。
    RefreshApps,
    /// 启动应用。不创建窗口——窗口由呈现域订阅事件后创建。
    Launch { package: PackageName },
}

/// 命令的接收回执。
///
/// 接受不等于完成：完成通过事件告知。接受也不等于成功——
/// 安装是否成功必须回读验证，见 docs/架构/02-设备域.md 第七节。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum Ack {
    Accepted { seq: Seq },
    Rejected { reason: Rejection },
}

impl Ack {
    pub fn is_accepted(&self) -> bool {
        matches!(self, Ack::Accepted { .. })
    }
}

/// 拒绝原因。
///
/// 分成两类是因为处理方式不同：预期内的环境状况不该打扰用户，
/// 也不需要用户做任何事；真正的错误才需要上报并给出修复建议。
/// 见 docs/架构总览.md 第 7 节第 7 条。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Rejection {
    /// 当前状态不允许该命令，但这是正常情形，用户可以理解并重试。
    ///
    /// 用 owned 字符串而非 &'static str：返回值要能反序列化，
    /// 而 &'static str 无法从外部数据构造。
    WrongState { current: State, wanted: String },
    /// 尚未就绪。命令可以等系统就绪后再试。
    NotReady { state: State },
    /// 真正的错误。
    Error { code: ErrorCode, detail: String },
}

/// 错误分类。取值对应实测到的失败种类，见 docs/架构/02-设备域.md 第七节。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// 端口区间耗尽。
    NoFreePort,
    /// 系统镜像缺失。
    ImageMissing,
    /// 运行时组件缺失。
    ComponentMissing,
    /// APK 签名与已装版本冲突。
    SignatureMismatch,
    /// 版本降级被拒。
    VersionDowngrade,
    /// 磁盘空间不足。
    OutOfSpace,
    /// APK 本身不可解析。
    InvalidApk,
    /// 目标包不存在。
    PackageNotFound,
    /// adb 层面失败。
    AdbFailure,
    /// 主机侧文件系统操作失败（建目录、写日志、删除实例）。
    Io,
    /// 当前里程碑尚未实现。不是错误配置，是能力缺口，措辞要区分开。
    Unsupported,
    /// 未分类。
    Other,
}

impl ErrorCode {
    /// 把 adb 的 stderr 文本映射成分类。
    ///
    /// 必须解析文本而不是只看退出码：install 空间不足、resolve-activity 无匹配、
    /// am start 组件不存在等情形，退出码都是 0。
    pub fn from_adb_stderr(stderr: &str) -> ErrorCode {
        let s = stderr;
        if s.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") || s.contains("signatures do not match") {
            ErrorCode::SignatureMismatch
        } else if s.contains("INSTALL_FAILED_VERSION_DOWNGRADE") {
            ErrorCode::VersionDowngrade
        } else if s.contains("not enough space") {
            ErrorCode::OutOfSpace
        } else if s.contains("INSTALL_FAILED_INVALID_APK")
            || s.contains("code is missing")
            || s.contains("INSTALL_PARSE_FAILED_NO_CERTIFICATES")
        {
            ErrorCode::InvalidApk
        } else if s.contains("DELETE_FAILED_INTERNAL_ERROR") {
            ErrorCode::PackageNotFound
        } else {
            ErrorCode::Other
        }
    }
}

// ---------------------------------------------------------------------------
// 事件
// ---------------------------------------------------------------------------

/// 应用条目。label 与 icon 异步补全，列表中先以包名占位。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppEntry {
    pub package: PackageName,
    /// 系统应用（pm list packages -s）还是用户安装（-3）。
    pub system: bool,
    /// 展示名，来自主机侧 aapt，可能尚未补全。
    pub label: Option<String>,
    /// 图标缓存键，含 versionCode，应用更新后自动失效。
    pub icon_key: Option<String>,
}

/// 事件。单向、可重放、单调递增游标，不携带回调引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Event {
    /// 状态或子阶段变化。from 为 None 表示首次观测。
    StateChanged {
        seq: Seq,
        from: Option<State>,
        to: State,
        phase: Option<Phase>,
        facts: Facts,
    },
    /// 应用清单变化。
    AppsChanged { seq: Seq, apps: Vec<AppEntry> },
    /// 进度细节，用于 Stalled 诊断。
    Progress { seq: Seq, phase: Phase, detail: String },
    /// 日志行。
    Log { seq: Seq, stream: LogStream, line: String },
}

impl Event {
    pub fn seq(&self) -> Seq {
        match self {
            Event::StateChanged { seq, .. }
            | Event::AppsChanged { seq, .. }
            | Event::Progress { seq, .. }
            | Event::Log { seq, .. } => *seq,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogStream {
    Stdout,
    Stderr,
    /// 我们自己的诊断，不是进程输出。
    Internal,
}

// ---------------------------------------------------------------------------
// 快照
// ---------------------------------------------------------------------------

/// 某一时刻的完整事实集合。
///
/// 快照带 seq：调用方先取快照、再订阅事件时，靠 seq 丢弃已被快照覆盖的旧事件。
/// 没有这个字段，就只能在「先订阅后取快照」和「先取快照后订阅」之间二选一，
/// 两者都会漏或多。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub id: InstanceId,
    pub state: State,
    pub phase: Option<Phase>,
    pub facts: Facts,
    pub apps: Vec<AppEntry>,
    pub seq: Seq,
}
