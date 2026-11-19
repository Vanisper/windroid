//! 受管目录由一个工作区负责发布和迁移。只读借用的开发 SDK 不登记。
use super::{StorageLocations, absolute, invalid, metadata, rename_exclusive};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MARKER: &str = ".windroid-workspace";

fn owner(root: &Path) -> io::Result<Option<PathBuf>> {
    let marker = root.join(MARKER);
    let Some(meta) = metadata(&marker)? else {
        return Ok(None);
    };
    if !meta.is_file() || meta.len() > 16_384 {
        return Err(invalid(format!("目录归属记录无效：{}", marker.display())));
    }
    Ok(Some(serde_json::from_reader(File::open(marker)?)?))
}

fn check(home: &Path, root: &Path) -> io::Result<()> {
    for entry in root.ancestors() {
        if let Some(other) = owner(entry)?
            && other != home
        {
            return Err(invalid(format!(
                "{} 由另一个工作区 {} 管理，请选择其他目录",
                entry.display(),
                other.display()
            )));
        }
    }
    Ok(())
}

fn claim(home: &Path, root: &Path) -> io::Result<()> {
    if owner(root)?.as_deref() == Some(home) {
        return Ok(());
    }
    fs::create_dir_all(root)?;
    // 完整归属记录只发布一次。两个工作区同时选择空目录时只有一方能取得归属。
    let temp = root.join(format!(".windroid-owner-{}", crate::instances::nonce()?));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        serde_json::to_writer(&file, home)?;
        file.sync_all()?;
        match rename_exclusive(&temp, &root.join(MARKER)) {
            Err(e) if e.kind() == ErrorKind::AlreadyExists => check(home, root),
            other => other,
        }
    })();
    let _ = fs::remove_file(temp);
    result
}

pub(super) fn access(
    home: &Path,
    settings: &StorageLocations,
    components: bool,
    images: bool,
    register: bool,
) -> io::Result<()> {
    let home = absolute(home)?;
    for (needed, root) in [
        (components, settings.runtime_root.clone()),
        (images, settings.images()),
    ] {
        if needed {
            let root = absolute(&root)?;
            check(&home, &root)?;
            if register {
                claim(&home, &root)?;
            }
        }
    }
    Ok(())
}

pub(super) fn release(
    home: &Path,
    inactive: &StorageLocations,
    current: &StorageLocations,
) -> io::Result<()> {
    let home = absolute(home)?;
    let keep = [
        absolute(&current.runtime_root)?,
        absolute(&current.images())?,
    ];
    for path in [&inactive.runtime_root, &inactive.images()] {
        let root = absolute(path)?;
        if !keep.contains(&root) && owner(&root)?.as_deref() == Some(&home) {
            fs::remove_file(root.join(MARKER))?;
        }
    }
    Ok(())
}
