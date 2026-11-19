use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use windroid_locations::StorageLocations;

use super::catalog::{Artifact, BUILD_TOOLS, EMULATOR_VERSION, IMAGE_DIR, Kind};
use super::integrity::{check_digest, invalid};

/// 各设备域使用同一组路径。私有模式完全忽略系统 SDK、PATH 和 Homebrew。
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub runtime_root: PathBuf,
    /// 镜像根目录，其下按 API、标签和 ABI 区分安装。
    pub images: PathBuf,
    pub sdk: PathBuf,
    pub emulator: PathBuf,
    pub adb: PathBuf,
    pub aapt: PathBuf,
    pub image: PathBuf,
    pub server_jar: PathBuf,
    pub borrowed: bool,
    adb_override: bool,
    jar_override: bool,
    config_error: Option<String>,
}

impl RuntimePaths {
    pub fn discover(home: &Path) -> Self {
        Self::resolve(home, |key| std::env::var_os(key))
    }

    pub(super) fn resolve(home: &Path, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Self {
        let explicit = |key| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);
        let sdk = explicit("WINDROID_SDK");
        let jar = explicit("WINDROID_SERVER_JAR");
        let adb = explicit("WINDROID_ADB");
        let mut paths = match sdk {
            None => Self::managed(home),
            Some(sdk) => {
                let default_jar = || {
                    ["/opt/homebrew", "/usr/local"]
                        .into_iter()
                        .map(|base| {
                            Path::new(base).join("Cellar/scrcpy/5.0.1/share/scrcpy/scrcpy-server")
                        })
                        .find(|path| path.is_file())
                        .unwrap_or_else(|| Self::managed(home).server_jar)
                };
                Self::from_sdk(sdk, jar.clone().unwrap_or_else(default_jar), true)
            }
        };
        if let Some(jar) = jar {
            paths.server_jar = jar;
            paths.jar_override = true;
        }
        if let Some(adb) = adb {
            // 显式给出命令名时按 Command::new 的 PATH 搜索顺序检查可执行文件。
            paths.adb = if adb.parent() == Some(Path::new("")) {
                env("PATH")
                    .and_then(|path| {
                        std::env::split_paths(&path)
                            .map(|d| d.join(&adb))
                            .find(|p| {
                                fs::metadata(p).is_ok_and(|m| {
                                    m.is_file() && m.permissions().mode() & 0o111 != 0
                                })
                            })
                    })
                    .unwrap_or(adb)
            } else {
                adb
            };
            paths.adb_override = true;
        }
        paths
    }

    pub(super) fn managed(home: &Path) -> Self {
        let config = StorageLocations::load(home);
        let issue = config.as_ref().err().map(ToString::to_string);
        let config = config.unwrap_or_else(|_| StorageLocations::defaults(home));
        let root = &config.runtime_root;
        let mut paths = Self::from_sdk(root.join("sdk"), root.join("scrcpy-server.jar"), false);
        paths.images = config.images();
        paths.image = paths
            .images
            .join(Path::new(IMAGE_DIR).strip_prefix("system-images").unwrap());
        paths.config_error = issue;
        paths
    }

    /// 迁移可能已由另一进程完成。持有资源锁后再确认一次，避免写回旧目录。
    pub(crate) fn check_configuration(&self, home: &Path) -> io::Result<()> {
        if self.borrowed {
            return Ok(());
        }
        if let Some(message) = &self.config_error {
            return Err(invalid(message));
        }
        let current = StorageLocations::load(home)?;
        if self.runtime_root != current.runtime_root || self.images != current.images() {
            return Err(invalid("存储位置已更改，请重新检查运行环境"));
        }
        Ok(())
    }

    pub(super) fn from_sdk(sdk: PathBuf, server_jar: PathBuf, borrowed: bool) -> Self {
        Self {
            runtime_root: sdk.parent().unwrap_or(&sdk).to_owned(),
            images: sdk.join("system-images"),
            emulator: sdk.join("emulator/emulator"),
            adb: sdk.join("platform-tools/adb"),
            aapt: sdk.join(BUILD_TOOLS).join("aapt"),
            image: sdk.join(IMAGE_DIR),
            sdk,
            server_jar,
            borrowed,
            adb_override: false,
            jar_override: false,
            config_error: None,
        }
    }

    pub(super) fn read_only(&self, kind: Kind) -> bool {
        self.borrowed
            || (kind == Kind::Adb && self.adb_override)
            || (kind == Kind::Server && self.jar_override)
    }

    pub(super) fn explicit_adb(&self, kind: Kind) -> bool {
        kind == Kind::Adb && self.adb_override
    }

    pub(super) fn installation_root(&self, pkg: Artifact) -> &Path {
        if pkg.kind == Kind::Image && !self.images.starts_with(&self.runtime_root) {
            &self.images
        } else {
            &self.runtime_root
        }
    }

    pub(super) fn destination(&self, kind: Kind) -> PathBuf {
        match kind {
            Kind::Emulator => self.sdk.join("emulator"),
            Kind::Adb if self.adb_override => self.adb.clone(),
            Kind::Adb => self.sdk.join("platform-tools"),
            Kind::Aapt => self.sdk.join(BUILD_TOOLS),
            Kind::Image => self.image.clone(),
            Kind::Server => self.server_jar.clone(),
        }
    }
}

pub(super) const RECEIPT: &str = ".windroid-component";

pub(super) fn receipt(pkg: Artifact) -> String {
    format!(
        "windroid-v1 {} {} {}\n",
        pkg.id,
        pkg.version,
        pkg.digest.hex()
    )
}

/// 每个目录逐层检查，拒绝私有 SDK 或 staging 内跳转到用户其他目录的链接。
/// 根目录由调用方指定；打开一个新 home 时只扫描，不创建任何目录。
pub(super) fn directory(home: &Path, path: &Path, create: bool) -> io::Result<()> {
    let suffix = path
        .strip_prefix(home)
        .map_err(|_| invalid("目录在 Windroid 根目录之外"))?;
    if home.as_os_str().is_empty() {
        return Err(invalid("Windroid 根目录为空"));
    }
    if create {
        fs::create_dir_all(home)?;
    }
    if !fs::symlink_metadata(home)?.is_dir() {
        return Err(invalid(format!("根目录必须为普通目录：{}", home.display())));
    }
    let mut dir = home.to_path_buf();
    for part in suffix.components() {
        let std::path::Component::Normal(name) = part else {
            return Err(invalid("运行时目录无效"));
        };
        dir.push(name);
        if create {
            match fs::create_dir(&dir) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                r => r?,
            }
        }
        if !fs::symlink_metadata(&dir)?.is_dir() {
            return Err(invalid(format!(
                "目录不能是符号链接或文件：{}",
                dir.display()
            )));
        }
    }
    Ok(())
}

pub(super) fn regular(path: &Path) -> io::Result<fs::Metadata> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() {
        return Err(invalid(format!("需要普通文件：{}", path.display())));
    }
    Ok(m)
}

fn small_text(path: &Path) -> io::Result<String> {
    regular(path)?;
    let mut s = String::new();
    File::open(path)?.take(16_385).read_to_string(&mut s)?;
    if s.len() > 16_384 {
        return Err(invalid("组件元数据过大"));
    }
    Ok(s)
}

pub(super) fn installed(home: &Path, paths: &RuntimePaths, pkg: Artifact) -> io::Result<()> {
    paths.check_configuration(home)?;
    let dest = paths.destination(pkg.kind);
    if paths.read_only(pkg.kind) {
        // 只读检查显式选中的链接目标。单独指定 adb 时可以使用开发者自己的可执行文件。
        let selected = dest.canonicalize()?;
        if paths.explicit_adb(pkg.kind) {
            let meta = regular(&selected)?;
            if meta.len() == 0 || meta.permissions().mode() & 0o111 == 0 {
                return Err(invalid(format!(
                    "指定的 adb 为空或不可执行：{}",
                    dest.display()
                )));
            }
            return Ok(());
        }
        return validate_payload(&selected, pkg);
    }
    let parent = if pkg.prefix.is_some() {
        dest.as_path()
    } else {
        dest.parent().unwrap()
    };
    directory(paths.installation_root(pkg), parent, false)?;
    if pkg.prefix.is_some() && small_text(&dest.join(RECEIPT))? != receipt(pkg) {
        return Err(invalid("组件安装记录与发布版本不符"));
    }
    validate_payload(&dest, pkg)
}

fn revision(value: &str) -> Option<[u32; 3]> {
    let mut parts = value.split('.');
    let mut result = [0; 3];
    for field in &mut result {
        if let Some(part) = parts.next() {
            *field = part.parse().ok()?;
        }
    }
    parts.next().is_none().then_some(result)
}

/// 受信任包在发布前以及后续每次重扫都要满足同一份布局/版本契约。
/// 启动自检只读取小元数据与 734 KB 的协议绑定 JAR，不扫描多 GB 的镜像内容。
pub(super) fn validate_payload(dest: &Path, pkg: Artifact) -> io::Result<()> {
    if pkg.kind == Kind::Server {
        return check_digest(dest, pkg, || Ok(()));
    }
    let props = small_text(&dest.join("source.properties"))?;
    let property = |key: &str| {
        props
            .lines()
            .filter_map(|s| s.trim().split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.trim())
    };
    if property("Pkg.Revision") != Some(pkg.version) {
        return Err(invalid(format!(
            "{} 需要版本 {}，实际为 {}",
            pkg.name,
            pkg.version,
            property("Pkg.Revision").unwrap_or("未知")
        )));
    }
    if pkg.kind == Kind::Image
        && (property("AndroidVersion.ApiLevel") != Some("34")
            || property("SystemImage.TagId") != Some("google_apis_playstore")
            || property("SystemImage.Abi") != Some("arm64-v8a"))
    {
        return Err(invalid("系统镜像的 API、标签或 ABI 不匹配"));
    }
    if pkg.kind == Kind::Image {
        // 当前镜像只声明一项最低依赖，例如 emulator#34.2.16。比较数字而非词典序。
        let required = property("Pkg.Dependencies")
            .and_then(|v| v.strip_prefix("emulator#"))
            .and_then(revision);
        if !required
            .is_some_and(|v| v <= revision(EMULATOR_VERSION).expect("pinned emulator revision"))
        {
            return Err(invalid("镜像声明的最低模拟器版本不兼容"));
        }
    }
    let executable = pkg.kind != Kind::Image;
    for &(name, bytes) in pkg.essentials {
        let path = dest.join(name);
        directory(dest, path.parent().expect("relative component file"), false)?;
        let meta = regular(&path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}：{e}", path.display())))?;
        if meta.len() != bytes || (executable && meta.permissions().mode() & 0o111 == 0) {
            return Err(invalid(format!(
                "文件长度不符或不可执行：{}",
                path.display()
            )));
        }
    }
    Ok(())
}
