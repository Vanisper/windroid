//! 运行时自检：用户打开软件时，运行所需的东西是否齐备。
//!
//! 用户不需要知道「SDK」「AVD」「镜像」这些词。这里只回答一个问题：
//! **现在能不能创建并启动一个实例。** 不能的话，缺什么、在哪、怎么补。
//!
//! 自检不修改任何东西。它读事实、给结论，修复动作由调用方发起。

use std::path::{Path, PathBuf};

/// 一项运行时组件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// 用户能看懂的名字。
    pub name: &'static str,
    /// 是否可用。
    pub ok: bool,
    /// 实际找到的位置（或未找到时为空）。
    pub path: Option<PathBuf>,
    /// 缺失时的说明：缺少它会怎样、怎么补。
    pub remedy: &'static str,
}

/// 自检结果。
#[derive(Debug, Clone)]
pub struct Readiness {
    pub components: Vec<Component>,
    /// 可用的 AVD 名列表。空表示没有可启动的实例模板。
    pub avds: Vec<String>,
}

impl Readiness {
    /// 是否可以创建并启动实例。
    pub fn usable(&self) -> bool {
        self.components.iter().all(|c| c.ok) && !self.avds.is_empty()
    }

    /// 缺失的项。
    pub fn missing(&self) -> impl Iterator<Item = &Component> {
        self.components.iter().filter(|c| !c.ok)
    }
}

/// 执行自检。
///
/// 查的是路径与可执行性，不启动任何进程——自检必须瞬时完成，用户打开软件
/// 时不该等。
pub fn check() -> Readiness {
    let sdk = sdk_root();
    let components = vec![
        cmd_component(
            "Android 模拟器",
            sdk.as_ref().map(|s| s.join("emulator/emulator")),
            "无法创建实例。需要安装 Android 模拟器组件。",
        ),
        cmd_component(
            "adb",
            sdk.as_ref().map(|s| s.join("platform-tools/adb")),
            "无法与实例通信。需要安装 Android platform-tools。",
        ),
        lib_component(
            "scrcpy-server",
            scrcpy_server(),
            "无法显示应用画面。需要随应用分发的 scrcpy-server。",
        ),
    ];

    let avds = list_avds(sdk.as_deref());
    Readiness { components, avds }
}

/// SDK 根目录。
///
/// 顺序：环境变量 → 标准安装位置。发布版会改为应用自带目录。
fn sdk_root() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WINDROID_SDK") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home).join("Library/Android/sdk");
    p.is_dir().then_some(p)
}

/// scrcpy-server 的位置。
fn scrcpy_server() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WINDROID_SERVER_JAR") {
        return Some(PathBuf::from(p));
    }
    // 随应用分发的优先。开发期退回系统安装的 scrcpy。
    let candidates = [
        "/opt/homebrew/Cellar/scrcpy/5.0.1/share/scrcpy/scrcpy-server",
        "/usr/local/Cellar/scrcpy/5.0.1/share/scrcpy/scrcpy-server",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

fn cmd_component(name: &'static str, path: Option<PathBuf>, remedy: &'static str) -> Component {
    let ok = path.as_ref().is_some_and(|p| is_executable(p));
    Component { name, ok, path: ok.then_some(path).flatten(), remedy }
}

fn lib_component(name: &'static str, path: Option<PathBuf>, remedy: &'static str) -> Component {
    let ok = path.as_ref().is_some_and(|p| p.is_file());
    Component { name, ok, path, remedy }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// 可启动的 AVD 列表。
///
/// 读 `~/.android/avd/*.ini`：每个 `.ini` 对应一个 AVD，文件名即 AVD 名。
/// 不调用 `emulator -list-avds`：那要启动一个进程，而自检必须瞬时完成。
fn list_avds(sdk: Option<&Path>) -> Vec<String> {
    let _ = sdk;
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dir = PathBuf::from(home).join(".android/avd");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "ini"))
        .filter_map(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}
