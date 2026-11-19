//! 应用库协调实例选择、设备命令和原生窗口。引擎线程只读取内存中的快照，
//! 每台实例有自己的命令队列；安装 A 的 APK 不妨碍切换或操作 B。
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use contract::{
    Ack, AppEntry, Command, Desktop, ErrorCode, Facts, InstanceId, Rejection, Snapshot, State,
};
use device::{Events, Handle};
use nsh_engine::{HostCall, HostChannel, JsonValue};
use runtime::{
    Layout,
    instances::{InstanceConfig, InstanceRecord, InstanceStore},
    provision::{Provisioner, RuntimePaths},
};
use serde_json::json;

use crate::{
    SnapshotView, names,
    surface::{WindowCommand, WindowEvent, WindowKey},
};

mod commands;
mod first_page;
mod observation;
mod resources;
#[cfg(test)]
mod tests;

pub(super) fn error(message: impl ToString) -> Rejection {
    Rejection::Error {
        code: ErrorCode::Other,
        detail: message.to_string(),
    }
}

type Reply = Result<u64, Rejection>;
struct Request {
    data: JsonValue,
    reply: Option<HostCall>,
}
type Lane = mpsc::SyncSender<Request>;

#[derive(Clone)]
struct Instance {
    name: String,
    handle: Handle,
    record: Option<InstanceRecord>,
    config: Option<InstanceConfig>,
    lane: Lane,
}

#[derive(Default)]
struct ViewState {
    selected: Option<String>,
    instances: BTreeMap<String, Instance>,
    operations: BTreeMap<String, JsonValue>,
    /// 已经打开或正在创建的窗口分别记账，连续打开计一次配额。
    open: BTreeSet<(String, Option<String>)>,
    opening: BTreeSet<(String, Option<String>)>,
    /// 每个实例等待原生窗口层完成的关闭批次。回执前的窗口事件属于旧会话。
    closing: BTreeMap<String, usize>,
    streaming: BTreeSet<(String, Option<String>)>,
    frozen: BTreeSet<String>,
    last_interaction: BTreeMap<String, Instant>,
    /// 最近一次设备任务观测；未读取、读取失败与确实无任务分开表示。
    tasks: BTreeMap<String, BTreeSet<String>>,
    task_errors: BTreeMap<String, String>,
}

impl ViewState {
    fn write_running(&self, out: &mut JsonValue, snap: &Snapshot) {
        let id = snap.id.as_str();
        let tasks = self.tasks.get(id);
        out["running"] = snap
            .apps
            .iter()
            .filter(|app| app.launcher)
            .filter_map(|app| {
                let package = app.package.as_str();
                let window_open = self.open.contains(&(id.into(), Some(package.into())));
                (window_open || tasks.is_some_and(|tasks| tasks.contains(package))).then(|| {
                    json!({
                        "instanceId": id, "package": package,
                        "label": app.label.as_deref().unwrap_or(package), "windowOpen": window_open
                    })
                })
            })
            .collect();

        // 任务栈非空时，要等 LAUNCHER 清单核实哪些包能恢复。两者的读取顺序不固定。
        let needs_catalog = tasks.is_some_and(|tasks| !tasks.is_empty());
        let catalog_error = match &snap.catalog_status {
            contract::CatalogStatus::Failed { message } if needs_catalog => {
                Some(format!("应用列表读取失败 · {message}"))
            }
            _ => None,
        };
        out["runningError"] = json!(self.task_errors.get(id).or(catalog_error.as_ref()));
        out["tasksLoaded"] = json!(
            tasks.is_some()
                && (!needs_catalog || snap.catalog_status == contract::CatalogStatus::Ready)
        );
    }
}

pub struct Library {
    layout: Layout,
    recommended_template: &'static str,
    first_page: Option<first_page::FirstPage>,
    runtime: Option<Provisioner>,
    store: Option<InstanceStore>,
    channel: HostChannel,
    windows: mpsc::Sender<WindowCommand>,
    state: Mutex<ViewState>,
    sequence: AtomicU64,
    starting: Mutex<()>,
    stopping: AtomicBool,
    /// 只有测试挂载的单实例允许省略路由键。
    fixture: bool,
}

impl Library {
    /// 启动应用库，fake 模式的数据仅在内存中存在。
    pub fn attach(
        channel: HostChannel,
        layout: Layout,
        fake: bool,
        windows: mpsc::Sender<WindowCommand>,
        events: mpsc::Receiver<WindowEvent>,
    ) -> Result<Arc<Self>, String> {
        let first_page = if fake {
            None
        } else {
            let enabled =
                std::env::var("WINDROID_SHOW_GUIDE_ON_FIRST_LAUNCH").as_deref() == Ok("1");
            Some(first_page::FirstPage::open(&layout.root, enabled).map_err(|e| e.to_string())?)
        };
        let mut library = Self::new(channel, layout, fake, windows, false);
        Arc::get_mut(&mut library).unwrap().first_page = first_page;
        if let Some(store) = &library.store {
            for record in store.list().map_err(|e| e.to_string())? {
                library.add_record(record);
            }
            let selected = store
                .selected()
                .map_err(|e| e.to_string())?
                .map(|id| id.to_string());
            let mut state = library.state.lock().unwrap();
            state.selected = selected
                .filter(|id| state.instances.contains_key(id))
                .or_else(|| state.instances.keys().next().cloned());
        }
        library.connect();
        let weak = Arc::downgrade(&library);
        thread::spawn(move || {
            for event in events {
                let Some(library) = weak.upgrade() else { break };
                library.window_event(event);
            }
        });
        if !fake {
            library.watch_preparation();
        }
        Ok(library)
    }

    fn new(
        channel: HostChannel,
        layout: Layout,
        fake: bool,
        windows: mpsc::Sender<WindowCommand>,
        fixture: bool,
    ) -> Arc<Self> {
        let physical_mib = runtime::instances::physical_memory_mib().unwrap_or(8192);
        Arc::new(Self {
            recommended_template: if physical_mib <= 8192 {
                "compact-720"
            } else {
                "phone-1080"
            },
            first_page: None,
            runtime: (!fake).then(|| Provisioner::new(layout.root.clone())),
            store: (!fake).then(|| InstanceStore::new(layout.clone())),
            layout,
            channel,
            windows,
            state: Mutex::default(),
            sequence: AtomicU64::new(0),
            starting: Mutex::new(()),
            stopping: AtomicBool::new(false),
            fixture,
        })
    }

    pub(crate) fn fixture(channel: HostChannel, handle: Handle, events: Events) -> Arc<Self> {
        let (windows, _) = mpsc::channel();
        let library = Self::new(
            channel,
            Layout {
                root: Default::default(),
            },
            true,
            windows,
            true,
        );
        let id = handle.id().to_string();
        library.add(id.clone(), handle, events, None);
        library.state.lock().unwrap().selected = Some(id);
        library.connect();
        library
    }

    fn add_record(self: &Arc<Self>, record: InstanceRecord) {
        let (handle, events) = device::spawn(
            record.id.clone(),
            record.serial(),
            runtime::EmulatorBackend::new(record.id.clone(), self.layout.clone(), record.port),
            device::Timeouts::default(),
        );
        self.add(record.name.clone(), handle, events, Some(record));
    }

    fn add(
        self: &Arc<Self>,
        name: String,
        handle: Handle,
        events: Events,
        record: Option<InstanceRecord>,
    ) {
        let id = handle.id().to_string();
        let instance = Instance {
            name,
            handle,
            lane: self.lane(),
            config: record.as_ref().map(|r| r.config(&self.layout)),
            record,
        };
        {
            let mut state = self.state.lock().unwrap();
            state.last_interaction.insert(id.clone(), Instant::now());
            state.instances.insert(id.clone(), instance);
        }
        self.watch_device(id.clone(), events);
        if self.store.is_some() {
            self.watch_tasks(id);
        }
    }

    fn connect(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.channel.handle("windroid.firstPageShown", move |call| {
            let Some(lib) = weak.upgrade() else {
                return call.reject("应用库已关闭");
            };
            let result = lib.first_page.as_ref().map(|p| p.shown()).transpose();
            call.reply(result.map(|_| JsonValue::Null).map_err(|e| e.to_string()));
        });
        self.connect_resources();
        let global = self.lane();
        let weak = Arc::downgrade(self);
        self.channel.handle(names::COMMAND, move |call| {
            let Some(lib) = weak.upgrade() else {
                call.reject("应用库已关闭");
                return;
            };
            let data = call.data().clone();
            let lane = if matches!(
                data["kind"].as_str(),
                Some(
                    "selectInstance"
                        | "createInstance"
                        | "deleteInstance"
                        | "prepareRuntime"
                        | "importImage"
                        | "prepareImage"
                        | "pausePreparation"
                        | "recheckRuntime"
                )
            ) {
                None
            } else {
                let state = lib.state.lock().unwrap();
                data["instanceId"]
                    .as_str()
                    .and_then(|id| state.instances.get(id))
                    .map(|i| i.lane.clone())
            };
            if let Err(e) = lane.as_ref().unwrap_or(&global).try_send(Request {
                data,
                reply: Some(call),
            }) {
                let (mpsc::TrySendError::Full(req) | mpsc::TrySendError::Disconnected(req)) = e;
                req.reply.unwrap().reject("操作队列繁忙，请稍后再试");
            }
        });
        let weak = Arc::downgrade(self);
        self.channel.handle(names::SNAPSHOT, move |call| {
            if let Some(lib) = weak.upgrade() {
                call.resolve(lib.snapshot());
            } else {
                call.reject("应用库已关闭");
            }
        });
        let weak = Arc::downgrade(self);
        self.channel.handle(names::READINESS, move |call| {
            if let Some(lib) = weak.upgrade() {
                call.resolve(lib.preparation());
            } else {
                call.reject("应用库已关闭");
            }
        });
    }

    fn lane(self: &Arc<Self>) -> Lane {
        let (tx, rx) = mpsc::sync_channel::<Request>(32);
        let weak = Arc::downgrade(self);
        thread::spawn(move || {
            for req in rx {
                let Some(lib) = weak.upgrade() else {
                    if let Some(call) = req.reply {
                        call.reject("应用库已关闭");
                    }
                    break;
                };
                let result = lib.perform(&req.data);
                let ack = match result {
                    Ok(seq) => Ack::Accepted { seq },
                    Err(reason) => {
                        lib.release_quit(&req.data);
                        lib.publish_note(
                            req.data["instanceId"].as_str(),
                            rejection_note(req.data["kind"].as_str(), &reason),
                        );
                        Ack::Rejected { reason }
                    }
                };
                if let Some(call) = req.reply {
                    call.resolve(crate::to_json(&ack));
                }
                lib.notify();
            }
        });
        tx
    }

    fn notify(&self) {
        let _ = self.channel.send(names::EVENT, JsonValue::Null);
    }
    fn note(&self, id: Option<&str>, failed: bool, message: impl AsRef<str>) {
        self.publish_note(id, json!({"message":message.as_ref(),"failed":failed}));
    }

    fn publish_note(&self, id: Option<&str>, mut note: JsonValue) {
        let mut state = self.state.lock().unwrap();
        note["instanceId"] = json!(id);
        note["seq"] = json!(self.revision());
        state.operations.insert(id.unwrap_or("").into(), note);
        drop(state);
        self.notify();
    }
    fn revision(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::Relaxed) + 1
    }
    /// 包含所选设备事实和全库导航数据。全部来自内存，查询不会调用 adb 或扫描磁盘。
    pub fn snapshot(&self) -> JsonValue {
        let preparation = self.preparation();
        let state = self.state.lock().unwrap();
        let current = state
            .selected
            .as_deref()
            .and_then(|id| state.instances.get(id));
        let snap = current
            .map(|i| i.handle.snapshot())
            .unwrap_or_else(|| Snapshot {
                id: InstanceId::new(""),
                serial: String::new(),
                state: State::Absent,
                phase: None,
                facts: Facts::default(),
                apps: vec![],
                catalog_status: contract::CatalogStatus::Pending,
                desktop: Desktop::default(),
                seq: 0,
            });
        let id = snap.id.to_string();
        let mut view = SnapshotView::of(snap.clone()).with_apps(&snap.apps, &snap.desktop);
        // actor 判断设备状态，应用库再叠加同一份准备快照中的主机条件。
        if preparation["componentsReady"] != true {
            view.capabilities.create = false;
            view.capabilities.start = false;
        }
        if let Some(record) = current.and_then(|row| row.record.as_ref()) {
            view.capabilities.start &= resources::image_available(&preparation, &record.spec.image);
        }
        let mut out = crate::to_json(&view);
        out["instances"] = state
            .instances
            .iter()
            .map(|(id, instance)| {
                let device = instance.handle.snapshot();
                json!({
                    "id": id, "name": instance.name, "state": device.state,
                    "frozen": state.frozen.contains(id),
                    "summary": crate::describe(device.state, device.phase, &device.facts),
                    "config": instance.config, "observed": device.facts.allocation
                })
            })
            .collect();
        out["showWelcome"] = json!(self.first_page.as_ref().is_some_and(|p| p.welcome()));
        out["selectedId"] = json!(state.selected);
        out["frozen"] = json!(state.frozen.contains(&id));
        out["operation"] = state
            .operations
            .get(&id)
            .or_else(|| state.operations.get(""))
            .cloned()
            .unwrap_or_default();
        state.write_running(&mut out, &snap);
        out["preparation"] = preparation;
        if let Some(runtime) = &self.runtime {
            let paths = runtime.paths();
            out["storage"] = json!({
                "home": self.layout.root, "sdk": paths.sdk,
                "images": paths.sdk.join("system-images")
            });
        }
        out["templateRamMiB"] = [
            "phone-1080",
            "compact-720",
            "tablet-1200",
            "tv-1080",
            "tv-4k",
        ]
        .into_iter()
        .map(|template| {
            (
                template.to_owned(),
                json!(runtime::instances::ram_mib(template)),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
        out["recommendedTemplate"] = self.recommended_template.into();
        out
    }

    /// 主窗口关闭时先关闭窗口会话，再请求这些 actor 停止各自持有的子进程。
    pub fn shutdown(&self) {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(runtime) = &self.runtime {
            let _ = runtime.pause();
        }
        let handles: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .instances
            .values()
            .map(|i| i.handle.clone())
            .collect();
        thread::scope(|scope| {
            for handle in handles {
                scope.spawn(move || {
                    if handle.snapshot().facts.process_alive {
                        let _ = handle.send(Command::Stop);
                    }
                });
            }
        });
    }
}

fn rejection_note(kind: Option<&str>, reason: &Rejection) -> JsonValue {
    // 原生文件面板通过事件完成，没有 JS 命令回执。将同一错误码与诊断带入实例快照。
    if kind == Some("install")
        && let Rejection::Error { code, detail } = reason
    {
        return json!({"failed":true,"message":"APK 安装未完成","installError":{"code":code,"detail":detail}});
    }
    let message = match reason {
        Rejection::Error { detail, .. } => detail.clone(),
        Rejection::NotReady { .. } => "实例尚未就绪".into(),
        Rejection::WrongState { wanted, .. } => format!("此操作需要{wanted}的实例"),
    };
    json!({"failed":true,"message":message})
}
