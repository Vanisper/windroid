//! 关闭偏好与任务采样。首次询问、临时选择和持久选择分别保存。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use host::surface::CloseChoice;

/// 连接中请求退出时，先撤销可能仍在发送的 START_APP。流线程收尾后才向实例
/// 请求强停，避免稍晚的启动把已经退出的应用重新拉起。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum QuitState {
    #[default]
    Idle,
    WaitingForWorker,
    Requested,
}

impl QuitState {
    pub fn begin(&mut self, can_quit: bool) -> bool {
        if *self != Self::Idle {
            return false;
        }
        *self = if can_quit {
            Self::Requested
        } else {
            Self::WaitingForWorker
        };
        can_quit
    }

    pub fn worker_ended(&mut self) -> bool {
        if *self != Self::WaitingForWorker {
            return false;
        }
        *self = Self::Requested;
        true
    }
}

pub struct Preferences {
    path: PathBuf,
    pub choice: Option<CloseChoice>,
}

impl Preferences {
    pub fn load(home: &Path) -> Self {
        let path = home.join("preferences/window-close");
        let read = || -> io::Result<_> {
            let mut value = String::new();
            File::open(&path)?.take(64).read_to_string(&mut value)?;
            match value.trim() {
                "background" => Ok(Some(CloseChoice::Background)),
                "quit" => Ok(Some(CloseChoice::Quit)),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "无法识别关闭偏好",
                )),
            }
        };
        let choice = match read() {
            Ok(choice) => choice,
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                log::warn!("读取关闭偏好失败：{e}");
                None
            }
        };
        Self { path, choice }
    }

    pub fn remember(&mut self, choice: CloseChoice, checked: bool) -> io::Result<()> {
        if !checked {
            return Ok(());
        }
        static NUMBER: AtomicU64 = AtomicU64::new(1);
        fs::create_dir_all(self.path.parent().expect("preference has parent"))?;
        let temp = self.path.with_extension(format!(
            "{}-{}",
            std::process::id(),
            NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        let value = match choice {
            CloseChoice::Background => "background",
            CloseChoice::Quit => "quit",
        };
        if let Err(error) = writeln!(file, "{value}").and_then(|_| fs::rename(&temp, &self.path)) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        self.choice = Some(choice);
        Ok(())
    }
}

/// 连接、首帧和 Activity 出现是三个独立的时刻。目标显示连续两次没有 Activity
/// 才关闭，允许设备在换 Activity 的瞬间交回一份空快照。
#[derive(Default)]
pub struct ActivityWatch {
    pub display: Option<u32>,
    pub painted: Option<Instant>,
    /// 已从 Android 任务快照确认目标活动实际启动。
    pub saw_task: bool,
    misses: u8,
}
impl ActivityWatch {
    pub fn on_tasks(&mut self, package: &str, tasks: &[(String, u32)], now: Instant) -> bool {
        let Some(display) = self.display else {
            return false;
        };
        if tasks.iter().any(|(p, d)| p == package && *d == display) {
            self.saw_task = true;
            self.misses = 0;
            return false;
        }
        self.misses = self.misses.saturating_add(1);
        let startup_over = self.painted.is_some_and(|painted| {
            now.saturating_duration_since(painted) >= Duration::from_secs(15)
        });
        self.misses >= 2 && (self.saw_task || startup_over)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_during_launch_waits_for_cleanup_and_is_dispatched_only_once() {
        let mut pending = QuitState::Idle;
        assert!(!pending.begin(false));
        assert!(!pending.begin(true));
        assert!(pending.worker_ended());
        assert!(!pending.worker_ended());
        assert_eq!(pending, QuitState::Requested);
        pending = QuitState::Idle;
        assert!(pending.begin(true));
        assert!(!pending.worker_ended());
    }

    #[test]
    fn temporary_close_choices_do_not_become_a_default() {
        let home = std::env::temp_dir().join(format!(
            "windroid-window-prefs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut prefs = Preferences::load(&home);
        assert_eq!(prefs.choice, None);
        prefs.remember(CloseChoice::Quit, false).unwrap();
        assert_eq!(Preferences::load(&home).choice, None);
        assert!(!home.exists());
        prefs.remember(CloseChoice::Background, true).unwrap();
        assert_eq!(
            Preferences::load(&home).choice,
            Some(CloseChoice::Background)
        );
        prefs.remember(CloseChoice::Quit, false).unwrap();
        assert_eq!(prefs.choice, Some(CloseChoice::Background));
        prefs.remember(CloseChoice::Quit, true).unwrap();
        assert_eq!(Preferences::load(&home).choice, Some(CloseChoice::Quit));
        fs::remove_dir_all(&home).unwrap();
    }
    #[test]
    fn self_exit_uses_package_and_display_and_tolerates_transitions() {
        let start = Instant::now();
        let mut activity = ActivityWatch::default();
        let ours = vec![("app".into(), 6)];
        let elsewhere = vec![("app".into(), 0), ("other".into(), 6)];
        assert!(!activity.on_tasks("app", &elsewhere, start));
        activity.display = Some(6);
        activity.painted = Some(start);
        assert!(!activity.on_tasks("app", &elsewhere, start));
        assert!(!activity.on_tasks("app", &elsewhere, start));
        assert!(!activity.on_tasks("app", &ours, start));
        assert!(!activity.on_tasks("app", &elsewhere, start));
        assert!(!activity.on_tasks("app", &ours, start));
        assert!(!activity.on_tasks("app", &elsewhere, start));
        assert!(activity.on_tasks("app", &elsewhere, start));
    }
}
