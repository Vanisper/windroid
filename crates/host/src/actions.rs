//! 应用可执行的操作。
//!
//! 操作与应用分开：应用只有自身的事实（包名、标签、能否启动），操作是
//! 由「应用事实 + 设备状态 + 一类能力的查询结果」共同派生的。这样新增一类
//! 操作不需要改应用结构，也不需要界面知道判据。
//!
//! 每个操作自带三件事：标识（给宿主执行）、文案（给用户看）、是否可用
//! （不可用时给原因）。界面按同一个形状渲染，不写分支。

use contract::AppEntry;

/// 操作的动作。
///
/// 序列化后由界面原样回传，宿主据此执行。加新操作只需加一个变体。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// 打开应用窗口。
    Launch,
    /// 设为默认桌面。
    SetDefaultHome,
    /// 结束应用进程。
    ForceStop,
    /// 清除应用数据。
    ClearData,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Launch => "打开",
            Self::SetDefaultHome => "设为默认桌面",
            Self::ForceStop => "结束进程",
            Self::ClearData => "清除数据",
        }
    }
}

/// 一个可执行的操作。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    pub kind: Kind,
    pub label: &'static str,
    pub enabled: bool,
    /// 不可用的原因。可用时为 None。
    pub reason: Option<&'static str>,
    /// 危险的操作用不同的视觉处理，界面据此决定要不要二次确认。
    pub destructive: bool,
}

impl Action {
    fn new(kind: Kind, enabled: bool, reason: Option<&'static str>, destructive: bool) -> Self {
        Self { kind, label: kind.label(), enabled, reason, destructive }
    }
}

/// 派生某应用的操作集。
///
/// 判据集中在这里，不散在界面。新增一类能力时，先加查询（见
/// runtime::catalog::HomeState），再在这里加一条派生规则。
pub fn for_app(
    app: &AppEntry,
    home: &runtime::catalog::HomeState,
    device_usable: bool,
) -> Vec<Action> {
    let mut out = Vec::new();

    // 打开：需要设备可用，且应用声明了 LAUNCHER。
    out.push(Action::new(
        Kind::Launch,
        device_usable && app.launcher,
        if !device_usable {
            Some("实例未就绪")
        } else if !app.launcher {
            Some("此应用无法直接启动")
        } else {
            None
        },
        false,
    ));

    // 设为默认桌面：只有声明了 HOME 且不是当前默认的应用才提供。
    // 不提供时不是「禁用」，而是根本不出现——对一个不能当桌面的应用
    // 显示一条灰掉的「设为默认桌面」只是噪音。
    if home.is_candidate(app.package.as_str()) && !home.is_default(app.package.as_str()) {
        out.push(Action::new(
            Kind::SetDefaultHome,
            device_usable,
            (!device_usable).then_some("实例未就绪"),
            false,
        ));
    }

    // 结束进程与清除数据：系统应用不提供。对系统应用做这两件事的后果
    // 用户无法预期，且通常也不是他想要的。
    if !app.system {
        out.push(Action::new(
            Kind::ForceStop,
            device_usable,
            (!device_usable).then_some("实例未就绪"),
            false,
        ));
        out.push(Action::new(
            Kind::ClearData,
            device_usable,
            (!device_usable).then_some("实例未就绪"),
            true,
        ));
    }

    out
}
