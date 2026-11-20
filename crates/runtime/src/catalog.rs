//! 应用清单：从设备读出有哪些应用、各自能否启动、能否作桌面。
//!
//! 这一层只做查询与解析，不缓存、不排序、不分组——分组是用户的决定，
//! 属于呈现域。
//!
//! 三个判定的来源必须分开，它们互不推导：
//! - **系统/用户**：`pm list packages -s/-3`。这是 Android 自己的分类，
//!   它已经处理了「系统包被更新后搬到 /data/app」的情况。看 APK 路径会错。
//! - **能否启动**：LAUNCHER intent。
//! - **能否作桌面**：HOME intent。用户装的第三方桌面与系统桌面在这里没有区别。

use contract::{AppEntry, PackageName};

/// 桌面相关的设备状态。
///
/// 与 [AppEntry] 分开：这些是「这台设备上哪些应用能当桌面、当前是谁」，
/// 属于一类能力查询，不是应用的固有属性。将来增加别类能力（通知、后台限制）
/// 时，只需新增同样形状的查询，应用结构不动。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomeState {
    /// 声明了 HOME intent 的包名。
    pub candidates: std::collections::BTreeSet<String>,
    /// 当前默认桌面。
    pub default: Option<String>,
}

impl HomeState {
    pub fn is_candidate(&self, package: &str) -> bool {
        self.candidates.contains(package)
    }

    pub fn is_default(&self, package: &str) -> bool {
        self.default.as_deref() == Some(package)
    }
}

/// 读一次应用清单。
pub fn snapshot(device: adb::Device<'_>) -> Result<Vec<AppEntry>, String> {
    let user = packages(device, "-3")?;
    let system = packages(device, "-s")?;
    let launcher = component_packages(device, LAUNCHER)?;

    let mut out = Vec::new();
    for (pkgs, is_system) in [(user, false), (system, true)] {
        for pkg in pkgs {
            // 只有能启动的应用进清单。不能启动的（纯库、overlay、只作桌面的
            // 组件）在应用库里没有可做的事——它们需要的是别的入口，不是网格。
            if !launcher.contains(&pkg) {
                continue;
            }
            out.push(AppEntry {
                system: is_system,
                launcher: true,
                label: None,
                icon_key: None,
                package: PackageName::new(&pkg),
            });
        }
    }
    // 包名去重：同一应用可能因两次查询都命中而出现两次。
    out.sort_by(|a, b| a.package.as_str().cmp(b.package.as_str()));
    out.dedup_by(|a, b| a.package == b.package);
    Ok(out)
}

/// 读桌面候选与默认桌面。系统选择器是占位解析结果，只有候选可以成为默认桌面。
pub fn home_state(device: adb::Device<'_>) -> Result<HomeState, String> {
    let candidates = component_packages(device, HOME)?;
    let default = default_home_package(device)?.filter(|package| candidates.contains(package));
    Ok(HomeState {
        candidates,
        default,
    })
}

const HOME: &str = "android.intent.category.HOME";
const LAUNCHER: &str = "android.intent.category.LAUNCHER";

/// Android 包名会进入设备端 shell。各段以 ASCII 字母开头，后接字母、数字、下划线。
/// 单段系统包（例如 android）同样允许。
pub fn valid_package(package: &str) -> bool {
    !package.is_empty()
        && package.split('.').all(|part| {
            let mut chars = part.bytes();
            chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
}

pub(crate) fn require_package(package: &str) -> Result<(), String> {
    if valid_package(package) {
        Ok(())
    } else {
        Err("Android 包名无效".into())
    }
}

/// adb 或 Android shell 可能在输出错误时仍退出 0。错误不能变成空目录或任务消失。
pub(crate) fn checked_shell(device: adb::Device<'_>, cmd: &str) -> Result<adb::AdbOutput, String> {
    check_output(adb::shell(device, cmd))
}

fn check_output(output: std::io::Result<adb::AdbOutput>) -> Result<adb::AdbOutput, String> {
    let out = output.map_err(|e| format!("设备命令未能完成：{e}"))?;
    if out.code != Some(0) || out.stdout.lines().chain(out.stderr.lines()).any(error_line) {
        return Err(diagnostic(&out));
    }
    Ok(out)
}

pub(crate) fn error_line(line: &str) -> bool {
    let line = line.trim();
    if line.starts_with("*** SERVICE") && line.contains("DUMP TIMEOUT") {
        return true;
    }
    [
        "Error:",
        "Error type ",
        "error:",
        "Failure",
        "Failed",
        "failed",
        "adb: failed",
        "adb: error:",
        "Exception occurred",
        "Security exception",
        "java.lang.",
        "Can't find service:",
        "cmd: Can't find service:",
        "Unknown command",
        "Permission Denial:",
        "DUMP TIMEOUT",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

pub(crate) fn diagnostic(out: &adb::AdbOutput) -> String {
    out.stderr
        .lines()
        .chain(out.stdout.lines())
        .find(|l| error_line(l))
        .or_else(|| {
            out.stderr
                .lines()
                .chain(out.stdout.lines())
                .find(|l| !l.trim().is_empty())
        })
        .map(|l| l.trim().to_owned())
        .unwrap_or_else(|| format!("adb 未返回诊断（退出码 {:?}）", out.code))
}

fn packages(device: adb::Device<'_>, kind: &str) -> Result<Vec<String>, String> {
    let out = checked_shell(device, &format!("pm list packages {kind} --user 0"))?;
    let mut packages = Vec::new();
    for line in out.lines() {
        let pkg = line
            .strip_prefix("package:")
            .filter(|p| valid_package(p))
            .ok_or("无法解析包索引")?;
        packages.push(pkg.to_owned());
    }
    if kind == "-s" && !packages.iter().any(|pkg| pkg == "android") {
        return Err("未取得系统包索引".into());
    }
    Ok(packages)
}

fn component_package(component: &str) -> Option<&str> {
    let (package, class) = component.split_once('/')?;
    (valid_package(package)
        && !class.is_empty()
        && class
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '$')))
    .then_some(package)
}

/// --brief 的组件行独占一行，旁边是优先级、计数等元数据。
fn components(device: adb::Device<'_>, category: &str) -> Result<Vec<String>, String> {
    let cmd = format!(
        "cmd package query-activities --brief --user 0 -a android.intent.action.MAIN -c {category}"
    );
    let out = checked_shell(device, &cmd)?;
    let result: Vec<_> = out
        .lines()
        .filter(|l| component_package(l).is_some())
        .map(str::to_owned)
        .collect();
    if result.is_empty()
        && !out
            .lines()
            .any(|l| l == "No activities found" || l == "0 activities found:")
    {
        return Err(format!("无法读取 {category} 候选组件"));
    }
    Ok(result)
}

fn component_packages(
    device: adb::Device<'_>,
    category: &str,
) -> Result<std::collections::BTreeSet<String>, String> {
    Ok(components(device, category)?
        .iter()
        .filter_map(|l| component_package(l).map(str::to_owned))
        .collect())
}

fn resolve(
    device: adb::Device<'_>,
    category: &str,
    package: Option<&str>,
) -> Result<Option<String>, String> {
    let mut cmd = format!(
        "cmd package resolve-activity --brief --user 0 -a android.intent.action.MAIN -c {category}"
    );
    if let Some(package) = package {
        require_package(package)?;
        cmd.push_str(&format!(" -p {package}"));
    }
    let out = checked_shell(device, &cmd)?;
    if let Some(component) = out.lines().find(|line| component_package(line).is_some()) {
        return Ok(Some(component.to_owned()));
    }
    if out.lines().any(|l| l == "No activity found") {
        Ok(None)
    } else {
        Err(format!("无法解析 {category} 的默认组件"))
    }
}

/// 返回当前 HOME 解析到的包。home_state 会再按候选集合剔除系统选择器。
pub fn default_home_package(device: adb::Device<'_>) -> Result<Option<String>, String> {
    Ok(resolve(device, HOME, None)?
        .as_deref()
        .and_then(component_package)
        .map(str::to_owned))
}

/// 确认是 HOME 候选，并以同一用户再次解析 HOME，得到目标包后才完成。
pub fn set_default_home(device: adb::Device<'_>, package: &str) -> Result<bool, String> {
    require_package(package)?;
    let Some(component) = components(device, HOME)?
        .into_iter()
        .find(|line| component_package(line) == Some(package))
    else {
        return Ok(false);
    };
    // 单引号保护 class 名中合法的 '$'；component_package 已拒绝引号。
    checked_shell(
        device,
        &format!("cmd package set-home-activity --user 0 '{component}'"),
    )?;
    let actual = default_home_package(device)?;
    if actual.as_deref() != Some(package) {
        return Err(format!(
            "设备未确认默认桌面 {package}（当前为 {}）",
            actual.as_deref().unwrap_or("未设置")
        ));
    }
    Ok(true)
}

/// 在默认显示启动一个 LAUNCHER 组件。应用独立窗口由 scrcpy 的目标显示会话启动。
pub fn launch(device: adb::Device<'_>, package: &str) -> Result<bool, String> {
    require_package(package)?;
    let Some(component) = resolve(device, LAUNCHER, Some(package))? else {
        return Ok(false);
    };
    if component_package(&component) != Some(package) {
        return Ok(false);
    }
    let start = format!(
        "am start -W --user 0 --display 0 -a android.intent.action.MAIN -c {LAUNCHER} -n '{component}'"
    );
    // -W 等待首帧。冷启动可能超过普通 adb 探测的时间预算。
    check_output(adb::run_with_timeout(
        device,
        &["shell", &start],
        std::time::Duration::from_secs(20),
    ))?;
    let resumed = crate::running::snapshot(device)?
        .iter()
        .any(|t| t.package == package && t.display_id == 0 && t.resumed);
    if !resumed {
        return Err(format!("未在主显示确认 {package} 已启动"));
    }
    Ok(true)
}
