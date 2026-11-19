//! 记录用户实际选中的挂载点。路径在 /Volumes 外（例如手动挂载的磁盘映像）
//! 也可能在卸载后落回宿主卷；目录存在不能证明它仍在原卷上。
use super::*;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Mounts {
    pub runtime: Option<PathBuf>,
    pub images: Option<PathBuf>,
}

pub(super) fn at(path: &Path) -> io::Result<PathBuf> {
    let existing = existing_ancestor(path)?;
    #[cfg(target_os = "macos")]
    {
        use std::ffi::{CStr, CString, OsStr};
        use std::os::unix::ffi::OsStrExt;
        let name = CString::new(existing.as_os_str().as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        let stat = unsafe {
            if libc::statfs(name.as_ptr(), stat.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            stat.assume_init()
        };
        // Darwin 的 f_mntonname 是以 NUL 结尾的本地挂载路径。
        let bytes = unsafe { CStr::from_ptr(stat.f_mntonname.as_ptr()) }.to_bytes();
        Path::new(OsStr::from_bytes(bytes)).canonicalize()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let canonical = existing.canonicalize()?;
        let dev = fs::metadata(&canonical)?.dev();
        let mut point = canonical.as_path();
        while let Some(parent) = point.parent() {
            if fs::metadata(parent)?.dev() != dev {
                break;
            }
            point = parent;
        }
        Ok(point.into())
    }
}

pub(super) fn load(home: &Path) -> io::Result<Mounts> {
    #[derive(Deserialize)]
    struct Binding {
        #[serde(default)]
        mounts: Mounts,
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(home.join(windroid_locations::SETTINGS));
    match file {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Mounts::default()),
        Ok(file) => {
            if !file.metadata()?.is_file() || file.metadata()?.len() > 32 * 1024 {
                return Err(invalid("存储设置类型或长度无效"));
            }
            let value: Binding = serde_json::from_reader(file)?;
            value.mounts.validate()?;
            Ok(value.mounts)
        }
        Err(e) => Err(e),
    }
}
impl Mounts {
    pub(super) fn validate(&self) -> io::Result<()> {
        for point in [&self.runtime, &self.images].into_iter().flatten() {
            if !point.is_absolute() || point.components().any(|p| p == Component::ParentDir) {
                return Err(invalid("资源卷挂载路径无效"));
            }
        }
        Ok(())
    }
    pub(super) fn check(&self, path: &Path, image: bool) -> io::Result<()> {
        check_volume(path)?;
        let point = if image { &self.images } else { &self.runtime };
        if let Some(expected) = point
            && at(path)? != *expected
        {
            return Err(io::Error::new(
                ErrorKind::NotConnected,
                format!("存储卷未挂载在 {}，请重新连接该卷", expected.display()),
            ));
        }
        Ok(())
    }
}

pub(crate) fn check_selected_volume(home: &Path, root: &Path) -> io::Result<()> {
    check_volume(root)?;
    let settings = StorageLocations::load(home)?;
    let points = load(home)?;
    if same_location(root, &settings.runtime_root) {
        points.check(root, false)?;
    }
    if same_location(root, &settings.image_root) {
        points.check(root, true)?;
    }
    Ok(())
}
