use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use windroid_locations::StorageLocations;

use super::catalog::{Artifact, CATALOG, Kind};
use super::integrity::{check_digest, invalid};

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub runtime_root: PathBuf,
    pub images: PathBuf,
    pub emulator: PathBuf,
    pub adb: PathBuf,
    pub aapt: PathBuf,
    pub server_jar: PathBuf,
    pub(super) config_error: Option<String>,
}

pub(crate) struct ManagedPackage {
    pub(crate) path: PathBuf,
    pub(super) artifact: Artifact,
}

impl ManagedPackage {
    pub(crate) fn id(&self) -> &'static str {
        self.artifact.id
    }
    pub(crate) fn image(&self) -> Option<contract::Image> {
        self.artifact.image()
    }
    pub(crate) fn label(&self) -> String {
        format!(
            "{} {}{}",
            self.artifact.name,
            if self.artifact.image_spec.is_some() {
                "r"
            } else {
                ""
            },
            self.artifact.version
        )
    }
    pub(crate) fn unpacked_bytes(&self) -> u64 {
        self.artifact.unpacked
    }
    /// 调用方在整个使用期间持有版本对象的共享租约。
    pub(crate) fn check(&self, home: &Path, mode: super::Verification) -> io::Result<()> {
        super::manifest::check(home, &self.path, self.artifact, mode, || Ok(())).map(|_| ())
    }
    pub(crate) fn content_digest(&self) -> String {
        super::manifest::content_digest(self.artifact)
    }
    pub(crate) fn empty_skeleton(&self, path: &Path) -> io::Result<bool> {
        super::manifest::empty_skeleton(path, self.artifact)
    }
    pub(crate) fn download_space(&self, config: &StorageLocations) -> (u64, Option<u64>, bool) {
        let paths = RuntimePaths::for_locations(config);
        let dest = paths.destination(self.artifact);
        let volume = super::volume_info(dest.parent().expect("version directory")).ok();
        (
            super::required_bytes(self.artifact, super::prefix_bytes(&paths, self.artifact)),
            volume.map(|v| v.0),
            volume.is_some_and(|v| v.1),
        )
    }
    pub(crate) fn verify_at(&self, path: &Path) -> io::Result<String> {
        super::manifest::full(path, self.artifact, || Ok(())).map(|(digest, _)| digest)
    }
}

impl RuntimePaths {
    pub fn discover(home: &Path) -> Self {
        Self::managed(home)
    }

    pub(super) fn managed(home: &Path) -> Self {
        let config = StorageLocations::load(home);
        let issue = config.as_ref().err().map(ToString::to_string);
        let config = config.unwrap_or_else(|_| StorageLocations::defaults(home));
        let mut paths = Self::for_locations(&config);
        paths.config_error = issue;
        paths
    }

    pub(crate) fn for_locations(config: &StorageLocations) -> Self {
        let root = &config.runtime_root;
        let component = |kind| {
            let pkg = CATALOG
                .iter()
                .find(|p| p.kind == kind)
                .expect("pinned component");
            root.join(pkg.id).join(pkg.version)
        };
        Self {
            runtime_root: root.clone(),
            images: config.images(),
            emulator: component(Kind::Emulator).join("emulator"),
            adb: component(Kind::Adb).join("adb"),
            aapt: component(Kind::Aapt).join("aapt"),
            server_jar: component(Kind::Server).join("scrcpy-server.jar"),
            config_error: None,
        }
    }

    pub(crate) fn check_configuration(
        &self,
        home: &Path,
        components: bool,
        images: bool,
    ) -> io::Result<()> {
        if let Some(error) = &self.config_error {
            return Err(invalid(error));
        }
        let current = StorageLocations::load(home)?;
        if components && !crate::storage::same_location(&self.runtime_root, &current.runtime_root)
            || images && !crate::storage::same_location(&self.images, &current.images())
        {
            return Err(invalid("存储位置已更改，请重新检查运行环境"));
        }
        Ok(())
    }

    pub(super) fn installation_root(&self, pkg: Artifact) -> &Path {
        if pkg.image_spec.is_some() {
            &self.images
        } else {
            &self.runtime_root
        }
    }

    pub(crate) fn managed_packages(config: &StorageLocations) -> Vec<ManagedPackage> {
        let paths = Self::for_locations(config);
        CATALOG
            .iter()
            .map(|&artifact| ManagedPackage {
                path: paths.destination(artifact),
                artifact,
            })
            .collect()
    }

    pub(super) fn destination(&self, pkg: Artifact) -> PathBuf {
        if let Some(image) = pkg.image() {
            self.images
                .join(crate::instances::image_subdir(&image).expect("catalog image"))
        } else {
            self.runtime_root.join(pkg.id).join(pkg.version)
        }
    }
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
    if !fs::metadata(home)?.is_dir() {
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

pub(super) fn small_text(path: &Path) -> io::Result<String> {
    regular(path)?;
    let mut s = String::new();
    File::open(path)?.take(16_385).read_to_string(&mut s)?;
    if s.len() > 16_384 {
        return Err(invalid("组件元数据过大"));
    }
    Ok(s)
}

/// 受信任包在发布前以及后续每次重扫都要满足同一份布局/版本契约。
/// 启动自检只读取小元数据与 734 KB 的协议绑定 JAR，不扫描多 GB 的镜像内容。
pub(super) fn validate_payload(dest: &Path, pkg: Artifact) -> io::Result<()> {
    if pkg.kind == Kind::Server {
        return check_digest(&dest.join("scrcpy-server.jar"), pkg, || Ok(()));
    }
    let props = small_text(&dest.join("source.properties"))?;
    super::metadata::verify(&props, pkg)?;
    let executable = pkg.kind != Kind::Image;
    for &(name, bytes) in pkg.essentials {
        let path = dest.join(name);
        directory(dest, path.parent().expect("relative component file"), false)?;
        let meta = regular(&path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}：{e}", path.display())))?;
        if executable && !has_arm64(&path)? {
            return Err(invalid(format!(
                "宿主可执行文件缺少 arm64 架构：{}",
                path.display()
            )));
        }
        if meta.len() != bytes || (executable && meta.permissions().mode() & 0o111 == 0) {
            return Err(invalid(format!(
                "文件长度不符或不可执行：{}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// 此目录上游包只发布 Mach-O arm64 或同时含 arm64 的 universal 可执行文件。
fn has_arm64(path: &Path) -> io::Result<bool> {
    let mut f = File::open(path)?;
    let mut head = [0; 8];
    f.read_exact(&mut head)?;
    const ARM64: u32 = 0x0100000c;
    match u32::from_be_bytes(head[..4].try_into().unwrap()) {
        0xcffaedfe => Ok(u32::from_le_bytes(head[4..].try_into().unwrap()) == ARM64),
        magic @ (0xcafebabe | 0xcafebabf) => {
            let count = u32::from_be_bytes(head[4..].try_into().unwrap());
            if count > 32 {
                return Ok(false);
            }
            let mut row = [0; 32];
            for _ in 0..count {
                f.read_exact(&mut row[..if magic == 0xcafebabe { 20 } else { 32 }])?;
                if u32::from_be_bytes(row[..4].try_into().unwrap()) == ARM64 {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}
