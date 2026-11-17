//! 应用展示名。
//!
//! 包名对用户没有意义，必须换成应用自己声明的名字（`application-label`）。
//! 名字在 APK 的资源里，只能拉下 APK 用 aapt2 读。
//!
//! 每个应用要拉一次 APK（可达数十 MB），因此必须缓存：按
//! `包名-版本号` 建键，应用更新后自动失效。缓存放在实例目录之外，
//! 因为同一份 APK 跨实例相同。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 标签缓存。
pub struct Labels {
    dir: PathBuf,
    /// 内存缓存，避免每次读盘。
    memory: BTreeMap<String, String>,
}

impl Labels {
    pub fn new(cache_root: &Path) -> Self {
        let dir = cache_root.join("labels");
        let _ = std::fs::create_dir_all(&dir);
        Self { dir, memory: BTreeMap::new() }
    }

    /// 取一批应用的展示名。已有的走缓存，缺的补齐。
    ///
    /// 返回 `包名 → 名字`。取不到的不出现在结果里——调用方用包名兜底，
    /// 而不是显示一个空白项。
    pub fn resolve(&mut self, serial: &str, packages: &[String]) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for pkg in packages {
            if let Some(name) = self.memory.get(pkg) {
                out.insert(pkg.clone(), name.clone());
                continue;
            }
            if let Some((version, name)) = self.read_cache(pkg) {
                self.memory.insert(pkg.clone(), name.clone());
                let _ = version;
                out.insert(pkg.clone(), name);
                continue;
            }
            if let Some((version, name)) = self.fetch(serial, pkg) {
                self.store(pkg, &version, &name);
                self.memory.insert(pkg.clone(), name.clone());
                out.insert(pkg.clone(), name);
            }
        }
        out
    }

    fn cache_key(&self, pkg: &str) -> PathBuf {
        self.dir.join(format!("{pkg}.txt"))
    }

    fn read_cache(&self, pkg: &str) -> Option<(String, String)> {
        let text = std::fs::read_to_string(self.cache_key(pkg)).ok()?;
        let mut lines = text.lines();
        let version = lines.next()?.to_owned();
        let name = lines.next()?.to_owned();
        (!name.is_empty()).then_some((version, name))
    }

    fn store(&self, pkg: &str, version: &str, name: &str) {
        let _ = std::fs::write(self.cache_key(pkg), format!("{version}\n{name}"));
    }

    /// 拉 APK 并读标签。
    fn fetch(&self, serial: &str, pkg: &str) -> Option<(String, String)> {
        let version = version_of(serial, pkg)?;
        // 版本号已变过说明有缓存也是旧的，先删。
        let _ = std::fs::remove_file(self.cache_key(pkg));

        let path = adb::shell(serial, &format!("pm path {pkg}")).ok()?;
        let apk = path
            .lines()
            .find_map(|l| l.trim().strip_prefix("package:"))?
            .to_owned();

        let tmp = std::env::temp_dir().join(format!("windroid-label-{}.apk", std::process::id()));
        let pulled = adb::run(serial, &["pull", &apk, tmp.to_str()?]).ok()?;
        if !pulled.has("1 file pulled") {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }

        let name = aapt_label(&tmp);
        let _ = std::fs::remove_file(&tmp);
        let name = name?;
        Some((version, name))
    }
}

/// 版本号。用于缓存失效。
fn version_of(serial: &str, pkg: &str) -> Option<String> {
    let out = adb::shell(serial, &format!("dumpsys package {pkg}")).ok()?;
    out.lines()
        .find_map(|l| l.trim().strip_prefix("versionName="))
        .map(|v| v.trim().to_owned())
}

/// 用 aapt2 读 `application-label`。
///
/// 优先取中文标签：应用名是给中文用户看的，很多应用有单独的 zh-CN 名字。
fn aapt_label(apk: &Path) -> Option<String> {
    let aapt = aapt2()?;
    let out = Command::new(aapt).arg("dump").arg("badging").arg(apk).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);

    let pick = |key: &str| -> Option<String> {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split(':').nth(1))
            .map(|v| v.trim().trim_matches('\'').to_owned())
            .filter(|v| !v.is_empty())
    };
    pick("application-label-zh-CN")
        .or_else(|| pick("application-label-zh"))
        .or_else(|| pick("application-label"))
}

/// aapt2 的位置。发布版随运行时组件分发；开发期用系统 SDK。
fn aapt2() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WINDROID_AAPT2") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    let root = PathBuf::from(home).join("Library/Android/sdk/build-tools");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    // 取最高版本。目录名是版本号，按字符串排序在个位数上够用；
    // 将来跨到两位数时再改成解析版本号。
    versions.sort();
    versions
        .into_iter()
        .rev()
        .map(|d| d.join("aapt2"))
        .find(|p| p.is_file())
}
