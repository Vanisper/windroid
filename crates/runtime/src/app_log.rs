//! 主进程在工作区确认后登记本次启动。采集子进程和只读预览不初始化主程序日志。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::Level;
use windroid_diagnostics::{Event, RunWriter, Sink, new_run_id};

struct CaptureStatus {
    run: Option<String>,
    issue: Option<String>,
}

struct Current {
    home: PathBuf,
    id: String,
    sink: Option<Sink>,
    error: Option<String>,
    captures: BTreeMap<String, CaptureStatus>,
    attempts: BTreeMap<String, Option<String>>,
}

static CURRENT: Mutex<Option<Current>> = Mutex::new(None);

/// 窗口、流和实例全部收尾后，主线程用已知结果封存。崩溃时留给读取者判断写者锁。
pub struct AppLog {
    home: PathBuf,
    writer: Option<RunWriter>,
}

impl AppLog {
    pub fn open(home: &Path, id: &str) -> Self {
        let (writer, error) = match RunWriter::app_with_id(home, id) {
            Ok(writer) => (Some(writer), None),
            Err(error) => {
                eprintln!("Windroid 日志采集不可用：{error}");
                (None, Some(error.to_string()))
            }
        };
        *CURRENT.lock().unwrap() = Some(Current {
            home: home.into(),
            id: id.to_owned(),
            sink: writer.as_ref().map(RunWriter::sink),
            error,
            captures: BTreeMap::new(),
            attempts: BTreeMap::new(),
        });
        record(home, Level::Info, "主程序", None, None, "Windroid 启动");
        Self {
            home: home.into(),
            writer,
        }
    }

    pub fn allocate_id() -> std::io::Result<String> {
        new_run_id()
    }

    pub fn finish(self, result: &str) {
        record(&self.home, Level::Info, "主程序", None, None, result);
        CURRENT.lock().unwrap().take();
        if let Some(writer) = self.writer
            && let Err(error) = writer.finish(diagnostic_text(result))
        {
            eprintln!("Windroid 日志封存失败：{error}");
        }
    }
}

pub fn current_id(home: &Path) -> Option<String> {
    CURRENT
        .lock()
        .unwrap()
        .as_ref()
        .filter(|s| s.home == home)
        .map(|s| s.id.clone())
}

/// 宿主开始执行时清除上次关联；通过所有检查、分配本次模拟器启动标识后再绑定。
pub fn start_attempt(home: &Path, id: &str, run: Option<&str>) {
    if let Some(s) = CURRENT.lock().unwrap().as_mut().filter(|s| s.home == home) {
        s.attempts.insert(id.to_owned(), run.map(str::to_owned));
    }
}

pub fn last_start(home: &Path, id: &str) -> Option<(String, Option<String>)> {
    let guard = CURRENT.lock().unwrap();
    let s = guard.as_ref().filter(|s| s.home == home)?;
    Some((s.id.clone(), s.attempts.get(id)?.clone()))
}

/// 预检拒绝只更新执行请求的关联。实际通过检查的新采集周期才替换实例的采集状态。
pub(crate) fn begin_capture(home: &Path, id: &str, run: Option<&str>) {
    if let Some(s) = CURRENT.lock().unwrap().as_mut().filter(|s| s.home == home) {
        s.attempts.insert(id.to_owned(), run.map(str::to_owned));
        s.captures.insert(
            id.to_owned(),
            CaptureStatus {
                run: run.map(str::to_owned),
                issue: None,
            },
        );
    }
}

pub(crate) fn set_capture_issue(home: &Path, id: &str, run: Option<&str>, reason: String) {
    let mut guard = CURRENT.lock().unwrap();
    if let Some(status) = guard
        .as_mut()
        .filter(|s| s.home == home)
        .and_then(|s| s.captures.get_mut(id))
        .filter(|status| status.run.as_deref() == run)
    {
        status.issue = Some(reason);
    }
}

pub fn capture_issue(home: &Path, instance: Option<&str>) -> Option<String> {
    let guard = CURRENT.lock().unwrap();
    let session = guard.as_ref().filter(|s| s.home == home)?;
    match instance {
        Some(id) => session.captures.get(id)?.issue.clone(),
        None => session.error.clone().or_else(|| {
            session
                .sink
                .as_ref()?
                .degraded()
                .then(|| "写入受限，部分记录可能缺失".into())
        }),
    }
}

pub fn record(
    home: &Path,
    level: Level,
    component: &str,
    instance: Option<&str>,
    emulator_run: Option<&str>,
    message: &str,
) {
    let guard = CURRENT.lock().unwrap();
    let Some(session) = guard.as_ref().filter(|s| s.home == home) else {
        return;
    };
    if let Some(sink) = &session.sink {
        let mut event = Event::new("windroid", diagnostic_text(message));
        event.level = Some(level.to_string());
        event.component = Some(component.to_owned());
        event.instance_id = instance.map(String::from);
        event.emulator_run_id = emulator_run.map(String::from);
        sink.emit(event);
    } else {
        eprintln!("{level} {component} {} {message}", instance.unwrap_or(""));
    }
}

/// 只接收 Windroid 自己的警告与错误。底层库可能输出请求内容或完整传输细节。
pub fn warning(record: &log::Record<'_>) {
    let component = record.target().split("::").next().unwrap_or_default();
    if record.level() > Level::Warn
        || !matches!(
            component,
            "windroid" | "runtime" | "host" | "stream" | "adb" | "device"
        )
    {
        return;
    }
    let home = CURRENT.lock().unwrap().as_ref().map(|s| s.home.clone());
    if let Some(home) = home {
        self::record(
            &home,
            record.level(),
            component,
            None,
            None,
            &record.args().to_string(),
        );
    }
}

/// 第三方下载错误可能嵌入带鉴权信息的 URL；诊断只需保留其前后的错误描述。
fn diagnostic_text(message: &str) -> String {
    let mut text = String::new();
    let mut rest = message;
    while let Some(start) = rest
        .find("https://")
        .into_iter()
        .chain(rest.find("http://"))
        .min()
    {
        text.push_str(&rest[..start]);
        text.push_str("[URL]");
        rest = &rest[start..];
        rest = rest
            .find(char::is_whitespace)
            .map_or("", |end| &rest[end..]);
    }
    text.push_str(rest);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwritable_log_location_keeps_the_session_and_the_original_file() {
        let id = AppLog::allocate_id().unwrap();
        let root = std::env::temp_dir().join(format!("windroid-log-fallback-{id}"));
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join("logs/app"), b"existing").unwrap();
        let session = AppLog::open(&root, &id);
        assert_eq!(current_id(&root).as_deref(), Some(id.as_str()));
        assert!(capture_issue(&root, None).is_some());
        start_attempt(&root, "phone", None);
        assert_eq!(last_start(&root, "phone"), Some((id, None)));
        record(
            &root,
            Level::Warn,
            "实例调度",
            Some("phone"),
            None,
            "端口被占用",
        );
        begin_capture(&root, "phone", Some("first"));
        begin_capture(&root, "phone", Some("second"));
        set_capture_issue(&root, "phone", Some("first"), "上次采集器迟到".into());
        assert!(capture_issue(&root, Some("phone")).is_none());
        start_attempt(&root, "phone", None);
        set_capture_issue(&root, "phone", Some("second"), "活动采集有缺口".into());
        assert_eq!(
            capture_issue(&root, Some("phone")).as_deref(),
            Some("活动采集有缺口")
        );
        session.finish("正常退出");
        assert!(current_id(&root).is_none());
        assert_eq!(std::fs::read(root.join("logs/app")).unwrap(), b"existing");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn download_diagnostics_omit_credentials_and_query_strings() {
        assert_eq!(
            diagnostic_text(
                "下载 https://user:example@host/path?token=a 失败 http://other/private"
            ),
            "下载 [URL] 失败 [URL]"
        );
    }
}
