//! APK 安装与卸载。设备的包管理器负责确认最终状态，包括没有 LAUNCHER 的包。

use std::path::Path;
use std::process::Command;

use contract::ErrorCode;

use crate::catalog::{checked_shell, require_package, valid_package};

/// 只有包名和版本号均回读匹配，才返回 Installed。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed,
    /// 设备可能已经写入。调用方应刷新清单并报告具体的未确认原因。
    Unverified {
        detail: String,
    },
    Failed {
        code: ErrorCode,
        detail: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct ApkIdentity {
    package: String,
    version: u64,
}

/// 从准备好的 SDK 读取 manifest。直接读 xmltree 可以同时取得 versionCodeMajor，
/// 比较口径与 Android 的 [getLongVersionCode] 一致。
///
/// [getLongVersionCode]: https://developer.android.com/reference/android/content/pm/PackageInfo#getLongVersionCode()
fn apk_identity(aapt: &Path, apk: &Path) -> Result<ApkIdentity, Outcome> {
    let mut cmd = Command::new(aapt);
    cmd.args(["dump", "xmltree"])
        .arg(apk)
        .arg("AndroidManifest.xml");
    let out =
        adb::output_with_timeout(cmd, std::time::Duration::from_secs(15)).map_err(|error| {
            Outcome::Failed {
                code: if error.kind() == std::io::ErrorKind::NotFound {
                    ErrorCode::ComponentMissing
                } else {
                    ErrorCode::Io
                },
                detail: format!("无法执行运行时的 aapt（{}）：{error}", aapt.display()),
            }
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let reason = stderr
            .lines()
            .chain(stdout.lines())
            .find(|line| !line.trim().is_empty());
        return Err(Outcome::Failed {
            code: ErrorCode::InvalidApk,
            detail: format!("读取 APK 清单失败：{}", reason.unwrap_or("aapt 未返回诊断")),
        });
    }
    parse_manifest(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| Outcome::Failed {
        code: ErrorCode::InvalidApk,
        detail: "APK 清单缺少可确认的包名或版本号".into(),
    })
}

fn parse_manifest(xmltree: &str) -> Option<ApkIdentity> {
    let mut lines = xmltree.lines().map(str::trim);
    let root = lines.find(|line| line.starts_with("E: "))?;
    if root.split_whitespace().nth(1)? != "manifest" {
        return None;
    }
    let (mut package, mut minor, mut major) = (None, None, None);
    for line in lines.take_while(|line| !line.starts_with("E: ")) {
        let Some((name, value)) = line.strip_prefix("A: ").and_then(|l| l.split_once('=')) else {
            continue;
        };
        if name == "package" {
            let (pkg, _) = value.strip_prefix('"')?.split_once('"')?;
            if !valid_package(pkg) || package.replace(pkg.to_owned()).is_some() {
                return None;
            }
        } else if name.ends_with(":versionCode(0x0101021b)") {
            if minor.replace(version_part(value)?).is_some() {
                return None;
            }
        } else if name.ends_with(":versionCodeMajor(0x01010576)") {
            if major.is_some() {
                return None;
            }
            major = Some(version_part(value)?);
        }
    }
    Some(ApkIdentity {
        package: package?,
        version: (u64::from(major.unwrap_or(0)) << 32) | u64::from(minor?),
    })
}

fn version_part(value: &str) -> Option<u32> {
    // aapt 将整数打印为 (type 0x10)0x...；0x11 是十六进制整型资源。
    let value = value.trim();
    let value = value
        .strip_prefix("(type 0x10)")
        .or_else(|| value.strip_prefix("(type 0x11)"))
        .unwrap_or(value);
    if let Some(hex) = value.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).ok()
    } else {
        value.parse().ok()
    }
}

/// `aapt` 由实例的 RuntimePaths 提供。读清单失败时尚未向设备提交安装。
pub fn install(serial: &str, apk_path: &str, aapt: &Path) -> Result<Outcome, String> {
    // 绝对路径既支持空格，也保证以 '-' 开头的本地文件名不会被 adb/aapt 当成选项。
    let apk = match Path::new(apk_path).canonicalize() {
        Ok(path) if path.is_file() => path,
        Ok(_) => {
            return Ok(Outcome::Failed {
                code: ErrorCode::InvalidApk,
                detail: "请选择 APK 文件".into(),
            });
        }
        Err(e) => {
            return Ok(Outcome::Failed {
                code: ErrorCode::Io,
                detail: format!("无法打开 APK：{e}"),
            });
        }
    };
    let identity = match apk_identity(aapt, &apk) {
        Ok(identity) => identity,
        Err(failure) => return Ok(failure),
    };
    let path = apk.to_str().ok_or("APK 的实际路径不能转换为 UTF-8")?;
    let out = match adb::run(
        serial,
        &["install", "--no-incremental", "-r", "--user", "0", path],
    ) {
        Ok(out) => out,
        Err(e) => {
            return Ok(Outcome::Unverified {
                detail: format!("安装 {} 时通信中断：{e}", identity.package),
            });
        }
    };
    if let Some(failure) = install_failure(&out) {
        return Ok(failure);
    }
    let acknowledged = out
        .stdout
        .lines()
        .chain(out.stderr.lines())
        .any(|l| l.trim() == "Success");
    let detail = match installed_version(serial, &identity.package) {
        Ok(Some(actual)) if actual == identity.version && acknowledged => {
            return Ok(Outcome::Installed);
        }
        Ok(Some(actual)) if actual == identity.version => format!(
            "设备存在 {} 版本 {actual}，本次安装的回执缺失",
            identity.package
        ),
        Ok(Some(actual)) => format!(
            "{} 期望版本 {}，设备返回版本 {actual}",
            identity.package, identity.version
        ),
        Ok(None) => format!(
            "安装后在设备上未查到 {}（期望版本 {}）",
            identity.package, identity.version
        ),
        Err(e) => format!("安装后无法确认 {} 的版本：{e}", identity.package),
    };
    Ok(Outcome::Unverified { detail })
}

fn install_failure(out: &adb::AdbOutput) -> Option<Outcome> {
    let lines = || {
        out.stdout
            .lines()
            .chain(out.stderr.lines())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    // 先在两路输出中寻找具体分类，例如 stdout 只有 Failure，stderr 才有签名或存储诊断。
    for line in lines() {
        let code = if line.contains("INSTALL_FAILED_INSUFFICIENT_STORAGE")
            || line.contains("No space left on device")
        {
            ErrorCode::OutOfSpace
        } else if line.contains("INSTALL_PARSE_FAILED_") {
            ErrorCode::InvalidApk
        } else {
            ErrorCode::from_adb_stderr(line)
        };
        if code != ErrorCode::Other && code != ErrorCode::AdbFailure {
            return Some(Outcome::Failed {
                code,
                detail: line.into(),
            });
        }
    }
    let failure =
        lines().find(|line| line.contains("INSTALL_FAILED_") || crate::catalog::error_line(line));
    if out.code != Some(0) || failure.is_some() {
        let detail = failure
            .or_else(|| out.stderr.lines().find(|line| !line.trim().is_empty()))
            .or_else(|| lines().next_back());
        return Some(Outcome::Failed {
            code: ErrorCode::AdbFailure,
            detail: detail
                .map(str::to_owned)
                .unwrap_or_else(|| format!("adb install 异常退出（{:?}）", out.code)),
        });
    }
    None
}

/// 查询用户 0 的已安装包（含无 LAUNCHER 的应用，排除卸载后仅保留数据的记录）。
/// 整表应含基础框架包 android，从而可以区分包确实不存在和包服务返回空输出。
fn installed_version(serial: &str, package: &str) -> Result<Option<u64>, String> {
    let out = checked_shell(serial, "pm list packages --show-versioncode --user 0")?;
    let mut framework = false;
    let mut version = None;
    for line in out.lines() {
        let mut fields = line.split_whitespace();
        let name = fields
            .next()
            .and_then(|field| field.strip_prefix("package:"))
            .filter(|pkg| valid_package(pkg))
            .ok_or("包管理器返回了无效的包记录")?;
        let code = fields
            .next()
            .and_then(|v| v.strip_prefix("versionCode:"))
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|_| fields.next().is_none())
            .ok_or_else(|| format!("无法读取 {name} 的已安装版本"))?;
        framework |= name == "android";
        if name == package && version.replace(code).is_some() {
            return Err(format!("包管理器返回了多个 {name} 版本"));
        }
    }
    if !framework {
        return Err("未取得完整的用户 0 包索引".into());
    }
    Ok(version)
}

/// 保留数据时使用 pm uninstall -k，彻底删除时使用 adb uninstall。两者均回读包索引。
pub fn uninstall(serial: &str, package: &str, keep_data: bool) -> Result<bool, String> {
    require_package(package)?;
    let out = if keep_data {
        adb::shell(serial, &format!("pm uninstall -k {package}"))
    } else {
        adb::run(serial, &["uninstall", package])
    }
    .map_err(|e| format!("执行卸载失败：{e}"))?;
    if out.has("DELETE_FAILED_INTERNAL_ERROR") {
        if installed_version(serial, package)?.is_none() {
            return Ok(false);
        }
        return Err(format!(
            "设备拒绝卸载 {package}：{}",
            crate::catalog::diagnostic(&out)
        ));
    }
    if out.code != Some(0)
        || !out.lines().any(|l| l == "Success")
        || out
            .stdout
            .lines()
            .chain(out.stderr.lines())
            .any(crate::catalog::error_line)
    {
        return Err(format!(
            "无法确认卸载 {package}：{}",
            crate::catalog::diagnostic(&out)
        ));
    }
    if installed_version(serial, package)?.is_some() {
        return Err(format!("卸载后仍然查到 {package}"));
    }
    Ok(true)
}
