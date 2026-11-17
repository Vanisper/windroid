//! 副作用边界：设备域的 actor 通过它碰真实世界。
//!
//! 定义在契约层而非设备域，是为了让依赖方向成立：真实实现属于运行环境域，
//! 而运行环境域在设备域**之下**。若 trait 留在设备域，底层实现就得反向依赖
//! 上层——这正是 M0 阶段假后端能糊过去、真后端一接就暴露的问题。

use std::time::Duration;

use crate::{AppEntry, Command, Facts, Rejection};

/// 执行命令的真实副作用，并回报观测到的事实。
///
/// 把副作用收到这个 trait 后面，是为了让状态机在没有模拟器、没有子进程的
/// 情况下也能被完整测试。真实实现才是碰 emulator、adb、文件系统的地方。
pub trait Backend: Send + 'static {
    /// 执行命令中「发起动作」的部分。
    ///
    /// 返回值只表示**命令是否被受理**，不表示动作已完成。安装是否成功必须
    /// 通过后续的 [Backend::observe] 回读验证——adb 的退出码与 Success 文本
    /// 都不是证据（见 docs/架构/02-设备域.md 第七节）。
    fn exec(&mut self, cmd: &Command, facts: &mut Facts) -> Result<(), Rejection>;

    /// 强制终止进程，用于超过硬上限时回收端口与磁盘。
    ///
    /// 与 `Stop` 命令的区别：那是用户请求的优雅停止，这是系统为避免无限期
    /// 占用资源而做出的决定。实现应尽快终止，不做清理等待。
    fn abort(&mut self, facts: &mut Facts);

    /// 观测当前事实，就地更新。
    ///
    /// 实现方只能写观测到的东西，不要在这里推导状态——状态由
    /// [crate::State::of] 从事实算出来。
    fn observe(&mut self, facts: &mut Facts);

    /// 当前应用清单。
    ///
    /// 与 [Facts] 分开传：应用清单不是「状态事实」，它是按需刷新的目录，
    /// 参与状态推导毫无意义。混进 Facts 会让 snapshot_state_always_equals_
    /// derived_state 那条断言失去意义。
    ///
    /// 默认空实现：不关心应用的实现（如假后端）不必实现它。
    fn apps(&self) -> Vec<AppEntry> {
        Vec::new()
    }
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
    /// 过渡态（启动中、停止中）的观测间隔，其余状态按倍数放宽。
    ///
    /// 做成配置而非常量：测试需要它小到毫秒级才跑得快，生产需要它大到
    /// adb 调用不至于成为常态开销。两者只差一个数字，不该有两套代码。
    pub poll: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            adb: Duration::from_secs(60),
            services: Duration::from_secs(180),
            hard: Duration::from_secs(600),
            poll: Duration::from_millis(50),
        }
    }
}

/// 让 `Box<dyn Backend>` 也是 Backend。
///
/// 应用层要在真假后端之间二选一，而 [crate::Backend] 有泛型约束
/// （`spawn<B: Backend>`），不能直接吃泛型接口。这层转发让选择发生在
/// 组装处，而 actor 完全不必知道有几种后端。
impl Backend for Box<dyn Backend> {
    fn exec(&mut self, cmd: &Command, facts: &mut Facts) -> Result<(), Rejection> {
        (**self).exec(cmd, facts)
    }

    fn apps(&self) -> Vec<AppEntry> {
        (**self).apps()
    }

    fn abort(&mut self, facts: &mut Facts) {
        (**self).abort(facts);
    }

    fn observe(&mut self, facts: &mut Facts) {
        (**self).observe(facts);
    }
}
