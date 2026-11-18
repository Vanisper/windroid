//! 按实例缓存应用展示名。每次刷新先向用户 0 的包索引核对完整 versionCode 和安装路径。
//! 读取失败时省略该项，应用库使用包名，下次刷新继续尝试。

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use contract::InstanceId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::catalog::{error_line, valid_package};
use crate::provision::RuntimePaths;

const INDEX: &str = "pm list packages -f --show-versioncode --user 0";
const REFRESH_BUDGET: Duration = Duration::from_secs(15);
const COMMAND_BUDGET: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InstalledApk {
    version: u64,
    path: String,
}

#[derive(Serialize, Deserialize)]
struct CachedLabel {
    apk: InstalledApk,
    name: String,
}

pub struct Labels {
    dir: PathBuf,
    paths: RuntimePaths,
}

impl Labels {
    pub fn new(home: &Path, instance: &InstanceId) -> Self {
        // 实例 ID 随清单持久化。模拟器端口在删除实例后可能分配给其他设备。
        let key = format!("{:x}", Sha256::digest(instance.as_str().as_bytes()));
        Self {
            dir: home.join("cache/labels").join(key),
            paths: RuntimePaths::discover(home),
        }
    }

    /// 返回成功核验的包名与展示名。单轮预算覆盖索引、APK 拉取及资源解析。
    pub fn resolve(&mut self, serial: &str, packages: &[String]) -> BTreeMap<String, String> {
        let deadline = Instant::now() + REFRESH_BUDGET;
        let mut names = BTreeMap::new();
        if packages.is_empty() {
            return names;
        }
        let Some(index) = self.index(serial, deadline) else {
            return names;
        };
        let mut missing = Vec::new();
        for package in packages {
            let Some(apk) = index.get(package) else {
                continue;
            };
            let saved: Option<CachedLabel> = fs::read(self.cache_file(package))
                .ok()
                .and_then(|buf| serde_json::from_slice(&buf).ok());
            if let Some(saved) = saved.filter(|saved| &saved.apk == apk && usable(&saved.name)) {
                names.insert(package.clone(), saved.name);
            } else {
                missing.push((package, apk));
            }
        }
        if missing.is_empty() || !self.paths.aapt.is_file() {
            return names;
        }
        let Ok(scratch) = Scratch::create(&self.dir) else {
            return names;
        };
        // 把末次索引查询的时间留出来，慢包耗尽预算时也能保留本轮已核验的结果。
        let fetch_until =
            deadline - (deadline.saturating_duration_since(Instant::now()) / 3).min(COMMAND_BUDGET);
        let mut fetched = Vec::new();
        for (package, apk) in missing {
            if Instant::now() >= fetch_until {
                break;
            }
            // 每个批次独占暂存目录，每次只保留一个 APK，数百 MB 的拉取也不累计磁盘占用。
            let local = scratch.0.join(format!("{package}.apk"));
            let name = self.fetch(serial, package, apk, &local, fetch_until);
            let _ = fs::remove_file(&local);
            if let Some(name) = name {
                fetched.push((package, apk, name));
            }
        }
        if fetched.is_empty() && names.is_empty() {
            return names;
        }
        // 拉取期间应用可能更新。只发布结束时仍对应同一版本、同一路径的名称。
        let Some(current) = self.index(serial, deadline) else {
            return BTreeMap::new();
        };
        names.retain(|package, _| current.get(package) == index.get(package));
        for (package, apk, name) in fetched {
            if current.get(package) != Some(apk) {
                continue;
            }
            let saved = CachedLabel {
                apk: apk.clone(),
                name,
            };
            let temp = scratch.0.join("label.json");
            if let Ok(buf) = serde_json::to_vec(&saved)
                && fs::write(&temp, buf).is_ok()
            {
                let _ = fs::rename(temp, self.cache_file(package));
            }
            names.insert(package.clone(), saved.name);
        }
        names
    }

    fn cache_file(&self, package: &str) -> PathBuf {
        // macOS 文件系统通常忽略大小写，摘要使大小写不同的 Android 包名仍有各自的缓存。
        self.dir
            .join(format!("{:x}.json", Sha256::digest(package.as_bytes())))
    }

    fn index(&self, serial: &str, deadline: Instant) -> Option<BTreeMap<String, InstalledApk>> {
        let mut cmd = Command::new(&self.paths.adb);
        cmd.args(["-s", serial, "shell", INDEX]);
        let text = stdout(cmd, deadline)?;
        let mut packages = BTreeMap::new();
        // -f 取 applicationInfo.sourceDir，--show-versioncode 取 longVersionCode。
        // 安装路径中的随机目录名可以带多个 '='，从右侧切分。
        // https://android.googlesource.com/platform/frameworks/base/+/android-14.0.0_r75/services/core/java/com/android/server/pm/PackageManagerShellCommand.java
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let (entry, version) = line
                .strip_prefix("package:")?
                .rsplit_once(" versionCode:")?;
            let (path, package) = entry.rsplit_once('=')?;
            if !valid_package(package) || !path.starts_with('/') || !path.ends_with(".apk") {
                continue;
            }
            let apk = InstalledApk {
                version: version.parse().ok()?,
                path: path.into(),
            };
            if packages.insert(package.to_string(), apk).is_some() {
                return None;
            }
        }
        Some(packages)
    }

    fn fetch(
        &self,
        serial: &str,
        package: &str,
        apk: &InstalledApk,
        local: &Path,
        deadline: Instant,
    ) -> Option<String> {
        let mut pull = Command::new(&self.paths.adb);
        pull.args(["-s", serial, "pull", &apk.path]).arg(local);
        stdout(pull, deadline)?;
        let metadata = fs::metadata(local).ok()?;
        if !metadata.is_file() || metadata.len() == 0 {
            return None;
        }

        let mut badging = Command::new(&self.paths.aapt);
        badging.args(["dump", "badging"]).arg(local);
        let text = stdout(badging, deadline)?;
        // aapt 的 badging 只报告低 32 位，完整版本在两次设备索引中核对。
        let expected = format!(
            "package: name='{package}' versionCode='{}'",
            apk.version as u32
        );
        if !text.lines().any(|line| line.starts_with(&expected)) {
            return None;
        }
        for key in [
            "application-label-zh-CN:",
            "application-label-zh-Hans:",
            "application-label-zh:",
            "application-label:",
        ] {
            if let Some(name) = text
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|s| s.trim().strip_prefix('\'')?.strip_suffix('\''))
                .map(str::trim)
                .filter(|s| usable(s))
            {
                return Some(name.to_owned());
            }
        }
        // 没有资源表的 APK 可能只有清单里的字面量标签，badging 不会列出它。
        let mut tree = Command::new(&self.paths.aapt);
        tree.args(["dump", "xmltree"])
            .arg(local)
            .arg("AndroidManifest.xml");
        literal_label(&stdout(tree, deadline)?)
    }
}

fn literal_label(tree: &str) -> Option<String> {
    let mut lines = tree.lines().map(str::trim);
    lines.find(|line| {
        line.strip_prefix("E: ")
            .and_then(|v| v.split_whitespace().next())
            == Some("application")
    })?;
    lines
        .take_while(|line| !line.starts_with("E: "))
        .find_map(|line| {
            let (key, value) = line.strip_prefix("A: ")?.split_once('=')?;
            if !key.ends_with(":label(0x01010001)") {
                return None;
            }
            let value = value.split_once(" (Raw: ").map_or(value, |(v, _)| v);
            let name = value.strip_prefix('"')?.strip_suffix('"')?.trim();
            usable(name).then(|| name.to_owned())
        })
}

fn usable(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 1024 && !name.chars().any(char::is_control)
}

fn stdout(cmd: Command, deadline: Instant) -> Option<String> {
    let budget = deadline
        .checked_duration_since(Instant::now())?
        .min(COMMAND_BUDGET);
    let out = adb::output_with_timeout(cmd, budget).ok()?;
    if !out.status.success() || String::from_utf8_lossy(&out.stderr).lines().any(error_line) {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    (!text.lines().any(error_line)).then_some(text)
}

/// create_dir 以独占方式取得目录。进程内序号与已有目录检查覆盖并发刷新和 PID 复用。
struct Scratch(PathBuf);
impl Scratch {
    fn create(parent: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        fs::create_dir_all(parent)?;
        loop {
            let path = parent.join(format!(
                ".pull-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stalled_label_command_yields_to_the_refresh_deadline() {
        let mut stalled = Command::new("/bin/sleep");
        stalled.arg("1");
        assert!(stdout(stalled, Instant::now() + Duration::from_millis(40)).is_none());
    }
}
