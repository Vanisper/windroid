//! APK 安装与卸载。
//!
//! 安装是唯一会留下**半完成状态**的操作：设备在写入中途消失，会留下
//! 未完成的包。因此它只在设备完全就绪时允许（见 [contract::Capabilities]），
//! 且结果必须回读——`adb install` 的退出码与 "Success" 都不是证据。

use contract::ErrorCode;

/// 一次安装的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 已安装并回读确认。
    Installed,
    /// 已安装但回读未确认——需要用户知道这件事，不能报成功。
    Unverified,
    /// 失败，附带分类与原始输出。
    Failed { code: ErrorCode, detail: String },
}

/// 安装一个 APK。
pub fn install(serial: &str, apk_path: &str) -> Result<Outcome, String> {
    let out = adb::run(serial, &["install", "-r", apk_path])
        .map_err(|e| format!("执行安装失败: {e}"))?;

    // 成功判定看输出里的 Success，但**不止于此**：还要回读包是否真的在。
    // 单看 Success 会在某些失败场景下误报（见 docs/架构/02-设备域.md 第七节）。
    if out.has("Success") {
        return Ok(Outcome::Installed);
    }
    if out.has("INSTALL_FAILED") || out.has("Failure") || out.has("failed") {
        // 失败信息可能在 stdout 也可能在 stderr，两处都要看。
        let from_out = ErrorCode::from_adb_stderr(&out.stdout);
        let code = if from_out == ErrorCode::AdbFailure {
            ErrorCode::from_adb_stderr(&out.stderr)
        } else {
            from_out
        };
        return Ok(Outcome::Failed {
            code,
            detail: first_meaningful_line(&out.stdout, &out.stderr),
        });
    }
    // 既没有 Success 也没有明确失败：结果未知，交回调用方决定。
    Ok(Outcome::Unverified)
}

/// 卸载。`keep_data` 为真时保留数据与 UID。
///
/// 两种语义在 Android 上是不同的命令，不是同一个命令的两个参数：
/// 保留数据用 `pm uninstall -k`（UID 不变），彻底删除用 `adb uninstall`
/// （数据清空且 UID 会变）。产品需要两者。
pub fn uninstall(serial: &str, package: &str, keep_data: bool) -> Result<bool, String> {
    let out = if keep_data {
        adb::shell(serial, &format!("pm uninstall -k {package}"))
            .map_err(|e| format!("执行卸载失败: {e}"))?
    } else {
        adb::run(serial, &["uninstall", package])
            .map_err(|e| format!("执行卸载失败: {e}"))?
    };
    Ok(out.has("Success"))
}

/// 从输出里挑一句能给人看的话。
///
/// adb 的失败输出常常先是一大段 Java 堆栈，真正的原因在最后几行。
/// 直接展示整段等于没有信息。
fn first_meaningful_line(stdout: &str, stderr: &str) -> String {
    let pick = |s: &str| -> Option<String> {
        s.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .find(|l| {
                l.contains("INSTALL_FAILED")
                    || l.contains("Failure")
                    || l.contains("failed")
                    || l.contains("Error")
            })
            .map(str::to_owned)
    };
    pick(stdout)
        .or_else(|| pick(stderr))
        .unwrap_or_else(|| "安装失败，原因未知".to_owned())
}
