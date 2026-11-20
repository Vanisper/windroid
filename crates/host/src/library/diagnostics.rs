//! 只接受当前工作区内的来源标识。所有目录扫描、分页读取与 Finder 定位都在
//! 同一个后台队列完成；慢盘不会占用引擎线程或实例的启停队列。
use super::{Arc, HostCall, JsonValue, Library, json, mpsc, thread};
use objc2::rc::Retained;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSArray, NSString, NSURL};
use serde::Deserialize;
use std::{fs::File, io, os::fd::AsRawFd};
use windroid_diagnostics::{
    Catalog, LocatedFile, legacy_page, open_legacy_file, open_segment_file, read_page,
};

#[derive(Deserialize)]
#[serde(
    tag = "source",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Source {
    App,
    Emulator { instance_id: String },
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum Query {
    Catalog,
    LastStart,
    Page {
        run_id: String,
        day: Option<String>,
        after: u64,
        limit: usize,
    },
    Legacy {
        offset: u64,
    },
    Reveal {
        run_id: String,
        seq: u64,
    },
    RevealLegacy,
}

impl Library {
    pub(super) fn connect_diagnostics(self: &Arc<Self>) {
        let (send, requests) = mpsc::sync_channel::<HostCall>(32);
        let weak = Arc::downgrade(self);
        thread::spawn(move || {
            for mut call in requests {
                let Some(library) = weak.upgrade() else {
                    call.reject("应用库已关闭");
                    continue;
                };
                let result = library.query_logs(call.take_data());
                call.reply(result.map_err(|e| e.to_string()));
            }
        });
        self.channel
            .handle("windroid.logs", move |call| match send.try_send(call) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(call)) => call.reject("日志读取队列繁忙，请稍后刷新"),
                Err(mpsc::TrySendError::Disconnected(call)) => call.reject("应用库已关闭"),
            });
    }

    fn query_logs(&self, data: JsonValue) -> io::Result<JsonValue> {
        let source: Source = serde_json::from_value(data.clone())?;
        let query: Query = serde_json::from_value(data)?;
        let instance = match &source {
            Source::App => None,
            Source::Emulator { instance_id } => {
                if !self
                    .state
                    .lock()
                    .unwrap()
                    .instances
                    .contains_key(instance_id)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "实例已移除或不属于此工作区",
                    ));
                }
                Some(instance_id.as_str())
            }
        };
        let home = &self.layout.root;
        // 无持久化目录的无窗测试与内存模拟实例不能从相对路径读到工作目录。
        if (home.as_os_str().is_empty() || (instance.is_some() && self.store.is_none()))
            && !matches!(query, Query::LastStart)
        {
            return match query {
                Query::Catalog => Ok(json!({"runs":[],"legacy":false,"issue":null,"issues":[]})),
                _ => Err(io::Error::new(io::ErrorKind::NotFound, "没有持久化日志")),
            };
        }
        let legacy_instance =
            || instance.ok_or_else(|| io::Error::other("旧版日志只属于模拟器实例"));
        match query {
            Query::Catalog => {
                let issue = runtime::app_log::capture_issue(home, instance);
                let catalog = Catalog::load(home, instance).map_err(|e| match &issue {
                    Some(reason) => io::Error::new(
                        e.kind(),
                        format!("日志采集不可用 · {reason}；读取目录失败 · {e}"),
                    ),
                    None => e,
                })?;
                Ok(
                    json!({"runs":catalog.runs,"legacy":catalog.legacy,"issue":issue,"issues":catalog.issues}),
                )
            }
            Query::Page {
                run_id,
                day,
                after,
                limit,
            } => {
                let page = read_page(
                    home,
                    instance,
                    &run_id,
                    day.as_deref(),
                    after,
                    limit.clamp(1, 24),
                )?;
                Ok(crate::to_json(&page))
            }
            Query::Legacy { offset } => {
                let (text, next, more) = legacy_page(home, legacy_instance()?, offset, 16 * 1024)?;
                Ok(json!({"text":text, "next":next, "more":more}))
            }
            Query::Reveal { run_id, seq } => {
                reveal(open_segment_file(home, instance, &run_id, seq)?)
            }
            Query::LastStart => {
                let result = instance
                    .and_then(|id| runtime::app_log::last_start(home, id))
                    .map(|(app, emulator)| json!({"appRunId":app, "emulatorRunId":emulator}));
                Ok(json!(result))
            }
            Query::RevealLegacy => reveal(open_legacy_file(home, legacy_instance()?)?),
        }
    }
}

// 由仍持有的描述符构造文件引用 URL。Finder 按文件身份选中，目录在交接时更名也可识别。
// https://developer.apple.com/documentation/foundation/nsurl/filereferenceurl()
fn file_reference(file: &File) -> Option<Retained<NSURL>> {
    let descriptor = NSString::from_str(&format!("/dev/fd/{}", file.as_raw_fd()));
    NSURL::fileURLWithPath(&descriptor).fileReferenceURL()
}
fn reveal(location: LocatedFile) -> io::Result<JsonValue> {
    objc2::rc::autoreleasepool(|_| {
        let reference = file_reference(&location.file).ok_or_else(|| {
            io::Error::other(format!("无法定位日志文件 · {}", location.path.display()))
        })?;
        NSWorkspace::sharedWorkspace()
            .activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[reference]));
        Ok(JsonValue::Null)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finder_reference_follows_the_open_file_when_its_name_changes() {
        let home = std::env::temp_dir().join(windroid_diagnostics::new_run_id().unwrap());
        std::fs::create_dir(&home).unwrap();
        let path = home.join("first");
        let file = File::create_new(&path).unwrap();
        let reference = file_reference(&file).unwrap();
        let moved = home.join("moved");
        std::fs::rename(&path, &moved).unwrap();
        File::create_new(&path).unwrap();
        let resolved = reference.filePathURL().unwrap().path().unwrap().to_string();
        assert_eq!(
            std::fs::canonicalize(resolved).unwrap(),
            moved.canonicalize().unwrap()
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}
