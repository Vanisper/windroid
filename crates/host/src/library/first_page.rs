//! 已通过格式检查的工作区，在首个页面实际布局后记录展示状态。
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct FirstPage {
    pending: PathBuf,
    welcome: AtomicBool,
}

impl FirstPage {
    pub fn open(home: &Path, show_guide_on_first_launch: bool) -> io::Result<Self> {
        let prefs = home.join("preferences");
        let pending = prefs.join("first-page.pending");
        let shown = prefs.join("first-page-shown");
        let first = if shown.try_exists()? {
            false
        } else if pending.try_exists()? {
            true
        } else {
            fs::create_dir_all(&prefs)?;
            File::create(&pending)?;
            true
        };
        Ok(Self {
            pending,
            welcome: AtomicBool::new(first && show_guide_on_first_launch),
        })
    }

    pub fn welcome(&self) -> bool {
        self.welcome.load(Ordering::Acquire)
    }

    pub fn shown(&self) -> io::Result<()> {
        let shown = self.pending.with_file_name("first-page-shown");
        match fs::rename(&self.pending, &shown) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && shown.is_file() => (),
            result => result?,
        }
        self.welcome.store(false, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_survives_an_unshown_page() {
        let dir = std::env::temp_dir().join(format!("windroid-first-page-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        for (name, enabled) in [("direct", false), ("welcome", true)] {
            let home = dir.join(name);
            assert_eq!(FirstPage::open(&home, enabled).unwrap().welcome(), enabled);
            fs::create_dir(home.join("instances")).unwrap();
            let page = FirstPage::open(&home, enabled).unwrap();
            assert_eq!(page.welcome(), enabled);
            page.shown().unwrap();
            page.shown().unwrap();
            fs::remove_dir(home.join("instances")).unwrap();
            assert!(!FirstPage::open(&home, true).unwrap().welcome());
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
