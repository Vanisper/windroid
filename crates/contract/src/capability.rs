//! 当前状态下允许哪些操作。
//!
//! 这个模块的存在是为了避免一件事：把「能不能点」散落在界面的各个按钮上。
//! 散落判断必然遗漏——漏掉的通常是过渡态（启动中、停止中），而那些恰恰
//! 是最不该让用户连点的时候。
//!
//! 所以它是一个纯函数：输入状态，输出一组布尔量。界面只读结果，不自己判断。
//! 设备域的命令准入（[crate::State] 的 `guard`）用同一份判断，两边不会打架。

use crate::State;

/// 某状态下用户可执行的操作集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// 创建实例。只在没有实例时可用。
    pub create: bool,
    /// 删除实例。要求已停止。
    pub delete: bool,
    /// 启动。含从失败态重试。
    pub start: bool,
    /// 停止。
    pub stop: bool,
    /// 打开应用窗口。要求设备完全就绪。
    pub launch_app: bool,
    /// 安装 APK。**只在完全就绪时允许**：安装依赖在线且已引导完成的设备，
    /// 在过渡态发起会失败，或留下半个安装的包状态。
    pub install: bool,
    /// 刷新应用列表。
    pub refresh_apps: bool,
    /// 打开设备视图（整台设备的画面）。
    pub device_view: bool,
    /// 是否处于过渡态。界面据此显示进度而不是让用户猜。
    pub transitioning: bool,
}

impl Capabilities {
    /// 由状态推导。纯函数，无 I/O，可穷举测试。
    pub fn of(state: State) -> Self {
        match state {
            // 没有实例：只能创建。其余操作都没有对象。
            State::Absent => Self {
                create: true,
                delete: false,
                start: false,
                stop: false,
                launch_app: false,
                install: false,
                refresh_apps: false,
                device_view: false,
                transitioning: false,
            },
            // 已停止：可启动、可删除。删除要求停止，所以这里放开。
            State::Stopped => Self {
                create: false,
                delete: true,
                start: true,
                stop: false,
                launch_app: false,
                install: false,
                refresh_apps: false,
                device_view: false,
                transitioning: false,
            },
            // 启动中与 Stalled：设备还没准备好，任何依赖设备的操作都不该放开。
            // 停止也禁——当前实现不支持取消启动，放开会让用户以为能取消。
            State::Starting | State::Stalled => Self {
                create: false,
                delete: false,
                start: false,
                stop: false,
                launch_app: false,
                install: false,
                refresh_apps: false,
                device_view: false,
                transitioning: true,
            },
            // 就绪与运行中：全部放开。
            State::Ready | State::Streaming => Self {
                create: false,
                delete: false,
                start: false,
                stop: true,
                launch_app: true,
                install: true,
                refresh_apps: true,
                device_view: true,
                transitioning: false,
            },
            // 正在停止：设备随时可能消失，任何依赖设备的操作都会失败，
            // 而失败信息对用户毫无意义——他刚点了停止。
            State::Stopping => Self {
                create: false,
                delete: false,
                start: false,
                stop: false,
                launch_app: false,
                install: false,
                refresh_apps: false,
                device_view: false,
                transitioning: true,
            },
            // 失败：只能重试或删除。
            State::Failed => Self {
                create: false,
                delete: true,
                start: true,
                stop: false,
                launch_app: false,
                install: false,
                refresh_apps: false,
                device_view: false,
                transitioning: false,
            },
        }
    }

    /// 设备是否完全可用（依赖设备的操作是否全部放开）。
    pub fn usable(&self) -> bool {
        self.launch_app
    }
}
