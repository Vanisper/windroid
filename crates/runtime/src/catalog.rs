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
pub fn snapshot(serial: &str) -> Result<Vec<AppEntry>, String> {
    let user = packages(serial, "-3")?;
    let system = packages(serial, "-s")?;
    let launcher = component_packages(serial, "android.intent.category.LAUNCHER")?;

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

/// 读桌面状态：哪些应用能当桌面、当前默认是谁。
pub fn home_state(serial: &str) -> Result<HomeState, String> {
    Ok(HomeState {
        candidates: component_packages(serial, "android.intent.category.HOME")?,
        default: default_home_package(serial)?,
    })
}

/// 列出包名。`kind` 为 `-3`（用户安装）或 `-s`（系统）。
fn packages(serial: &str, kind: &str) -> Result<Vec<String>, String> {
    let out = adb::shell(serial, &format!("pm list packages {kind}"))
        .map_err(|e| format!("查询应用清单失败: {e}"))?;
    Ok(out
        .lines()
        .filter_map(|l| l.strip_prefix("package:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}

/// 声明了某 category 的组件，取其包名集合。
///
/// `query-activities` 即使带 `--brief` 也会输出带缩进的多行块，组件行形如
/// `com.example/.MainActivity`。因此不能按首列取，要找出含斜杠的行。
fn component_packages(serial: &str, category: &str) -> Result<std::collections::BTreeSet<String>, String> {
    let cmd = format!(
        "cmd package query-activities --brief -a android.intent.action.MAIN -c {category}"
    );
    let out = adb::shell(serial, &cmd).map_err(|e| format!("查询 {category} 失败: {e}"))?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| l.contains('/'))
        .filter_map(|l| l.split('/').next())
        .filter(|p| !p.is_empty() && !p.contains(' '))
        .map(str::to_owned)
        .collect())
}

/// 当前默认桌面所属包名。
///
/// `resolve-activity` 的正文在最后一行，前面是优先级等元信息。
pub fn default_home_package(serial: &str) -> Result<Option<String>, String> {
    let cmd = "cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.HOME";
    let out = adb::shell(serial, cmd).map_err(|e| format!("查询默认桌面失败: {e}"))?;
    Ok(out
        .lines()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .find(|l| l.contains('/'))
        .and_then(|l| l.split('/').next())
        .map(|p| p.trim().to_owned()))
}

/// 把一个应用设为默认桌面。
///
/// 需要完整的组件名（`包名/Activity`），因此先查 HOME 候选取其组件。
/// 不能接受任意包名：Android 要求的是一个声明了 HOME 的组件。
pub fn set_default_home(serial: &str, package: &str) -> Result<bool, String> {
    let cmd = "cmd package query-activities --brief -a android.intent.action.MAIN -c android.intent.category.HOME";
    let out = adb::shell(serial, cmd).map_err(|e| format!("查询桌面候选失败: {e}"))?;
    let component = out
        .lines()
        .map(str::trim)
        .filter(|l| l.contains('/'))
        .find(|l| l.split('/').next() == Some(package));

    let Some(component) = component else {
        return Ok(false);
    };
    let out = adb::shell(
        serial,
        &format!("cmd package set-home-activity --user 0 {component}"),
    )
    .map_err(|e| format!("设置默认桌面失败: {e}"))?;
    // 同样只能看输出：这条命令失败时退出码也是 0。
    Ok(!out.has("Error") && !out.has("Exception"))
}

/// 启动一个应用。
///
/// 成功与否必须回读验证：`am start` 在目标不存在时也退出 0。
pub fn launch(serial: &str, package: &str) -> Result<bool, String> {
    let cmd = format!(
        "monkey -p {package} -c android.intent.category.LAUNCHER 1"
    );
    let out = adb::shell(serial, &cmd).map_err(|e| format!("启动失败: {e}"))?;
    // monkey 的失败特征：输出里含 "No activities found" 或 "aborted"。
    Ok(!out.has("No activities found") && !out.has("aborted"))
}
