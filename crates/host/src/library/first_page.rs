//! 初次入口属于工作区。宿主先认出旧版留下的目录，再允许实例注册表创建空目录。
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
            let legacy = [
                "instances",
                "preferences",
                "runtime",
                "staging",
                "groups.json",
                "cache",
                "logs",
            ]
            .into_iter()
            .map(|name| home.join(name).try_exists())
            .try_fold(false, |found, item| item.map(|exists| found || exists))?;
            fs::create_dir_all(&prefs)?;
            // 提前保留首次资格。界面未载入就退出时，新建的实例注册表不会被误认成旧工作区。
            File::create(if legacy { &shown } else { &pending })?;
            !legacy
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
    fn entry_survives_an_unshown_page_and_recognizes_legacy_workspaces() {
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
        let legacy = dir.join("legacy");
        fs::create_dir_all(legacy.join("instances")).unwrap();
        assert!(!FirstPage::open(&legacy, true).unwrap().welcome());
        fs::remove_dir_all(dir).unwrap();
    }
}
