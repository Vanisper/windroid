//! 原生应用库的无窗交互测试。生产 TSX、宿主桥和 Fake actor 走同一条命令与事件链。
//! 许可和下载用内存回执控制各个阶段，测试不会下载组件或修改用户的实例。

use contract::{Ack, Command, ErrorCode, Image, InstanceId, Rejection, Spec};
use device::FakeBackend;
use host::surface::{WindowCommand, WindowEvent};
use nsh_core::{
    Bounds, Points, Size,
    platform::{Idiom, Os},
};
use nsh_engine::{Bundle, EngineConfig, HostChannel};
use nsh_testing::Headless;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(8);
const READER: &str = "com.example.reader";

fn config() -> EngineConfig {
    static BUNDLE: OnceLock<Bundle> = OnceLock::new();
    let sets = vec![nsh_base::registry()];
    let bundle = BUNDLE.get_or_init(|| {
        let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/App.tsx");
        nsh_testing::compile_tsx(entry, &sets).expect("编译应用与原生控件")
    });
    let mut config = EngineConfig::new(bundle.clone());
    config.ui_sets = sets;
    config.viewport = Size::new(Points(1024.0), Points(900.0));
    config.environment.window = config.viewport;
    config.environment.os = Os::MacOs;
    config.environment.idiom = Idiom::Desktop;
    config
}

fn render(config: EngineConfig) -> Headless {
    let mut app = Headless::start(config).expect("启动应用库");
    app.first_render().expect("首帧");
    app
}

fn wait(app: &mut Headless, selector: &str) {
    app.wait_for_within(selector, WAIT)
        .unwrap_or_else(|e| panic!("{selector}: {e}"));
}

fn gone(app: &mut Headless, selector: &str) {
    app.wait_for_gone(selector, WAIT)
        .unwrap_or_else(|e| panic!("{selector}: {e}"));
}

fn bounds(app: &Headless, test_id: &str) -> Bounds<Points> {
    app.screen()
        .layout_nodes()
        .into_iter()
        .find(|node| node.test_id.as_deref() == Some(test_id))
        .unwrap()
        .bounds
}

fn press(app: &mut Headless, id: &str) {
    app.press(&format!("id:{id}"))
        .unwrap_or_else(|e| panic!("{id}: {e}"));
}

fn choose(app: &mut Headless, label: &str) {
    app.choose_menu_item(label)
        .unwrap_or_else(|e| panic!("{label}: {e}"));
}

struct LibraryUi {
    app: Headless,
    library: Arc<host::Library>,
    windows: mpsc::Receiver<WindowCommand>,
    events: mpsc::Sender<WindowEvent>,
}

impl LibraryUi {
    fn new() -> Self {
        let config = config();
        let (tx, windows) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        let library = host::Library::attach(
            config.host.clone(),
            runtime::Layout {
                root: PathBuf::new(),
            },
            true,
            tx,
            rx,
        )
        .expect("挂载 Fake 应用库");
        let mut app = render(config);
        wait(&mut app, "id:inventory");
        press(&mut app, "workspace-next");
        wait(&mut app, "id:use-system-image");
        press(&mut app, "use-system-image");
        Self {
            app,
            library,
            windows,
            events,
        }
    }

    fn create(&mut self) -> String {
        wait(&mut self.app, "id:hardware-fields");
        press(&mut self.app, "create");
        wait(&mut self.app, "id:start");
        gone(&mut self.app, "id:pending");
        self.library.snapshot()["selectedId"]
            .as_str()
            .unwrap()
            .into()
    }

    fn start(&mut self) {
        press(&mut self.app, "start");
        wait(&mut self.app, "text:/服务 3/3/");
        gone(&mut self.app, "id:pending");
    }
}

/// 两个相同包名的实例通过各自实例 ID 关联。后台保留运行行，完全退出移除该行。
#[test]
fn windows_follow_the_instance_through_background_restore_and_quit() {
    let mut ui = LibraryUi::new();
    let first = ui.create();
    ui.start();
    let app = &mut ui.app;
    press(app, "open-device");
    wait(app, "text:窗口已连接");
    gone(app, "id:pending");
    wait(app, "id:app-com.example.reader");
    press(app, "app-com.example.reader");
    gone(app, "id:pending");
    press(app, "panel-tasks");
    wait(app, "text:窗口已打开");
    gone(app, "id:app-com.example.reader");

    press(app, "new-instance");
    press(app, "use-system-image");
    let second = ui.create();
    ui.start();
    press(&mut ui.app, "app-com.example.reader");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, "panel-tasks");
    wait(&mut ui.app, "text:窗口已打开");
    assert_eq!(ui.library.snapshot()["running"][0]["instanceId"], second);

    press(&mut ui.app, &format!("instance-{first}"));
    wait(&mut ui.app, "id:app-com.example.reader");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, "panel-tasks");
    press(&mut ui.app, "background-com.example.reader");
    wait(&mut ui.app, "text:后台运行");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, "focus-com.example.reader");
    wait(&mut ui.app, "text:窗口已打开");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, "quit-com.example.reader");
    wait(&mut ui.app, "id:tasks-empty");
    wait(&mut ui.app, "text:应用已退出");
    assert_eq!(ui.library.snapshot()["running"], json!([]));

    press(&mut ui.app, &format!("instance-{second}"));
    wait(&mut ui.app, "id:app-com.example.reader");
    press(&mut ui.app, "panel-tasks");
    wait(&mut ui.app, "text:窗口已打开");
    assert_eq!(ui.library.snapshot()["running"][0]["instanceId"], second);
}

#[test]
fn launchable_packages_open_from_both_source_sections() {
    let mut ui = LibraryUi::new();
    let instance = ui.create();
    ui.start();
    let app = &mut ui.app;
    for (source, package, actions) in [
        ("installed", READER, vec!["打开", "结束进程", "清除数据"]),
        ("system", "com.example.launcher", vec!["打开"]),
    ] {
        let tile = format!("id:section-{source} >> id:app-{package}");
        wait(app, &tile);
        assert_eq!(app.screen().count(&format!("id:app-{package}")).unwrap(), 1);
        assert!(app.screen().enabled(&tile).unwrap());
        app.right_click(&tile).unwrap();
        let labels: Vec<_> = app
            .menu()
            .unwrap()
            .into_iter()
            .map(|action| action.label)
            .collect();
        assert_eq!(labels, actions);
        choose(app, "打开");
        gone(app, "id:pending");
    }
    gone(app, "role:button[分组]");
    press(app, "panel-tasks");
    wait(app, "id:focus-com.example.reader");
    wait(app, "id:focus-com.example.launcher");
    let snapshot = ui.library.snapshot();
    let tasks = snapshot["running"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    assert!(
        tasks
            .iter()
            .all(|task| task["instanceId"] == instance && task["windowOpen"] == true)
    );
}

#[test]
fn apk_picker_keeps_its_opening_instance_and_window_errors_remain_visible() {
    let mut ui = LibraryUi::new();
    let first = ui.create();
    ui.start();
    press(&mut ui.app, "add-apk");
    let target = match ui.windows.recv_timeout(WAIT).unwrap() {
        WindowCommand::PickApk { instance_id, .. } => instance_id,
        other => panic!("应弹出文件选择器: {other:?}"),
    };
    assert_eq!(target, first);
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, "new-instance");
    press(&mut ui.app, "use-system-image");
    let second = ui.create();
    ui.events
        .send(WindowEvent::ApkPicked {
            instance_id: target,
            path: "/tmp/Fake.apk".into(),
        })
        .unwrap();
    assert_eq!(ui.library.snapshot()["selectedId"], second);
    press(&mut ui.app, &format!("instance-{first}"));
    wait(&mut ui.app, "text:/APK 已安装/");
    // 无 JS 回执的文件面板事件也走真实命令队列。空路径让内存设备产生可重复的拒绝。
    press(&mut ui.app, "add-apk");
    assert!(
        matches!(ui.windows.recv_timeout(WAIT).unwrap(), WindowCommand::PickApk { instance_id, .. } if instance_id == first)
    );
    ui.events
        .send(WindowEvent::ApkPicked {
            instance_id: first.clone(),
            path: PathBuf::new(),
        })
        .unwrap();
    wait(&mut ui.app, "id:toggle-install-diagnostic");
    let note = &ui.library.snapshot()["operation"];
    assert_eq!(note["instanceId"], first);
    assert_eq!(
        note["installError"],
        json!({"code": ErrorCode::Other, "detail": "请选择 APK 文件"})
    );
    gone(&mut ui.app, "id:install-diagnostic");
    press(&mut ui.app, "toggle-install-diagnostic");
    wait(&mut ui.app, "text:请选择 APK 文件");
    ui.events
        .send(WindowEvent::Error {
            instance_id: first.clone(),
            message: "窗口未收到首帧".into(),
        })
        .unwrap();
    wait(&mut ui.app, "text:窗口未收到首帧");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, &format!("instance-{second}"));
    gone(&mut ui.app, "text:窗口未收到首帧");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, &format!("instance-{first}"));
    wait(&mut ui.app, "text:窗口未收到首帧");
}

#[test]
fn creation_template_and_instance_deletion_confirmation() {
    let mut ui = LibraryUi::new();
    wait(&mut ui.app, "text:创建第一个实例");
    press(&mut ui.app, "choose-template");
    choose(&mut ui.app, "紧凑设备 · 1280 × 720");
    wait(&mut ui.app, "text:/紧凑设备/");
    let id = ui.create();
    ui.start();
    press(&mut ui.app, "stop");
    wait(&mut ui.app, "id:start");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, "open-management");
    press(&mut ui.app, "delete-instance");
    wait(&mut ui.app, "text:删除「我的 Android」？");
    press(&mut ui.app, "cancel-confirmation");
    assert_eq!(ui.library.snapshot()["selectedId"], id);
    press(&mut ui.app, "delete-instance");
    press(&mut ui.app, "confirm-action");
    wait(&mut ui.app, "id:inventory");
    assert_eq!(ui.library.snapshot()["instances"], json!([]));
}

#[test]
fn a_stalled_boot_reports_the_missing_service() {
    let config = config();
    let (handle, events) = device::spawn(
        InstanceId::new("ui-test"),
        "fake-5554".into(),
        FakeBackend::new().stalling(),
        device::Timeouts {
            adb: Duration::from_millis(300),
            services: Duration::from_millis(300),
            hard: Duration::from_secs(30),
            poll: Duration::from_millis(10),
        },
    );
    let _bridge = host::Bridge::attach(handle.clone(), config.host.clone(), events);
    let spec = Spec {
        template: "phone-1080".into(),
        image: Image {
            revision: 14,
            api: 34,
            tag: "google_apis_playstore".into(),
            abi: "arm64-v8a".into(),
        },
        disk_bytes: 8 << 30,
    };
    assert!(handle.send(Command::Create { spec }).is_accepted());
    let mut app = render(config);
    wait(&mut app, "id:instance-ui-test >> text:已停止");
    let stopped = bounds(&app, "instance-ui-test");
    press(&mut app, "start");
    wait(&mut app, "text:等待package 服务");
    wait(&mut app, "id:instance-ui-test >> text:/启动较慢，服务 0/3/");
    assert_eq!(stopped, bounds(&app, "instance-ui-test"));
}

/// 可控回执让测试推进下载阶段或文件拖入，而不用点击真实的下载器、文件面板。
struct Scripted {
    app: Headless,
    state: Arc<Mutex<Value>>,
    commands: Arc<Mutex<Vec<Value>>>,
    channel: HostChannel,
    first_pages: Arc<std::sync::atomic::AtomicUsize>,
}

impl Scripted {
    fn new(
        state: Value,
        handle: impl Fn(&Value, &mut Value) -> Value + Send + Sync + 'static,
    ) -> Self {
        let config = config();
        let channel = config.host.clone();
        let state = Arc::new(Mutex::new(state));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let view = state.clone();
        let first_pages = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let receipts = first_pages.clone();
        channel.handle("windroid.firstPageShown", move |call| {
            receipts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            call.resolve(Value::Null);
        });
        channel.handle("windroid.snapshot", move |call| {
            call.resolve(view.lock().unwrap().clone())
        });
        channel.handle("windroid.hardwareLimits", |call| {
            let limits = hardware_limits(call.data());
            call.resolve(limits);
        });
        let view = state.clone();
        let seen = requests.clone();
        channel.handle("windroid.command", move |call| {
            seen.lock().unwrap().push(call.data().clone());
            let ack = handle(call.data(), &mut view.lock().unwrap());
            call.resolve(ack);
        });
        Self {
            app: render(config),
            state,
            commands: requests,
            channel,
            first_pages,
        }
    }

    fn update(&self, change: impl FnOnce(&mut Value)) {
        change(&mut self.state.lock().unwrap());
        self.channel.send("windroid.event", Value::Null).unwrap();
    }
}

fn image(id: &str, api: u32, ready: bool) -> Value {
    json!({"id":id, "name":format!("Android API {api}"), "version":"14", "source":"https://example.org/image.zip",
        "license":"镜像许可", "licenseUrl":"https://example.org/terms", "licenseText":"镜像许可条款", "sizeBytes":1048576,
        "ready":ready, "location":"/tmp/images","repairToken":null, "integrity":if ready {"quick"} else {"missing"},
        "writable":true,"requiredBytes":if ready {0} else {3145728},"availableBytes":1073741824, "downloadedBytes":0, "issue":null,
        "image":{"api":api,"tag":"google_apis_playstore","abi":"arm64-v8a","revision":14},"minEmulator":"34.0.0"})
}

fn hardware_limits(target: &Value) -> Value {
    let minimum = if target["template"] == "tv-4k" {
        4096
    } else {
        2560
    };
    json!({"minRamMiB":minimum,"maxRamMiB":8192,"maxCpuCores":8,"defaultCpuCores":2,
        "defaultRamMiB":minimum,"maxDiskGiB":40,"defaultDiskGiB":8})
}

fn ready_view() -> Value {
    json!({"id":"a","instances":[{"id":"a","name":"工作机","state":"ready"}],"selectedId":"a",
        "state":"ready","summary":"已就绪","capabilities":{"install":true,"launchApp":true,"deviceView":true,"stop":true,"refreshApps":true},
        "facts":{"services":{"activity":true,"package":true,"window":true},"adbVisible":true},"groups":[],"catalogStatus":{"kind":"ready"},"running":[],"desktop":null,"tasksLoaded":true,
        "preparation":{"phase":"ready","componentsReady":true,"images":[image("r34",34,true)],"components":[],"currentComponent":null,"totalBytes":0,"downloadedBytes":0,"candidateTarget":null,"candidateDownloadedBytes":null,"error":null}})
}

#[test]
fn preparation_requires_consent_and_supports_license_pause_resume_retry_and_rescan() {
    let mut view = ready_view();
    view["instances"] = json!([]);
    view["selectedId"] = Value::Null;
    view["preparation"] = json!({"phase":"missing","components":[{"id":"image","name":"系统镜像","version":"34","source":"https://example.org/image.zip",
        "license":"镜像许可","licenseUrl":"https://example.org/terms","licenseText":"镜像完整许可正文","sizeBytes":1048576,"ready":false,
        "location":"/tmp/runtime","integrity":"missing","repairToken":null,"writable":true,"requiredBytes":3145728,"availableBytes":1073741824}],
        "currentComponent":null,"downloadedBytes":0,"totalBytes":1048576,"candidateTarget":null,"candidateDownloadedBytes":null,"error":null});
    let mut ui = Scripted::new(view, |cmd, view| {
        let p = &mut view["preparation"];
        match cmd["kind"].as_str().unwrap() {
            "prepareRuntime" => {
                assert_eq!(cmd["acceptedLicenses"], true);
                p["phase"] = json!("downloading");
                p["error"] = Value::Null;
                p["currentComponent"] = json!("image");
                p["downloadedBytes"] = json!(524288);
            }
            "pausePreparation" => p["phase"] = json!("paused"),
            "recheckRuntime" => p["phase"] = json!("ready"),
            other => panic!("意外的环境命令 {other}"),
        }
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "text:还没有实例");
    press(&mut ui.app, "show-settings");
    wait(&mut ui.app, "id:settings");
    press(&mut ui.app, "toggle-sidebar");
    press(&mut ui.app, "all-instances");
    wait(&mut ui.app, "id:inventory");
    press(&mut ui.app, "workspace-next");
    wait(&mut ui.app, "text:准备 Android 环境");
    press(&mut ui.app, "license-image");
    wait(&mut ui.app, "text:镜像完整许可正文");
    press(&mut ui.app, "close-license");
    assert!(ui.commands.lock().unwrap().is_empty());
    press(&mut ui.app, "prepare-runtime");
    wait(&mut ui.app, "text:512 KiB / 1.0 MiB");
    wait(&mut ui.app, "id:current-component");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, "pause-preparation");
    wait(&mut ui.app, "text:同意并继续下载");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, "prepare-runtime");
    wait(&mut ui.app, "text:正在下载组件");
    ui.update(|view| {
        view["preparation"]["phase"] = json!("failed");
        view["preparation"]["error"] = json!("文件校验失败");
    });
    wait(&mut ui.app, "text:文件校验失败");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, "prepare-runtime");
    wait(&mut ui.app, "text:正在下载组件");
    ui.update(|view| view["preparation"]["phase"] = json!("paused"));
    wait(&mut ui.app, "text:下载已暂停");
    gone(&mut ui.app, "id:pending");
    press(&mut ui.app, "recheck-runtime");
    wait(&mut ui.app, "text:运行环境已就绪");
    press(&mut ui.app, "return-library");
    wait(&mut ui.app, "id:inventory");
    let commands = ui.commands.lock().unwrap();
    assert_eq!(
        commands
            .iter()
            .map(|c| c["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "prepareRuntime",
            "pausePreparation",
            "prepareRuntime",
            "prepareRuntime",
            "recheckRuntime"
        ]
    );
}

#[test]
fn full_verification_then_repair_requires_consent_for_the_reviewed_version_and_location() {
    let mut view = ready_view();
    let mut tool = image("tool", 34, true);
    tool["image"] = Value::Null;
    tool["name"] = json!("Android SDK Build-Tools");
    tool["version"] = json!("34.0.0");
    tool["location"] = json!("/Volumes/Windroid/build-tools/34.0.0");
    view["preparation"]["components"] = json!([tool]);
    let mut ui = Scripted::new(view, |cmd, view| {
        let preparation = &mut view["preparation"];
        match cmd["kind"].as_str().unwrap() {
            "verifyResource" => {
                assert_eq!(cmd["resourceId"], "tool");
                preparation["componentsReady"] = json!(false);
                preparation["phase"] = json!("failed");
                let target = &mut preparation["components"][0];
                target["integrity"] = json!("damaged");
                target["repairToken"] = json!("original-volume-and-tree");
                target["issue"] = json!("aapt 文件内容 SHA-256 不符");
                target["ready"] = json!(false);
            }
            "repairResource" => {
                assert_eq!(
                    cmd,
                    &json!({"kind":"repairResource","resourceId":"tool",
                    "location":"/Volumes/Windroid/build-tools/34.0.0","repairToken":"replacement-volume-and-tree","confirmed":true,"acceptedLicenses":true})
                );
                preparation["components"][0]["integrity"] = json!("verified");
                preparation["components"][0]["repairToken"] = Value::Null;
                preparation["components"][0]["ready"] = json!(true);
                preparation["components"][0]["issue"] = Value::Null;
                preparation["componentsReady"] = json!(true);
                preparation["phase"] = json!("ready");
            }
            _ => panic!("意外的版本操作 {cmd}"),
        }
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "show-environment");
    wait(&mut ui.app, "text:快速自检通过");
    press(&mut ui.app, "verify-tool");
    wait(&mut ui.app, "text:/aapt 文件内容/");
    gone(&mut ui.app, "id:prepare-runtime");
    press(&mut ui.app, "repair-tool");
    wait(&mut ui.app, "id:repair-review-tool >> text:/Volumes/");
    press(&mut ui.app, "cancel-repair-tool");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
    press(&mut ui.app, "repair-tool");
    // 同一挂载点直接换成第二个非空版本身份，也会撤销已经展开的确认。
    ui.update(|s| {
        s["preparation"]["components"][0]["repairToken"] = json!("replacement-volume-and-tree");
    });
    wait(&mut ui.app, "text:/目标卷或版本状态已变化/");
    gone(&mut ui.app, "id:confirm-repair-tool");
    press(&mut ui.app, "repair-tool");
    ui.update(|s| {
        s["preparation"]["components"][0]["repairToken"] = Value::Null;
    });
    wait(&mut ui.app, "text:/尚未确认目标卷及目录身份/");
    gone(&mut ui.app, "id:confirm-repair-tool");
    assert!(!ui.app.screen().enabled("id:repair-tool").unwrap());
    ui.update(|s| {
        s["preparation"]["components"][0]["repairToken"] = json!("replacement-volume-and-tree");
    });
    wait(&mut ui.app, "text:/目标卷或版本状态已变化/");
    press(&mut ui.app, "repair-tool");
    ui.update(|s| s["preparation"]["components"][0]["location"] = json!("/other/34.0.0"));
    gone(&mut ui.app, "id:confirm-repair-tool");
    ui.update(|s| {
        s["preparation"]["components"][0]["location"] =
            json!("/Volumes/Windroid/build-tools/34.0.0")
    });
    wait(&mut ui.app, "text:/Volumes/");
    gone(&mut ui.app, "id:confirm-repair-tool");
    press(&mut ui.app, "repair-tool");
    ui.update(|s| s["preparation"]["components"][0]["integrity"] = json!("conflict"));
    gone(&mut ui.app, "id:confirm-repair-tool");
    ui.update(|s| s["preparation"]["components"][0]["integrity"] = json!("damaged"));
    wait(&mut ui.app, "id:repair-tool");
    gone(&mut ui.app, "id:confirm-repair-tool");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
    press(&mut ui.app, "repair-tool");
    press(&mut ui.app, "confirm-repair-tool");
    wait(&mut ui.app, "text:已通过完整核验");
    assert_eq!(ui.commands.lock().unwrap().len(), 2);
}

#[test]
fn image_writes_report_space_permissions_and_unknown_files_without_offering_overwrite() {
    let mut view = ready_view();
    let mut full = image("full", 34, false);
    full["availableBytes"] = json!(1024);
    let mut readonly = image("ro", 35, false);
    readonly["writable"] = json!(false);
    let mut unknown = image("unknown", 36, false);
    unknown["integrity"] = json!("conflict");
    unknown["issue"] = json!("包含未知文件 extra.img");
    let mut resident = image("resident", 33, true);
    resident["writable"] = json!(false);
    view["preparation"]["images"] = json!([full, readonly, unknown, resident]);
    let mut ui = Scripted::new(view, |_, _| unreachable!());
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "show-environment");
    press(&mut ui.app, "environment-images");
    wait(&mut ui.app, "id:image-full >> text:目标卷空间不足");
    wait(&mut ui.app, "id:image-ro >> text:目标目录不可写");
    wait(&mut ui.app, "id:image-unknown >> text:/extra.img/");
    wait(&mut ui.app, "id:image-resident >> text:/只读位置/");
    assert!(ui.app.screen().enabled("id:use-resident").unwrap());
    assert!(ui.app.screen().enabled("id:verify-resident").unwrap());
    for id in ["full", "ro", "unknown"] {
        gone(&mut ui.app, &format!("id:download-{id}"));
        gone(&mut ui.app, &format!("id:repair-{id}"));
    }
    assert!(ui.commands.lock().unwrap().is_empty());
}

#[test]
fn images_require_separate_consent_selection_and_instance_creation() {
    let mut view = ready_view();
    view["instances"] = json!([]);
    view["selectedId"] = Value::Null;
    view["preparation"]["images"] = json!([image("r34", 34, true), image("r35", 35, false)]);
    let mut ui = Scripted::new(view, |cmd, v| {
        match cmd["kind"].as_str().unwrap() {
            "prepareImage" => {
                assert_eq!(cmd["imageId"], "r35");
                assert_eq!(cmd["acceptedLicenses"], true);
                v["preparation"]["currentComponent"] = json!("r35");
                v["preparation"]["phase"] = json!("downloading");
                v["preparation"]["images"][1]["downloadedBytes"] = json!(524288);
            }
            "createInstance" => {
                assert_eq!(cmd["imageId"], "r35");
                assert_eq!(cmd["image"], image("r35", 35, true)["image"]);
            }
            _ => panic!("意外命令 {cmd}"),
        }
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:inventory");
    press(&mut ui.app, "show-environment");
    press(&mut ui.app, "environment-images");
    wait(&mut ui.app, "id:use-r34");
    wait(&mut ui.app, "id:download-r35");
    assert!(ui.commands.lock().unwrap().is_empty());
    press(&mut ui.app, "download-r35");
    wait(&mut ui.app, "id:image-progress-r35");
    press(&mut ui.app, "all-instances");
    ui.update(|v| {
        v["preparation"]["phase"] = json!("ready");
        v["preparation"]["currentComponent"] = Value::Null;
        v["preparation"]["images"][1]["ready"] = json!(true);
        v["preparation"]["images"][1]["integrity"] = json!("verified");
    });
    wait(&mut ui.app, "id:workspace-next >> text:创建实例");
    gone(&mut ui.app, "id:pending");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
    press(&mut ui.app, "workspace-next");
    wait(&mut ui.app, "id:use-r35");
    gone(&mut ui.app, "id:create");
    press(&mut ui.app, "use-r34");
    wait(&mut ui.app, "id:create-form >> text:/Android API 34/");
    press(&mut ui.app, "change-image");
    press(&mut ui.app, "use-r35");
    wait(&mut ui.app, "id:create-form >> text:/Android API 35/");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
    wait(&mut ui.app, "id:hardware-fields");
    press(&mut ui.app, "create");
    gone(&mut ui.app, "id:pending");
    assert_eq!(ui.commands.lock().unwrap().len(), 2);
}

#[test]
fn selection_pins_one_of_two_revisions_of_the_same_image() {
    let mut view = ready_view();
    view["instances"] = json!([]);
    view["selectedId"] = Value::Null;
    let mut other = image("api34-r15", 34, true);
    other["version"] = json!("15");
    other["image"]["revision"] = json!(15);
    view["preparation"]["images"] = json!([image("api34-r14", 34, true), other]);
    let mut ui = Scripted::new(view, |cmd, _| {
        assert_eq!(cmd["kind"], "createInstance");
        assert_eq!(cmd["imageId"], "api34-r14");
        assert_eq!(cmd["image"], image("api34-r14", 34, true)["image"]);
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:inventory");
    press(&mut ui.app, "workspace-next");
    wait(&mut ui.app, "id:image-api34-r15 >> text:/修订版 15/");
    press(&mut ui.app, "use-api34-r14");
    wait(&mut ui.app, "id:create-form >> text:/修订版 14/");
    wait(&mut ui.app, "id:hardware-fields");
    press(&mut ui.app, "create");
    gone(&mut ui.app, "id:pending");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
}

#[test]
fn selected_download_advances_only_the_current_creation_flow() {
    let mut view = ready_view();
    view["instances"] = json!([]);
    view["selectedId"] = Value::Null;
    view["preparation"]["images"] = json!([image("r35", 35, false)]);
    let mut ui = Scripted::new(view, |cmd, v| {
        assert_eq!(
            cmd,
            &json!({"kind":"prepareImage","imageId":"r35","acceptedLicenses":true})
        );
        v["preparation"]["currentComponent"] = json!("r35");
        v["preparation"]["phase"] = json!("downloading");
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:inventory");
    press(&mut ui.app, "workspace-next");
    press(&mut ui.app, "download-r35");
    wait(&mut ui.app, "id:image-progress-r35");
    ui.update(|v| {
        v["preparation"]["phase"] = json!("ready");
        v["preparation"]["currentComponent"] = Value::Null;
        v["preparation"]["images"][0]["ready"] = json!(true);
        v["preparation"]["images"][0]["integrity"] = json!("verified");
    });
    wait(&mut ui.app, "id:create-form >> text:/Android API 35/");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
}

#[test]
fn local_image_zip_is_reviewed_before_import_and_unknown_files_explain_why() {
    let mut view = ready_view();
    view["preparation"]["images"] = json!([image("r34", 34, false)]);
    let mut ui = Scripted::new(view, |cmd, view| {
        assert_eq!(
            cmd,
            &json!({"kind":"importImage","imageId":"r34","path":"/tmp/archive.zip","acceptedLicenses":true})
        );
        view["preparation"]["images"][0]["ready"] = json!(true);
        view["preparation"]["images"][0]["integrity"] = json!("verified");
        json!({"status":"accepted"})
    });
    ui.channel.handle("windroid.pickResource", |call| {
        assert_eq!(call.data()["kind"], "imageZip");
        call.resolve(json!("/tmp/archive.zip"));
    });
    let first = std::sync::atomic::AtomicBool::new(true);
    ui.channel.handle("windroid.inspectImage", move |call| {
        if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
            call.reject("ZIP 摘要与官方清单不符");
        } else {
            call.resolve(json!({"imageId":"r34"}));
        }
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "show-environment");
    press(&mut ui.app, "environment-images");
    press(&mut ui.app, "pick-image-zip");
    wait(&mut ui.app, "text:/ZIP 摘要/");
    assert!(ui.commands.lock().unwrap().is_empty());
    press(&mut ui.app, "pick-image-zip");
    wait(&mut ui.app, "id:image-import-preview");
    press(&mut ui.app, "cancel-import");
    assert!(ui.commands.lock().unwrap().is_empty());
    press(&mut ui.app, "pick-image-zip");
    wait(&mut ui.app, "id:confirm-import");
    press(&mut ui.app, "confirm-import");
    wait(&mut ui.app, "id:use-r34");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
}

#[test]
fn guide_skips_each_step_and_returns_to_its_origin_while_transfers_continue() {
    let mut view = ready_view();
    view["showWelcome"] = json!(true);
    view["instances"] = json!([]);
    view["selectedId"] = Value::Null;
    view["preparation"]["componentsReady"] = json!(false);
    view["preparation"]["phase"] = json!("missing");
    view["preparation"]["components"] = json!([{"id":"emulator","name":"Android Emulator","version":"37",
        "sizeBytes":1024, "location":"/tmp/tools", "license":"SDK", "ready":false,"integrity":"missing","repairToken":null,"requiredBytes":3072,"writable":true,"availableBytes":65536}]);
    let mut ui = Scripted::new(view, |cmd, v| {
        assert_eq!(
            cmd,
            &json!({"kind":"prepareRuntime","acceptedLicenses":true})
        );
        v["preparation"]["phase"] = json!("downloading");
        v["preparation"]["currentComponent"] = json!("emulator");
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "text:欢迎使用 Windroid");
    press(&mut ui.app, "guide-back");
    wait(&mut ui.app, "id:inventory");
    press(&mut ui.app, "guide-from-inventory");
    press(&mut ui.app, "guide-begin");
    wait(&mut ui.app, "id:components");
    assert!(bounds(&ui.app, "guide-skip-components").bottom().0 < 520.0);
    press(&mut ui.app, "prepare-runtime");
    wait(&mut ui.app, "id:component-progress");
    press(&mut ui.app, "guide-skip-components");
    wait(&mut ui.app, "id:workspace-next >> text:查看准备进度");
    ui.update(|v| {
        v["preparation"]["phase"] = json!("ready");
        v["preparation"]["componentsReady"] = json!(true);
        v["preparation"]["currentComponent"] = Value::Null;
    });
    wait(&mut ui.app, "id:workspace-next >> text:创建实例");
    press(&mut ui.app, "show-settings");
    press(&mut ui.app, "settings-logs");
    press(&mut ui.app, "show-guide");
    press(&mut ui.app, "guide-begin");
    wait(&mut ui.app, "id:image-list");
    press(&mut ui.app, "cancel-image-selection");
    wait(&mut ui.app, "id:workspace-scroll >> text:应用日志");
    press(&mut ui.app, "show-guide");
    press(&mut ui.app, "guide-begin");
    press(&mut ui.app, "use-r34");
    wait(&mut ui.app, "id:create-form");
    press(&mut ui.app, "cancel-create");
    wait(&mut ui.app, "id:workspace-scroll >> text:应用日志");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
    assert_eq!(ui.first_pages.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn creation_reply_does_not_redirect_a_new_guide_session() {
    let mut ui = Scripted::new(ready_view(), |_, _| unreachable!());
    let (submitted, waiting) = mpsc::channel();
    ui.channel.handle("windroid.command", move |call| {
        assert_eq!(call.data()["kind"], "createInstance");
        submitted.send(call.into_reply().unwrap()).unwrap();
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "show-settings");
    press(&mut ui.app, "show-guide");
    press(&mut ui.app, "guide-begin");
    press(&mut ui.app, "use-r34");
    wait(&mut ui.app, "id:hardware-fields");
    press(&mut ui.app, "create");
    let request = waiting.recv_timeout(WAIT).unwrap();
    press(&mut ui.app, "show-settings");
    press(&mut ui.app, "show-guide");
    wait(&mut ui.app, "id:guide-begin");
    ui.update(|v| {
        v["selectedId"] = json!("created");
        v["instances"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"created","name":"Android 2","state":"stopped"}));
    });
    request.resolve(json!({"status":"accepted"}));
    gone(&mut ui.app, "id:pending");
    wait(&mut ui.app, "id:guide-begin");
}

fn storage(runtime: &str, images: &str) -> Value {
    json!({"home":"/home", "runtimeRoot":runtime, "imageRoot":images,
        "defaultRuntimeRoot":"/home/runtime", "defaultImageRoot":"/home/system-images", "cleanupNotes":[],
        "runtimeIsDefault":runtime=="/home/runtime", "imageIsDefault":images=="/home/system-images"})
}

fn storage_plan(change: &Value, id: usize) -> Value {
    json!({"previewId":id, "mode":change["mode"],"cleanup":change["cleanup"],
        "runtimeRoot":change["runtimeRoot"],"imageRoot":change["imageRoot"], "versions":[],
        "requiredBytes":0,"availableBytes":null,"affectedInstances":[],"blockers":[]})
}

#[test]
fn storage_reviews_each_version_and_cleanup_choice_then_restores_each_root_independently() {
    let mut view = ready_view();
    view["storage"] = storage("/old", "/initial-images");
    let mut ui = Scripted::new(view, |cmd, v| {
        assert_eq!(cmd["kind"], "applyStorage");
        match cmd["plan"]["previewId"].as_u64().unwrap() {
            4 => v["storage"]["imageRoot"] = json!("/images"),
            7 => {
                // 同一实际目录经原生面板返回时可能采用另一种路径拼写。
                v["storage"]["runtimeRoot"] = json!("/private/home/runtime/");
                v["storage"]["runtimeIsDefault"] = json!(true);
            }
            id => panic!("无法识别此次确认的预览 {id}"),
        }
        json!({"status":"accepted"})
    });
    ui.channel.handle("windroid.pickResource", |call| {
        assert_eq!(call.data()["kind"], "imageDirectory");
        call.resolve(json!("/images"));
    });
    let count = std::sync::atomic::AtomicUsize::new(0);
    ui.channel.handle("windroid.previewStorage", move |call| {
        let id = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let change = call.data();
        assert_eq!(change["expected"], json!({"runtimeRoot":"/old","imageRoot":if id >= 5 {"/images"} else {"/initial-images"}}));
        assert_eq!(change["runtimeRoot"], if id >= 6 {"/home/runtime"} else {"/old"});
        assert_eq!(change["imageRoot"], if id == 5 {"/home/system-images"} else {"/images"});
        assert_eq!(change["mode"], match id {2..=4=>"migrate",7=>"future",_=>"use"});
        assert_eq!(change["cleanup"], id == 4);
        let mut plan = storage_plan(change, id);
        if id <= 4 {
            plan["versions"] = json!((0..9).map(|n| json!({"id":format!("r{n}"),"name":format!("版本 {n}"),"source":format!("/old/r{n}"),"target":format!("/images/r{n}"),"bytes":1024,
                "status":if id==1 {"missing"} else {"copy"}, "cleanup":id==4,"downloadable":false,"detail":""})).collect::<Vec<_>>());
            plan["affectedInstances"] = json!(["工作机 · API 34 · r14"]);
            plan["requiredBytes"] = json!(9216); plan["availableBytes"] = json!(1048576);
            plan["blockers"] = match id { 1=>json!(["目标缺少工作机使用的 r14"]),2=>json!(["请先停止「工作机」"]),_=>json!([]) };
        }
        call.resolve(plan);
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "show-settings");
    press(&mut ui.app, "settings-storage");
    press(&mut ui.app, "choose-image-directory");
    wait(&mut ui.app, "text:目标缺少工作机使用的 r14");
    assert!(!ui.app.screen().enabled("id:storage-apply").unwrap());
    press(&mut ui.app, "storage-mode-migrate");
    wait(&mut ui.app, "text:请先停止「工作机」");
    press(&mut ui.app, "storage-next");
    wait(&mut ui.app, "text:版本 8");
    press(&mut ui.app, "storage-see-instances");
    wait(&mut ui.app, "id:inventory");
    press(&mut ui.app, "show-settings");
    wait(&mut ui.app, "text:切换后保留源版本。");
    press(&mut ui.app, "storage-toggle-cleanup");
    wait(&mut ui.app, "text:迁移并清理已确认源版本");
    wait(
        &mut ui.app,
        "id:storage-version-r0 >> text:切换成功后，复核并清理此源版本。",
    );
    assert!(ui.commands.lock().unwrap().is_empty());
    press(&mut ui.app, "storage-apply");
    wait(&mut ui.app, "id:reset-image-directory");
    press(&mut ui.app, "reset-image-directory");
    wait(&mut ui.app, "id:storage-preview");
    press(&mut ui.app, "cancel-storage");
    press(&mut ui.app, "reset-runtime-directory");
    wait(&mut ui.app, "id:storage-preview");
    press(&mut ui.app, "storage-mode-future");
    wait(&mut ui.app, "text:确认下载位置");
    press(&mut ui.app, "storage-apply");
    wait(&mut ui.app, "id:choose-runtime-directory");
    gone(&mut ui.app, "id:reset-runtime-directory");
    assert_eq!(ui.commands.lock().unwrap().len(), 2);
}

fn temporary_ui_dir(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "windroid-ui-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&path).unwrap();
    path
}

fn real_resource_ui(home: &std::path::Path) -> (Headless, Arc<host::Library>) {
    let config = config();
    let (windows, _) = mpsc::channel();
    let (_events, receiver) = mpsc::channel();
    let library = host::Library::attach(
        config.host.clone(),
        runtime::Layout { root: home.into() },
        false,
        windows,
        receiver,
    )
    .unwrap();
    (render(config), library)
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn replacing_the_directory_after_review_rejects_a_real_host_repair_without_writing() {
    let temp = temporary_ui_dir("repair");
    let home = temp.join("workspace");
    runtime::workspace::open(&home).unwrap();
    let version = home.join("runtime/scrcpy-server/5.0.1");
    std::fs::create_dir_all(&version).unwrap();
    std::fs::write(version.join("scrcpy-server.jar"), b"first copy").unwrap();
    let (mut app, library) = real_resource_ui(&home);
    wait(&mut app, "id:show-environment");
    press(&mut app, "show-environment");
    press(&mut app, "repair-scrcpy-server");
    wait(&mut app, "id:confirm-repair-scrcpy-server");
    let previous = temp.join("saved-original");
    std::fs::rename(&version, &previous).unwrap();
    std::fs::create_dir(&version).unwrap();
    std::fs::write(version.join("scrcpy-server.jar"), b"other copy").unwrap();
    // 没有向界面发送刷新事件，确认携带的是先前看到的对象。
    press(&mut app, "confirm-repair-scrcpy-server");
    wait(&mut app, "text:/重新确认/");
    gone(&mut app, "id:pending");
    assert_eq!(
        std::fs::read(version.join("scrcpy-server.jar")).unwrap(),
        b"other copy"
    );
    assert_eq!(
        std::fs::read(previous.join("scrcpy-server.jar")).unwrap(),
        b"first copy"
    );
    assert_eq!(
        std::fs::read_dir(version.parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    library.shutdown();
    std::fs::remove_dir_all(temp).unwrap();
}

#[test]
fn restoring_a_default_through_another_spelling_uses_the_real_host_directory_identity() {
    use runtime::storage::{StorageLocations, SwitchMode, apply, preview_switch};
    let temp = temporary_ui_dir("default-root");
    let physical = temp.join("physical");
    std::fs::create_dir(&physical).unwrap();
    std::os::unix::fs::symlink(&physical, temp.join("alias")).unwrap();
    let home = temp.join("alias/workspace");
    runtime::workspace::open(&home).unwrap();
    let target = StorageLocations {
        runtime_root: temp.join("custom"),
        ..StorageLocations::defaults(&home)
    };
    apply(
        &home,
        &preview_switch(&home, &target, SwitchMode::Future, false).unwrap(),
    )
    .unwrap();
    let (mut app, library) = real_resource_ui(&home);
    wait(&mut app, "id:show-settings");
    press(&mut app, "show-settings");
    press(&mut app, "settings-storage");
    press(&mut app, "reset-runtime-directory");
    wait(&mut app, "id:storage-mode-future");
    press(&mut app, "storage-mode-future");
    wait(&mut app, "text:确认下载位置");
    press(&mut app, "storage-apply");
    wait(&mut app, "id:choose-runtime-directory");
    gone(&mut app, "id:reset-runtime-directory");
    gone(&mut app, "id:reset-image-directory");
    let saved = StorageLocations::load(&home).unwrap();
    assert_ne!(saved.runtime_root, home.join("runtime"));
    assert!(runtime::storage::same_location(
        &saved.runtime_root,
        &home.join("runtime")
    ));
    assert_eq!(saved.image_root, home.join("system-images"));
    library.shutdown();
    std::fs::remove_dir_all(temp).unwrap();
}

#[test]
fn candidate_download_preserves_the_bound_root_until_the_next_preview_is_confirmed() {
    let mut view = ready_view();
    view["storage"] = storage("/tools", "/original");
    let mut ui = Scripted::new(view, |cmd, v| {
        assert_eq!(
            cmd,
            &json!({"kind":"prepareCandidateImage","resourceId":"r34","previewId":1,"acceptedLicenses":true})
        );
        let p = &mut v["preparation"];
        p["currentComponent"] = json!("r34");
        p["phase"] = json!("downloading");
        p["candidateTarget"] = json!("/target/r14");
        p["candidateDownloadedBytes"] = json!(524288);
        json!({"status":"accepted"})
    });
    ui.channel.handle("windroid.pickResource", |call| {
        call.resolve(json!("/target"))
    });
    let first = std::sync::atomic::AtomicBool::new(true);
    ui.channel.handle("windroid.previewStorage", move |call| {
        let missing = first.swap(false, std::sync::atomic::Ordering::Relaxed);
        let mut plan = storage_plan(call.data(), if missing { 1 } else { 2 });
        plan["versions"] = json!([{"id":"r34", "name":"Android 14 · r14", "source":"/original/r14", "target":"/target/r14",
            "status":if missing {"missing"} else {"reuse"}, "bytes":1048576, "downloadable":missing,
            "downloadRequiredBytes":3145728,"downloadAvailableBytes":1073741824,"downloadWritable":true, "cleanup":false, "detail":""}]);
        plan["blockers"] = if missing {json!(["目标缺少 r14"])} else {json!([])};
        call.resolve(plan);
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "show-settings");
    press(&mut ui.app, "settings-storage");
    press(&mut ui.app, "choose-image-directory");
    wait(&mut ui.app, "id:download-candidate-r34");
    wait(&mut ui.app, "text:/下载及安装需额外空间 · 3.0 MiB/");
    assert!(ui.commands.lock().unwrap().is_empty());
    press(&mut ui.app, "download-candidate-r34");
    wait(&mut ui.app, "text:512 KiB / 1.0 MiB");
    ui.update(|v| {
        let p = &mut v["preparation"];
        p["phase"] = json!("ready");
        p["candidateDownloadedBytes"] = json!(1048576);
    });
    wait(&mut ui.app, "text:目标镜像已准备，请重新预览。");
    gone(&mut ui.app, "id:download-candidate-r34");
    gone(&mut ui.app, "id:pause-candidate");
    assert!(!ui.app.screen().enabled("id:storage-apply").unwrap());
    press(&mut ui.app, "storage-recheck");
    wait(&mut ui.app, "text:可原地复用");
    gone(&mut ui.app, "id:download-candidate-r34");
    assert!(ui.app.screen().enabled("id:storage-apply").unwrap());
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
    assert_eq!(
        ui.state.lock().unwrap()["storage"]["imageRoot"],
        "/original"
    );
}

#[test]
fn preview_from_an_old_workspace_view_cannot_restore_another_roots_location() {
    let mut view = ready_view();
    view["storage"] = storage("/old", "/local");
    let mut ui = Scripted::new(view, |_, _| unreachable!());
    ui.channel.handle("windroid.pickResource", |call| {
        call.resolve(json!("/images"))
    });
    let state = ui.state.clone();
    let first = std::sync::atomic::AtomicBool::new(true);
    ui.channel.handle("windroid.previewStorage", move |call| {
        if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
            assert_eq!(call.data()["expected"], json!({"runtimeRoot":"/old","imageRoot":"/local"}));
            state.lock().unwrap()["storage"] = storage("/fresh","/local");
            call.reject("原存储位置已改变");
        } else {
            assert_eq!(call.data(), &json!({"runtimeRoot":"/fresh","imageRoot":"/images","expected":{"runtimeRoot":"/fresh","imageRoot":"/local"}, "mode":"use","cleanup":false}));
            let plan = storage_plan(call.data(), 2);
            call.resolve(plan);
        }
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "show-settings");
    press(&mut ui.app, "settings-storage");
    press(&mut ui.app, "choose-image-directory");
    wait(&mut ui.app, "text:存储位置已更改，请重新选择目标文件夹。");
    gone(&mut ui.app, "id:retry-storage-preview");
    press(&mut ui.app, "choose-image-directory");
    wait(&mut ui.app, "id:storage-preview");
    assert!(ui.commands.lock().unwrap().is_empty());
}

#[test]
fn drops_require_one_local_apk_and_render_the_install_rejection() {
    const DIAGNOSTIC: &str = "Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Package com.example.reader signatures do not match previously installed version]";
    let mut ui = Scripted::new(ready_view(), |_, _| {
        json!(Ack::Rejected {
            reason: Rejection::Error {
                code: ErrorCode::SignatureMismatch,
                detail: DIAGNOSTIC.into()
            }
        })
    });
    wait(&mut ui.app, "id:library");
    for uri in [
        "https://example.org/file.apk",
        "file:///tmp/file.txt",
        "file://other-host/file.apk",
    ] {
        ui.app.drop_files("id:apk-drop", vec![uri.into()]).unwrap();
        wait(&mut ui.app, "text:请选择本机的 .apk 文件");
    }
    ui.app
        .drop_files(
            "id:apk-drop",
            vec!["file:///tmp/a.apk".into(), "file:///tmp/b.apk".into()],
        )
        .unwrap();
    wait(&mut ui.app, "text:每次请拖入一个 APK 文件");
    assert!(ui.commands.lock().unwrap().is_empty());
    ui.app
        .drop_files("id:apk-drop", vec!["file:///tmp/my%20app.APK".into()])
        .unwrap();
    wait(&mut ui.app, "text:APK 签名与已安装的应用不一致。");
    gone(&mut ui.app, "id:install-diagnostic");
    press(&mut ui.app, "toggle-install-diagnostic");
    wait(&mut ui.app, &format!("text:{DIAGNOSTIC}"));
    press(&mut ui.app, "toggle-install-diagnostic");
    gone(&mut ui.app, "id:install-diagnostic");
    assert_eq!(
        *ui.commands.lock().unwrap(),
        [json!({"kind":"install","instanceId":"a","apkPath":"/tmp/my app.APK"})]
    );
}

/// 原生文件面板先返回受理，安装结果稍后作为快照事件到达。
#[test]
fn apk_picker_shows_the_classified_reason_and_exact_android_diagnostic() {
    let mut ui = Scripted::new(ready_view(), |cmd, _| {
        assert_eq!(cmd, &json!({"kind":"addApk","instanceId":"a"}));
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:add-apk");
    for (index, (code, raw, expected)) in [
        (
            ErrorCode::OutOfSpace,
            "Requested internal only, but not enough space",
            "实例存储空间不足。",
        ),
        (
            ErrorCode::InvalidApk,
            "Failure [INSTALL_PARSE_FAILED_NO_CERTIFICATES]",
            "APK 文件无效，请选择完整且已签名的安装包。",
        ),
        (
            ErrorCode::AdbFailure,
            "Failure [INSTALL_FAILED_NO_MATCHING_ABIS]",
            "APK 不支持此实例的处理器架构。",
        ),
        (
            ErrorCode::AdbFailure,
            "Failure [INSTALL_FAILED_TEST_ONLY]",
            "Android 拒绝安装这个 APK。",
        ),
        (
            ErrorCode::AdbFailure,
            "安装后无法确认 com.example.reader 的版本：设备断开",
            "无法确认 APK 安装成功。",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        press(&mut ui.app, "add-apk");
        gone(&mut ui.app, "id:pending");
        gone(&mut ui.app, "id:notice");
        ui.update(|v| v["operation"] = json!({"instanceId":"a","seq":index+1,"message":"APK 安装未完成","failed":true,
            "installError":{"code":code,"detail":raw}}));
        wait(&mut ui.app, &format!("text:{expected}"));
        gone(&mut ui.app, "id:install-diagnostic");
        press(&mut ui.app, "toggle-install-diagnostic");
        wait(&mut ui.app, &format!("text:{raw}"));
    }
    assert_eq!(ui.commands.lock().unwrap().len(), 5);
}

#[test]
fn a_delayed_install_keeps_its_own_error_while_another_instance_is_in_use() {
    let mut view = ready_view();
    view["instances"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"b","name":"娱乐机","state":"ready"}));
    let stale = "APK 已安装并核实版本";
    view["operation"] = json!({"instanceId":"a","failed":false,"message":stale,"seq":7});
    let mut ui = Scripted::new(view, |_, _| unreachable!());
    let state = ui.state.clone();
    let (started, waiting) = mpsc::channel();
    ui.channel.handle("windroid.command", move |call| {
        match call.data()["kind"].as_str().unwrap() {
            "install" => {
                assert_eq!(call.data(), &json!({"kind":"install","instanceId":"a","apkPath":"/tmp/reader.apk"}));
                started.send(call.into_reply().unwrap()).unwrap();
            }
            "selectInstance" => {
                state.lock().unwrap()["selectedId"] = call.data()["instanceId"].clone();
                call.resolve(json!({"status":"accepted"}));
            }
            "refreshApps" => {
                assert_eq!(call.data()["instanceId"], "b");
                call.resolve(json!({"status":"rejected","reason":{"kind":"error","detail":"软件包服务离线"}}));
            }
            kind => panic!("意外的命令 {kind}"),
        }
    });
    wait(&mut ui.app, "id:library");
    ui.app
        .drop_files("id:apk-drop", vec!["file:///tmp/reader.apk".into()])
        .unwrap();
    let install = waiting.recv_timeout(WAIT).unwrap();
    wait(&mut ui.app, "id:pending");
    gone(&mut ui.app, &format!("text:{stale}"));
    press(&mut ui.app, "instance-b");
    gone(&mut ui.app, "id:pending");
    assert_eq!(ui.state.lock().unwrap()["selectedId"], "b");
    const DIAGNOSTIC: &str = "Failure [INSTALL_FAILED_VERSION_DOWNGRADE]";
    install.resolve(json!(Ack::Rejected {
        reason: Rejection::Error {
            code: ErrorCode::VersionDowngrade,
            detail: DIAGNOSTIC.into()
        }
    }));
    press(&mut ui.app, "refresh-apps");
    wait(&mut ui.app, "text:刷新应用失败 · 软件包服务离线");
    gone(&mut ui.app, "id:toggle-install-diagnostic");
    press(&mut ui.app, "instance-a");
    wait(&mut ui.app, "text:APK 版本比已安装的版本低。");
    gone(&mut ui.app, "text:刷新应用失败 · 软件包服务离线");
    press(&mut ui.app, "toggle-install-diagnostic");
    wait(&mut ui.app, &format!("text:{DIAGNOSTIC}"));
    press(&mut ui.app, "instance-b");
    gone(&mut ui.app, "id:install-diagnostic");
    press(&mut ui.app, "instance-a");
    wait(&mut ui.app, "text:查看原始诊断");
    gone(&mut ui.app, "id:install-diagnostic");
}

#[test]
fn destructive_app_menu_actions_require_the_original_confirmation() {
    let mut view = ready_view();
    view["groups"] = json!([{"id":"installed","name":"已安装","count":1,"apps":[{
        "package":READER,"label":"阅读器","system":false,"launchable":true,"actions":[
            {"kind":"clearData","label":"清除数据","enabled":true,"destructive":true}]}]}]);
    let mut ui = Scripted::new(view, |_, _| json!({"status":"accepted"}));
    wait(&mut ui.app, "id:menu-com.example.reader");
    press(&mut ui.app, "menu-com.example.reader");
    choose(&mut ui.app, "清除数据");
    wait(&mut ui.app, "text:清除数据 · 阅读器");
    press(&mut ui.app, "cancel-confirmation");
    assert!(ui.commands.lock().unwrap().is_empty());
    press(&mut ui.app, "menu-com.example.reader");
    choose(&mut ui.app, "清除数据");
    press(&mut ui.app, "confirm-action");
    wait(&mut ui.app, "text:已提交清除数据请求");
    assert_eq!(
        *ui.commands.lock().unwrap(),
        [json!({"kind":"clearData","instanceId":"a","package":READER})]
    );
}

#[test]
fn catalog_reports_loading_failure_and_verified_empty_separately() {
    let mut view = ready_view();
    view["catalogStatus"] = json!({"kind":"pending"});
    let mut ui = Scripted::new(view, |cmd, view| {
        assert_eq!(cmd, &json!({"kind":"refreshApps","instanceId":"a"}));
        view["catalogStatus"] = json!({"kind":"ready"});
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "text:正在读取应用…");
    gone(&mut ui.app, "id:empty-library");
    ui.update(|v| v["capabilities"]["refreshApps"] = json!(false));
    wait(&mut ui.app, "text:实例就绪后将读取应用列表。");
    gone(&mut ui.app, "id:empty-library");
    ui.update(|v| {
        v["capabilities"]["refreshApps"] = json!(true);
        v["groups"] = json!([{"id":"installed","name":"已安装","count":1,"apps":[{"package":READER,"label":"阅读器","launchable":true,"system":false,"actions":[]}]}]);
    });
    wait(&mut ui.app, "id:app-com.example.reader");
    wait(&mut ui.app, "id:catalog-pending");
    ui.update(|v| v["catalogStatus"] = json!({"kind":"failed","message":"软件包服务未就绪"}));
    wait(&mut ui.app, "text:应用列表读取失败 · 软件包服务未就绪");
    wait(&mut ui.app, "id:app-com.example.reader");
    ui.update(|v| v["groups"] = json!([]));
    gone(&mut ui.app, "id:app-com.example.reader");
    gone(&mut ui.app, "id:empty-library");
    press(&mut ui.app, "refresh-apps");
    wait(&mut ui.app, "id:empty-library");
    gone(&mut ui.app, "id:catalog-error");
}

#[test]
fn frozen_instance_resumes_via_start_and_uses_the_saved_id() {
    let mut view = ready_view();
    view["instances"][0]["state"] = json!("stopped");
    view["instances"][0]["frozen"] = json!(true);
    view["state"] = json!("stopped");
    view["frozen"] = json!(true);
    view["capabilities"] = json!({"start":true,"delete":true});
    let mut ui = Scripted::new(view, |cmd, view| {
        assert_eq!(cmd, &json!({"kind":"start","instanceId":"a"}));
        *view = ready_view();
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "text:已冻结 · 应用与文件已保存");
    wait(&mut ui.app, "text:恢复实例");
    press(&mut ui.app, "start");
    wait(&mut ui.app, "id:stop");
    gone(&mut ui.app, "text:恢复实例");
}

#[test]
fn recommended_template_can_be_overridden_before_creation() {
    let mut view = ready_view();
    view["instances"] = json!([]);
    view["selectedId"] = Value::Null;
    view["recommendedTemplate"] = json!("compact-720");
    let mut ui = Scripted::new(view, |cmd, v| {
        v["selectedId"] = json!("new");
        v["instances"] = json!([{"id":"new","name":cmd["name"],"state":"stopped"}]);
        v["operation"] = json!({"instanceId":"new","message":"实例已创建","failed":false,"seq":1});
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:inventory");
    press(&mut ui.app, "workspace-next");
    wait(&mut ui.app, "id:use-r34");
    press(&mut ui.app, "use-r34");
    wait(&mut ui.app, "text:/紧凑设备/");
    press(&mut ui.app, "choose-template");
    choose(&mut ui.app, "平板 · 1920 × 1200");
    ui.app.type_text("id:instance-name", " 工作").unwrap();
    ui.update(|v| v["recommendedTemplate"] = json!("phone-1080"));
    ui.app
        .set_viewport(Size::new(Points(480.0), Points(650.0)))
        .unwrap();
    wait(&mut ui.app, "text:/平板/");
    wait(&mut ui.app, "id:hardware-fields");
    press(&mut ui.app, "create");
    wait(&mut ui.app, "text:实例已创建");
    gone(&mut ui.app, "text:创建实例请求已受理");
    assert_eq!(
        *ui.commands.lock().unwrap(),
        [
            json!({"kind":"createInstance","template":"tablet-1200","name":"我的 Android 工作","imageId":"r34","image":image("r34",34,true)["image"],
                "cpuCores":2,"ramMiB":2560,"gpuMode":"host","diskGiB":8})
        ]
    );
}

#[test]
fn create_rechecks_the_selected_template_and_accepts_exact_custom_units() {
    let mut ui = Scripted::new(ready_view(), |cmd, _| {
        assert_eq!(cmd["kind"], "createInstance");
        assert_eq!(cmd["cpuCores"], 3);
        assert_eq!(cmd["ramMiB"], 4097);
        assert_eq!(cmd["diskGiB"], 11);
        assert_eq!(cmd["gpuMode"], "batch");
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "new-instance");
    press(&mut ui.app, "use-r34");
    wait(&mut ui.app, "id:choose-ram >> text:/2.5 GiB/");
    press(&mut ui.app, "choose-template");
    choose(&mut ui.app, "4K 电视 · 3840 × 2160");
    wait(&mut ui.app, "id:choose-ram >> text:/4 GiB/");
    for (field, invalid, valid) in [
        ("cpu", "0", "3"),
        ("ram", "4095", "4097"),
        ("disk", "41", "11"),
    ] {
        press(&mut ui.app, &format!("choose-{field}"));
        choose(&mut ui.app, "自定义…");
        let input = format!("id:custom-{field}");
        ui.app.clear(&input).unwrap();
        ui.app.type_text(&input, invalid).unwrap();
        wait(&mut ui.app, "id:hardware-issue");
        assert!(!ui.app.screen().enabled("id:create").unwrap());
        ui.app.clear(&input).unwrap();
        ui.app.type_text(&input, valid).unwrap();
        gone(&mut ui.app, "id:hardware-issue");
    }
    press(&mut ui.app, "choose-graphics");
    choose(&mut ui.app, "硬件加速（Vulkan 批处理）");
    press(&mut ui.app, "create");
    gone(&mut ui.app, "id:pending");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
}

#[test]
fn late_hardware_limits_cannot_enable_a_creation_on_a_full_volume() {
    let mut ui = Scripted::new(ready_view(), |_, _| unreachable!());
    let (requested, replies) = mpsc::channel();
    ui.channel.handle("windroid.hardwareLimits", move |call| {
        requested.send(call.into_reply().unwrap()).unwrap();
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "new-instance");
    press(&mut ui.app, "use-r34");
    let stale = replies.recv_timeout(WAIT).unwrap();
    press(&mut ui.app, "choose-template");
    choose(&mut ui.app, "4K 电视 · 3840 × 2160");
    let current = replies.recv_timeout(WAIT).unwrap();
    let mut full = hardware_limits(&json!({"template":"tv-4k"}));
    full["maxDiskGiB"] = json!(5);
    full["defaultDiskGiB"] = Value::Null;
    current.resolve(full);
    wait(&mut ui.app, "text:/不足 6 GiB/");
    stale.resolve(hardware_limits(&Value::Null));
    ui.app.idle(Duration::from_millis(50)).unwrap();
    assert!(!ui.app.screen().enabled("id:create").unwrap());
    press(&mut ui.app, "retry-hardware");
    let mut small = hardware_limits(&json!({"template":"tv-4k"}));
    small["maxDiskGiB"] = json!(7);
    small["defaultDiskGiB"] = json!(6);
    replies.recv_timeout(WAIT).unwrap().resolve(small);
    wait(&mut ui.app, "id:choose-disk >> text:/6 GiB/");
    press(&mut ui.app, "choose-disk");
    let options: Vec<_> = ui
        .app
        .menu()
        .unwrap()
        .into_iter()
        .map(|i| i.label)
        .collect();
    assert_eq!(options, ["6 GiB", "自定义…"]);
    assert!(ui.commands.lock().unwrap().is_empty());
}

#[test]
fn instance_settings_edit_only_after_process_exit_and_restart_the_selected_session() {
    let mut view = ready_view();
    view["instances"][0]["config"] = json!({
        "template":"compact-720","image":image("r34",34,true)["image"],"cpuCores":2,"ramMiB":1536,
        "screenWidth":1280,"screenHeight":720,"diskBytes":8589934592_u64,"graphics":"host"
    });
    view["instances"][0]["state"] = json!("failed");
    view["instances"][0]["processAlive"] = json!(true);
    let mut ui = Scripted::new(view, |cmd, view| {
        assert_eq!(cmd["instanceId"], "a");
        match cmd["kind"].as_str() {
            Some("updateHardware") => {
                assert_eq!(
                    cmd,
                    &json!({"kind":"updateHardware","instanceId":"a","cpuCores":4,"ramMiB":1536,"gpuMode":"software"})
                );
                view["instances"][0]["config"]["cpuCores"] = cmd["cpuCores"].clone();
                view["instances"][0]["config"]["graphics"] = cmd["gpuMode"].clone();
            }
            Some("coldRestart") => {
                view["state"] = json!("ready");
                view["instances"][0]["state"] = json!("ready");
                view["instances"][0]["processAlive"] = json!(true);
                view["instances"][0]["observed"] = json!({"cpuCores":4,"ramMiB":2560});
                view["capabilities"] = json!({"launchApp":true,"stop":true});
            }
            Some("restart") => {
                return json!({"status":"rejected","reason":{"kind":"error","detail":"本机内存压力过高"}});
            }
            other => panic!("意外命令 {other:?}"),
        }
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "all-instances");
    press(&mut ui.app, "manage-a");
    gone(&mut ui.app, "id:edit-hardware");
    ui.update(|v| {
        v["state"] = json!("stopped");
        v["instances"][0]["state"] = json!("stopped");
        v["instances"][0]["processAlive"] = json!(false);
        v["capabilities"] = json!({"start":true});
    });
    wait(&mut ui.app, "id:edit-hardware");
    press(&mut ui.app, "edit-hardware");
    wait(&mut ui.app, "id:hardware-editor >> text:/保留原设/");
    gone(&mut ui.app, "id:choose-disk");
    press(&mut ui.app, "choose-cpu");
    choose(&mut ui.app, "4 核");
    press(&mut ui.app, "choose-graphics");
    choose(&mut ui.app, "软件渲染");
    press(&mut ui.app, "save-hardware");
    gone(&mut ui.app, "id:hardware-editor");
    wait(&mut ui.app, "text:图形渲染 · 软件渲染");
    press(&mut ui.app, "manage-more");
    gone(&mut ui.app, "id:restart-instance");
    press(&mut ui.app, "cold-restart");
    press(&mut ui.app, "confirm-action");
    wait(&mut ui.app, "text:观测到的 CPU · 4 核     内存 · 2560 MiB");
    gone(&mut ui.app, "id:edit-hardware");
    press(&mut ui.app, "restart-instance");
    press(&mut ui.app, "confirm-action");
    wait(&mut ui.app, "text:/本机内存压力过高/");
    wait(&mut ui.app, "id:stop");
    press(&mut ui.app, "manage-back");
    wait(&mut ui.app, "id:row-a >> text:4 核");
    wait(&mut ui.app, "id:row-a >> text:1536 MiB");
    assert_eq!(ui.commands.lock().unwrap().len(), 3);
}

#[test]
fn spec_controls_remain_reachable_in_the_scroll_view_at_both_window_sizes() {
    fn reach(app: &mut Headless, width: f32, height: f32, ids: &[&str]) {
        for id in ids {
            let before = bounds(app, id);
            if before.top().0 < 0.0 || before.bottom().0 > height {
                let distance = before.top().0 - height / 2.0;
                app.scroll("id:workspace-scroll", Points(distance)).unwrap();
            }
            let control = bounds(app, id);
            assert!(
                control.top().0 >= 0.0 && control.bottom().0 <= height,
                "{id}: {control:?}"
            );
            assert!(
                control.left().0 >= 0.0 && control.right().0 <= width,
                "{id}: {control:?}"
            );
        }
    }
    for (width, height) in [(1024.0, 720.0), (480.0, 520.0)] {
        let mut view = ready_view();
        view["instances"][0]["state"] = json!("stopped");
        view["instances"][0]["config"] = json!({"cpuCores":3,"ramMiB":3136,"graphics":"host"});
        let mut ui = Scripted::new(view, |_, _| unreachable!());
        wait(&mut ui.app, "id:library");
        ui.app
            .set_viewport(Size::new(Points(width), Points(height)))
            .unwrap();
        press(&mut ui.app, "new-instance");
        press(&mut ui.app, "use-r34");
        wait(&mut ui.app, "id:hardware-fields");
        reach(
            &mut ui.app,
            width,
            height,
            &[
                "instance-name",
                "choose-template",
                "change-image",
                "choose-cpu",
                "choose-ram",
                "choose-graphics",
                "choose-disk",
                "create",
                "cancel-create",
            ],
        );
        press(&mut ui.app, "cancel-create");
        press(&mut ui.app, "manage-a");
        reach(&mut ui.app, width, height, &["edit-hardware"]);
        press(&mut ui.app, "edit-hardware");
        wait(&mut ui.app, "id:hardware-fields");
        reach(
            &mut ui.app,
            width,
            height,
            &[
                "choose-cpu",
                "choose-ram",
                "choose-graphics",
                "save-hardware",
                "cancel-hardware",
            ],
        );
    }
}

/// HOME 清单晚于库首帧到达；一次进入应用库即可选择只声明 HOME 的包。
#[test]
fn home_picker_tracks_late_candidates_and_instance_changes_in_place() {
    let mut view = ready_view();
    view["instances"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"b","name":"娱乐机","state":"ready"}));
    view["catalogStatus"] = json!({"kind":"pending"});
    let mut ui = Scripted::new(view, |cmd, view| {
        match (cmd["kind"].as_str(), cmd["instanceId"].as_str()) {
            (Some("setDefaultHome"), Some("a")) => {
                assert_eq!(cmd["package"], "demo.home");
                view["desktop"]["defaultPackage"] = cmd["package"].clone();
            }
            (Some("selectInstance"), Some(id)) => {
                view["selectedId"] = json!(id);
                view["id"] = json!(id);
                view["desktop"] = if id == "b" {
                    json!({"defaultPackage":null,"candidates":[]})
                } else {
                    json!({"defaultPackage":"demo.home","candidates":["demo.home"],"candidateLabels":{"demo.home":"书房桌面"}})
                };
            }
            other => panic!("意外的命令 {other:?}"),
        }
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:catalog-pending");
    gone(&mut ui.app, "id:choose-desktop");
    ui.update(|v| {
        v["groups"] = json!([{"id":"system","name":"系统应用","count":1,"apps":[
            {"package":"demo.store","label":"商店","system":true,"launchable":true,"actions":[]}]}]);
        v["catalogStatus"] = json!({"kind":"ready"});
        v["desktop"] = json!({"defaultPackage":"demo.stock","candidates":["demo.stock","demo.home"],"candidateLabels":{"demo.home":"书房桌面"}});
    });
    wait(&mut ui.app, "id:app-demo.store");
    gone(&mut ui.app, "id:app-demo.home");
    press(&mut ui.app, "open-management");
    wait(&mut ui.app, "text:默认桌面 · demo.stock");
    press(&mut ui.app, "choose-desktop");
    choose(&mut ui.app, "书房桌面");
    wait(&mut ui.app, "text:默认桌面 · 书房桌面");
    assert_eq!(
        *ui.commands.lock().unwrap(),
        [json!({"kind":"setDefaultHome","instanceId":"a","package":"demo.home"})]
    );
    press(&mut ui.app, "instance-b");
    gone(&mut ui.app, "id:choose-desktop");
    gone(&mut ui.app, "text:默认桌面 · 书房桌面");
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "open-management");
    ui.update(|v| v["desktop"] = json!({"defaultPackage":"demo.work","candidates":["demo.work"]}));
    wait(&mut ui.app, "text:默认桌面 · demo.work");
    press(&mut ui.app, "instance-a");
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "open-management");
    wait(&mut ui.app, "text:默认桌面 · 书房桌面");
    gone(&mut ui.app, "text:默认桌面 · demo.work");
}

#[test]
fn task_query_distinguishes_pending_cached_failure_and_confirmed_empty() {
    let mut view = ready_view();
    view["tasksLoaded"] = json!(false);
    let mut ui = Scripted::new(view, |_, _| unreachable!());
    wait(&mut ui.app, "id:panel-tasks >> text:运行中");
    press(&mut ui.app, "panel-tasks");
    wait(&mut ui.app, "text:正在读取运行中的应用…");
    gone(&mut ui.app, "id:tasks-empty");
    ui.update(|view| {
        view["tasksLoaded"] = json!(true);
        view["running"] =
            json!([{"instanceId":"a","package":READER,"label":"阅读器","windowOpen":false}]);
    });
    wait(&mut ui.app, "id:panel-tasks >> text:运行中 · 1");
    wait(&mut ui.app, "text:后台运行");
    ui.update(|view| view["runningError"] = json!("设备暂时离线"));
    wait(&mut ui.app, "text:/设备暂时离线/");
    wait(&mut ui.app, "id:panel-tasks >> text:运行中");
    wait(&mut ui.app, "text:后台运行");
    gone(&mut ui.app, "id:tasks-empty");
    press(&mut ui.app, "panel-apps");
    gone(&mut ui.app, "id:running-error");
    ui.update(|view| {
        view["runningError"] = Value::Null;
        view["running"] = json!([]);
    });
    press(&mut ui.app, "panel-tasks");
    wait(&mut ui.app, "id:tasks-empty");
    wait(&mut ui.app, "id:panel-tasks >> text:运行中 · 0");
    ui.app
        .set_viewport(Size::new(Points(420.0), Points(600.0)))
        .unwrap();
    wait(&mut ui.app, "id:task-scroll");
    let tasks_bounds = bounds(&ui.app, "task-scroll");
    press(&mut ui.app, "panel-apps");
    wait(&mut ui.app, "id:apk-drop");
    assert_eq!(tasks_bounds, bounds(&ui.app, "apk-drop"));
}

#[test]
fn keyboard_restores_the_task_from_the_selected_instance() {
    let mut view = ready_view();
    view["running"] =
        json!([{"instanceId":"a","package":READER,"label":"阅读器","windowOpen":false}]);
    let mut ui = Scripted::new(view, |cmd, view| {
        assert_eq!(
            cmd,
            &json!({"kind":"focusApp","instanceId":"a","package":READER})
        );
        view["running"][0]["windowOpen"] = json!(true);
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:panel-apps");
    // Tab 进入标签栏，方向键切换内容区。遍历一次窗口的按钮，避免测试依赖某个绝对焦点序号。
    let stops = ui.app.screen().count("role:button").unwrap() + 2;
    for _ in 0..stops {
        ui.app.press_key("Tab").unwrap();
        ui.app.press_key("Right").unwrap();
        if ui.app.screen().matches("id:running-list").unwrap() {
            break;
        }
    }
    wait(&mut ui.app, "role:button[恢复 阅读器 窗口]");
    ui.app.press_key("Tab").unwrap();
    ui.app.press_key("Enter").unwrap();
    wait(&mut ui.app, "text:窗口已打开");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
}

#[test]
fn inventory_routes_to_the_row_and_distinguishes_configured_observed_and_unknown() {
    let mut view = ready_view();
    view["instances"] = json!([
        {"id":"a","name":"工作机","state":"ready","config":{"cpuCores":2,"ramMiB":3072,"screenWidth":1080,"screenHeight":2400,"diskBytes":8589934592_i64}},
        {"id":"b","name":"阅读机","state":"ready","config":{"cpuCores":4,"ramMiB":1536,"screenWidth":1280,"screenHeight":720,"diskBytes":17179869184_i64},
            "observed":{"cpuCores":3,"ramMiB":2048}}
    ]);
    let mut ui = Scripted::new(view, |cmd, view| {
        assert_eq!(cmd["kind"], "selectInstance");
        view["id"] = cmd["instanceId"].clone();
        view["selectedId"] = cmd["instanceId"].clone();
        json!({"status":"accepted"})
    });
    wait(&mut ui.app, "id:library >> text:工作机");
    press(&mut ui.app, "all-instances");
    for (id, values) in [
        ("a", ["2 核", "3 GiB", "1080 × 2400", "8 GiB"]),
        ("b", ["4 核", "1536 MiB", "1280 × 720", "16 GiB"]),
    ] {
        for value in values {
            wait(&mut ui.app, &format!("id:row-{id} >> text:{value}"));
        }
    }
    press(&mut ui.app, "manage-b");
    wait(&mut ui.app, "id:manage-instance >> text:阅读机");
    wait(&mut ui.app, "text:CPU · 4 核");
    wait(&mut ui.app, "text:观测到的 CPU · 3 核     内存 · 2 GiB");
    press(&mut ui.app, "manage-back");
    wait(&mut ui.app, "id:inventory");

    // 旧工作区缺少 AVD 配置时仍有原名称、状态及导航，逐项留下未知。
    ui.update(|view| {
        view["instances"][0]
            .as_object_mut()
            .unwrap()
            .remove("config");
    });
    wait(&mut ui.app, "id:row-a >> text:未知");
    assert_eq!(ui.app.screen().count("id:row-a >> text:未知").unwrap(), 4);
    press(&mut ui.app, "enter-a");
    wait(&mut ui.app, "id:library >> text:工作机");
    press(&mut ui.app, "open-management");
    wait(&mut ui.app, "text:CPU · 未知");
    wait(&mut ui.app, "text:观测到的 CPU · 未知     内存 · 未知");
    press(&mut ui.app, "manage-back");
    wait(&mut ui.app, "id:library >> text:工作机");
    assert_eq!(
        *ui.commands.lock().unwrap(),
        [
            json!({"kind":"selectInstance","instanceId":"b"}),
            json!({"kind":"selectInstance","instanceId":"a"})
        ]
    );
}

#[test]
fn only_the_visible_app_catalog_accepts_an_apk_drop() {
    let mut ui = Scripted::new(ready_view(), |cmd, _| {
        assert_eq!(
            cmd,
            &json!({"kind":"install","instanceId":"a","apkPath":"/tmp/reader.apk"})
        );
        json!({"status":"accepted"})
    });
    ui.channel.handle("windroid.windowSettings", |call| {
        call.resolve(json!({"closeAction":"ask"}))
    });
    wait(&mut ui.app, "id:library");
    for (entry, target) in [
        ("panel-tasks", "task-scroll"),
        ("open-management", "manage-instance"),
        ("all-instances", "inventory"),
        ("show-settings", "settings"),
        ("show-environment", "environment"),
    ] {
        press(&mut ui.app, entry);
        let selector = format!("id:{target}");
        wait(&mut ui.app, &selector);
        gone(&mut ui.app, "id:apk-drop");
        let err = ui
            .app
            .drop_files(&selector, vec!["file:///tmp/reader.apk".into()])
            .unwrap_err();
        assert!(err.contains("onFileDrop"), "{err}");
    }
    assert!(ui.commands.lock().unwrap().is_empty());
    press(&mut ui.app, "instance-a");
    wait(&mut ui.app, "id:apk-drop");
    ui.app
        .drop_files("id:apk-drop", vec!["file:///tmp/reader.apk".into()])
        .unwrap();
    gone(&mut ui.app, "id:pending");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
}

#[test]
fn window_settings_share_the_native_close_choice() {
    let mut ui = Scripted::new(ready_view(), |_, _| unreachable!());
    let (reads, pending) = mpsc::channel();
    ui.channel.handle("windroid.windowSettings", move |call| {
        reads.send(call.into_reply().unwrap()).unwrap();
    });
    let (writes, saved) = mpsc::channel();
    ui.channel
        .handle("windroid.setWindowSettings", move |call| {
            let settings = call.data().clone();
            writes.send(settings.clone()).unwrap();
            call.resolve(settings);
        });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "show-settings");
    let first_read = pending.recv_timeout(WAIT).unwrap();
    // 关窗面板的选择先于初始读取回执到达。较旧的 ask 快照不能覆盖这次用户选择。
    ui.channel
        .send(
            "windroid.windowSettingsChanged",
            json!({"closeAction":"background"}),
        )
        .unwrap();
    wait(&mut ui.app, "id:close-preference >> text:/保留后台/");
    first_read.resolve(json!({"closeAction":"ask"}));
    ui.app.idle(Duration::from_millis(50)).unwrap();
    assert!(
        ui.app
            .screen()
            .matches("id:close-preference >> text:/保留后台/")
            .unwrap()
    );
    press(&mut ui.app, "close-preference");
    choose(&mut ui.app, "完全退出");
    assert_eq!(
        saved.recv_timeout(WAIT).unwrap(),
        json!({"closeAction":"quit"})
    );
    wait(&mut ui.app, "id:close-preference >> text:/完全退出/");
    ui.channel
        .send(
            "windroid.windowSettingsChanged",
            json!({"closeAction":"ask"}),
        )
        .unwrap();
    wait(&mut ui.app, "id:close-preference >> text:/每次询问/");
    assert!(ui.commands.lock().unwrap().is_empty());
}

#[test]
fn narrow_resource_pages_wrap_paths_and_keep_settings_navigation_visible() {
    let mut view = ready_view();
    let root = "/Volumes/Studio External Drive/开发与测试/Windroid/运行资源".to_string();
    let long_path = format!("{root}/system-images/android-35/google_apis_playstore/arm64-v8a/r9");
    view["preparation"]["images"] = json!([
        image("r33", 33, false),
        image("r35", 35, false),
        image("r34", 34, true),
        image("conflicting", 30, false)
    ]);
    view["preparation"]["images"][3]["integrity"] = json!("conflict");
    view["preparation"]["images"][3]["issue"] = json!("包含未知文件 extra.img");
    view["preparation"]["images"][1]["location"] = json!(long_path);
    view["storage"] = storage(&root, &long_path);
    let mut ui = Scripted::new(view, |_, _| unreachable!());
    ui.channel
        .handle("windroid.pickResource", |call| call.resolve(json!("/new")));
    ui.channel.handle("windroid.previewStorage", move |call| {
        let mut plan = storage_plan(call.data(),1);
        plan["versions"] = json!((0..16).map(|i| json!({"id":format!("id-{i}"),"name":format!("版本 {i}"), "source":format!("{long_path}/r{i}"),
            "target":format!("/Volumes/Another External Volume/Relocated Images/android-35/google_apis_playstore/arm64-v8a/r{i}"),"bytes":2048,
            "cleanup":false,"downloadable":false,"status":"reuse","detail":""})).collect::<Vec<_>>());
        call.resolve(plan);
    });
    ui.app
        .set_viewport(Size::new(Points(420.0), Points(520.0)))
        .unwrap();
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "show-environment");
    press(&mut ui.app, "environment-images");
    wait(&mut ui.app, "id:download-r35");
    wait(&mut ui.app, "text:需要处理");
    gone(&mut ui.app, "id:download-conflicting");
    assert!(bounds(&ui.app, "image-r34").top() < bounds(&ui.app, "image-r35").top());
    assert!(bounds(&ui.app, "image-r35").top() < bounds(&ui.app, "image-r33").top());
    let long_location = ui
        .app
        .screen()
        .layout_nodes()
        .into_iter()
        .filter_map(|n| n.text)
        .find(|row| row.text.text.contains("开发与测试"))
        .unwrap();
    assert!(long_location.content.size.height.0 > 24.0 && long_location.text.max_lines == 0);
    let check_width = |app: &Headless| {
        for node in app
            .screen()
            .layout_nodes()
            .into_iter()
            .filter(|n| !n.hidden)
        {
            assert!(
                node.bounds.right().0 <= 421.0 && node.bounds.left().0 >= -1.0,
                "{:?} {:?} {:?}",
                node.test_id,
                node.text.map(|t| t.text.text.clone()),
                node.bounds
            );
        }
    };
    check_width(&ui.app);
    press(&mut ui.app, "show-settings");
    press(&mut ui.app, "settings-storage");
    press(&mut ui.app, "choose-image-directory");
    wait(&mut ui.app, "id:storage-preview");
    let heading = bounds(&ui.app, "show-guide");
    check_width(&ui.app);
    ui.app.scroll("id:workspace-scroll", Points(250.0)).unwrap();
    assert_eq!(heading, bounds(&ui.app, "show-guide"));
    check_width(&ui.app);
}

#[test]
fn batch_selection_survives_settings_and_confirmation_keeps_reviewed_targets() {
    let mut view = ready_view();
    view["instances"] = json!([
        {"id":"a","name":"工作机","state":"ready"},
        {"id":"b","name":"阅读机","state":"ready"},
        {"id":"c","name":"测试机","state":"stopped"}
    ]);
    let mut ui = Scripted::new(view, |cmd, view| {
        assert_eq!(cmd["kind"], "selectInstance");
        view["selectedId"] = cmd["instanceId"].clone();
        json!({"status":"accepted"})
    });
    let (tx, requests) = mpsc::channel();
    ui.channel.handle("windroid.previewBatch", move |call| {
        tx.send((call.data().clone(), call.into_reply().unwrap()))
            .unwrap();
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "all-instances");
    wait(&mut ui.app, "text:全部停止…");
    press(&mut ui.app, "batch-stop");
    let (all, reply) = requests.recv_timeout(WAIT).unwrap();
    assert_eq!(
        all,
        json!({"scope":"all","action":"stop","ids":["a","b","c"]})
    );
    let plan = json!({"previewId":1,"scope":"all","action":"stop","items":[
        {"id":"a","name":"工作机","eligible":true,"reason":"将停止","windows":["设备视图","设置"]},
        {"id":"b","name":"阅读机","eligible":true,"reason":"将停止","windows":[]},
        {"id":"c","name":"测试机","eligible":false,"reason":"已停止","windows":[]}
    ]});
    reply.resolve(plan.clone());
    wait(&mut ui.app, "text:本机全部 3 个实例 · 可停止 2 个");
    let targets = bounds(&ui.app, "batch-targets");
    assert!(
        (targets.left().0 + targets.right().0 - 1024.).abs() < 2.,
        "{targets:?}"
    );
    assert!(targets.top().0 > 200., "{targets:?}");
    wait(&mut ui.app, "id:preview-a >> text:将关闭 · 设备视图、设置");
    wait(&mut ui.app, "id:preview-c >> text:跳过 · 已停止");
    wait(&mut ui.app, "text:/结束这些实例的后台任务/");
    ui.app
        .set_viewport(Size::new(Points(420.), Points(360.)))
        .unwrap();
    ui.app.scroll("id:batch-targets", Points(80.)).unwrap();
    let confirm = bounds(&ui.app, "batch-confirm");
    assert!(confirm.left().0 >= 0. && confirm.right().0 <= 420. && confirm.bottom().0 <= 360.);
    press(&mut ui.app, "batch-cancel");

    press(&mut ui.app, "check-all");
    wait(&mut ui.app, "text:已选 3 个实例");
    press(&mut ui.app, "check-all");
    wait(&mut ui.app, "text:共 3 个实例");
    press(&mut ui.app, "check-c");
    press(&mut ui.app, "check-a");
    press(&mut ui.app, "manage-b");
    wait(&mut ui.app, "id:manage-instance >> text:阅读机");
    press(&mut ui.app, "manage-back");
    wait(&mut ui.app, "text:已选 2 个实例");
    press(&mut ui.app, "batch-stop");
    let (chosen, reply) = requests.recv_timeout(WAIT).unwrap();
    assert_eq!(
        chosen,
        json!({"scope":"selected","action":"stop","ids":["a","c"]})
    );
    let mut selected = plan;
    selected["scope"] = json!("selected");
    selected["previewId"] = json!(2);
    selected["items"].as_array_mut().unwrap().remove(1);
    reply.resolve(selected.clone());
    wait(&mut ui.app, "text:已选 2 个实例 · 可停止 1 个");
    ui.update(|view| view["instances"].as_array_mut().unwrap().reverse());
    ui.app.idle(Duration::from_millis(80)).unwrap();
    assert!(ui.app.screen().enabled("id:batch-recheck").unwrap());
    press(&mut ui.app, "batch-recheck");
    let (recheck, reply) = requests.recv_timeout(WAIT).unwrap();
    assert_eq!(recheck, json!({"previewId":2}));
    selected["previewId"] = json!(3);
    selected["items"][0]["eligible"] = json!(false);
    selected["items"][0]["reason"] = json!("正在停止");
    reply.resolve(selected);
    wait(&mut ui.app, "id:preview-a >> text:跳过 · 正在停止");
    gone(&mut ui.app, "id:batch-confirm");
    assert!(bounds(&ui.app, "preview-a").top() < bounds(&ui.app, "preview-c").top());
    assert_eq!(
        ui.commands.lock().unwrap().as_slice(),
        [json!({"kind":"selectInstance","instanceId":"b"})]
    );
}

#[test]
fn batch_receipt_tracks_progress_and_retries_only_the_reviewed_failures() {
    let mut view = ready_view();
    view["batch"] = json!({"jobId":1,"action":"start","scope":"selected","active":true,"items":[
        {"id":"a","name":"工作机","status":"running","message":"正在启动"},
        {"id":"b","name":"阅读机","status":"pending","message":"等待处理"},
        {"id":"c","name":"测试机","status":"skipped","message":"已在运行"}
    ]});
    let mut ui = Scripted::new(view, |cmd, view| {
        assert_eq!(cmd, &json!({"kind":"executeBatch","previewId":2}));
        view["batch"] = json!({"jobId":2,"scope":"retry","action":"start","active":false,"items":[
            {"id":"b","name":"阅读机","status":"succeeded","message":"已启动"}]});
        json!({"status":"accepted"})
    });
    ui.channel.handle("windroid.previewBatch", |call| {
        assert_eq!(call.data(), &json!({"retryOf":1}));
        call.resolve(
            json!({"previewId":2,"scope":"retry","action":"start","items":[
                {"id":"b","name":"阅读机","eligible":true,"reason":"将启动","windows":[]}
            ]}),
        );
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "all-instances");
    wait(&mut ui.app, "text:正在启动 · 1 / 3");
    wait(&mut ui.app, "text:进行中 · 正在启动");
    gone(&mut ui.app, "id:batch-retry");
    gone(&mut ui.app, "id:check-all");
    ui.update(|view| {
        let job = &mut view["batch"];
        job["active"] = json!(false);
        job["items"][0]["status"] = json!("succeeded");
        job["items"][0]["message"] = json!("已启动");
        job["items"][1]["status"] = json!("failed");
        job["items"][1]["message"] = json!("模拟器进程已退出");
    });
    wait(&mut ui.app, "text:成功 1 个 · 跳过 1 个 · 失败 1 个");
    wait(&mut ui.app, "text:失败 · 模拟器进程已退出");
    press(&mut ui.app, "batch-retry");
    wait(&mut ui.app, "text:上次失败的 1 个实例 · 可启动 1 个");
    gone(&mut ui.app, "id:preview-a");
    gone(&mut ui.app, "id:preview-c");
    press(&mut ui.app, "batch-confirm");
    wait(&mut ui.app, "text:成功 1 个 · 跳过 0 个 · 失败 0 个");
    gone(&mut ui.app, "id:outcome-a");
    gone(&mut ui.app, "id:batch-retry");
    press(&mut ui.app, "batch-done");
    wait(&mut ui.app, "id:check-all");
    assert_eq!(ui.commands.lock().unwrap().len(), 1);
}

#[test]
fn batch_controls_drive_the_instance_lanes_and_leave_the_unselected_guest_ready() {
    let mut ui = LibraryUi::new();
    let first = ui.create();
    press(&mut ui.app, "new-instance");
    press(&mut ui.app, "use-system-image");
    let second = ui.create();
    press(&mut ui.app, "all-instances");
    press(&mut ui.app, "batch-start");
    wait(&mut ui.app, "text:本机全部 2 个实例 · 可启动 2 个");
    press(&mut ui.app, "batch-confirm");
    wait(&mut ui.app, "text:成功 2 个 · 跳过 0 个 · 失败 0 个");
    press(&mut ui.app, "batch-done");
    press(&mut ui.app, &format!("check-{first}"));
    press(&mut ui.app, "batch-stop");
    wait(&mut ui.app, "text:已选 1 个实例 · 可停止 1 个");
    gone(&mut ui.app, &format!("id:preview-{second}"));
    press(&mut ui.app, "batch-confirm");
    wait(&mut ui.app, "text:成功 1 个 · 跳过 0 个 · 失败 0 个");
    let view = ui.library.snapshot();
    assert_eq!(view["batch"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(view["batch"]["items"][0]["id"], first);
    let guests = view["instances"].as_array().unwrap();
    assert_eq!(
        guests.iter().find(|d| d["id"] == first).unwrap()["state"],
        "stopped"
    );
    assert_eq!(
        guests.iter().find(|d| d["id"] == second).unwrap()["state"],
        "ready"
    );
}

/// 经过工作区标记、真实实例清单、写者和读取桥，验证两个原生入口只展示各自的来源。
#[test]
fn native_log_entries_keep_history_paging_and_same_named_instances_separate() {
    use windroid_diagnostics::{Event, RunWriter, new_run_id};
    let home = std::env::temp_dir().join(format!("windroid-ui-logs-{}", new_run_id().unwrap()));
    runtime::workspace::open(&home).unwrap();
    let layout = runtime::Layout { root: home.clone() };
    let store = runtime::instances::InstanceStore::new(layout.clone());
    let spec = Spec {
        template: "compact-720".into(),
        image: Image {
            revision: 14,
            api: 34,
            tag: "google_apis_playstore".into(),
            abi: "arm64-v8a".into(),
        },
        disk_bytes: 6 << 30,
    };
    let a = store.create("工作机", spec.clone()).unwrap().id.to_string();
    let b = store.create("工作机", spec).unwrap().id.to_string();
    let old_app = RunWriter::app(&home).unwrap();
    let first = old_app.id().to_owned();
    old_app
        .sink()
        .emit(Event::new("windroid", "第一次 Windroid 启动"));
    old_app.finish("正常退出").unwrap();
    let app = RunWriter::app(&home).unwrap();
    let current = app.id().to_owned();
    let mut warning = Event::new("windroid", "启动前检查拒绝 · 端口已占用");
    warning.instance_id = Some(b.clone());
    app.sink().emit(warning);
    app.finish("正常退出").unwrap();
    let run_a = new_run_id().unwrap();
    let emu = RunWriter::emulator(&home, &a, Some(&current), &run_a).unwrap();
    for n in 1..=25 {
        emu.sink()
            .emit(Event::new("stdout", format!("实例 A 的输出 {n}")));
    }
    emu.finish("退出码 0").unwrap();
    std::fs::write(
        home.join("instances").join(&a).join("emulator.log"),
        "升级前的输出",
    )
    .unwrap();

    let config = config();
    let (commands, _windows) = mpsc::channel();
    let (_events, events_rx) = mpsc::channel();
    let library =
        host::Library::attach(config.host.clone(), layout, false, commands, events_rx).unwrap();
    let mut ui = render(config);
    wait(&mut ui, "id:library");
    press(&mut ui, "show-settings");
    press(&mut ui, "settings-logs");
    wait(&mut ui, &format!("id:log-line-{first}-1"));
    wait(&mut ui, "text:启动前检查拒绝 · 端口已占用");
    press(&mut ui, &format!("open-log-run-{first}"));
    wait(&mut ui, "text:第一次 Windroid 启动");
    gone(&mut ui, "text:启动前检查拒绝 · 端口已占用");

    press(&mut ui, "all-instances");
    press(&mut ui, &format!("manage-{a}"));
    wait(&mut ui, "id:manage-instance");
    press(&mut ui, "manage-more");
    press(&mut ui, "emulator-logs");
    wait(&mut ui, "text:实例 A 的输出 1");
    press(&mut ui, &format!("log-next-{run_a}"));
    wait(&mut ui, "text:实例 A 的输出 25");
    gone(&mut ui, "text:实例 A 的输出 1");
    press(&mut ui, &format!("log-back-{run_a}"));
    wait(&mut ui, "text:实例 A 的输出 1");
    press(&mut ui, "logs-legacy");
    wait(&mut ui, "text:旧版日志 · 启动时间未知");
    wait(&mut ui, "text:升级前的输出");
    press(&mut ui, "log-return");
    wait(&mut ui, "id:manage-instance");
    press(&mut ui, "manage-back");
    press(&mut ui, &format!("manage-{b}"));
    wait(&mut ui, "id:manage-instance");
    press(&mut ui, "manage-more");
    press(&mut ui, "emulator-logs");
    wait(&mut ui, "id:logs-empty");
    gone(&mut ui, "id:logs-legacy");
    gone(&mut ui, &format!("id:log-run-{run_a}"));
    library.shutdown();
    drop(ui);
    drop(library);
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn a_rejected_start_opens_its_app_run_and_follow_preserves_the_selected_day() {
    let mut view = ready_view();
    view["operation"] =
        json!({"instanceId":"a","message":"端口已占用","failed":true,"startFailed":true,"seq":1});
    let mut ui = Scripted::new(view, |_, _| unreachable!());
    let next_day = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let clock = next_day.clone();
    ui.channel.handle("windroid.logs", move |call| {
        use std::sync::atomic::Ordering;
        let q = call.data();
        if q["kind"] == "lastStart" {
            assert_eq!(q["instanceId"], "a");
            return call.resolve(json!({"appRunId":"current","emulatorRunId":null}));
        }
        assert_eq!(q["source"], "app");
        let first = json!({"day":"2026-11-18","local":"2026-11-18T23:59:59+08:00"});
        let second = json!({"day":"2026-11-19","local":"2026-11-19T00:00:01+08:00"});
        let midnight = clock.load(Ordering::Acquire);
        let value = match q["kind"].as_str().unwrap() {
            "catalog" => {
                let days = if midnight { json!([second["day"], first["day"]]) } else { json!([first["day"]]) };
                json!({"runs":[{"id":"current","started":first,"dates":days,"active":true}],
                    "legacy":false, "issue":null})
            }
            "page" => {
                assert_eq!(q["runId"], "current");
                let after = q["after"].as_u64().unwrap();
                let mut rows = vec![];
                for (n, stamp, msg) in [(1, &first, "拦截了 A"), (2, &second, "A 检查完成")] {
                    if n > after && (n == 1 || midnight) && (q["day"].is_null() || q["day"] == stamp["day"]) {
                        rows.push(json!({"seq":n,"stamp":stamp,"segment":n,"source":"windroid","text":msg,"instanceId":"a","continued":false}));
                    }
                }
                json!({"records":rows,"more":false})
            }
            kind => panic!("未预期的日志请求 {kind}"),
        };
        call.resolve(value);
    });

    wait(&mut ui.app, "id:open-start-log");
    press(&mut ui.app, "open-start-log");
    wait(&mut ui.app, "id:open-log-day-1");
    wait(&mut ui.app, "text:拦截了 A");
    press(&mut ui.app, "open-log-day-1");
    wait(&mut ui.app, "id:open-log-run-current");
    press(&mut ui.app, "logs-follow");
    wait(&mut ui.app, "text:暂停跟读");
    next_day.store(true, std::sync::atomic::Ordering::Release);
    press(&mut ui.app, "logs-refresh");
    wait(&mut ui.app, "id:logs-choose >> text:/2026-11-18/");
    press(&mut ui.app, "logs-choose");
    // 日期列表已经增加，读者仍留在原日期。
    assert!(
        ui.app
            .menu()
            .unwrap()
            .iter()
            .any(|m| m.label == "2026-11-19")
    );
    choose(&mut ui.app, "2026-11-18");
    gone(&mut ui.app, "text:A 检查完成");
    press(&mut ui.app, "open-log-run-current");
    wait(&mut ui.app, "text:拦截了 A");
    wait(&mut ui.app, "text:A 检查完成");
    wait(&mut ui.app, "id:open-log-day-2");
    press(&mut ui.app, "logs-follow");
    wait(&mut ui.app, "text:跟读");
}
