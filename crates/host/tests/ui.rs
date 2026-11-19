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
        wait(&mut self.app, "id:create-form");
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
        "ready":ready, "location":"/tmp/images", "borrowed":false, "downloadedBytes":0, "issue":null,
        "image":{"api":api,"tag":"google_apis_playstore","abi":"arm64-v8a","revision":14},"minEmulator":"34.0.0"})
}

fn ready_view() -> Value {
    json!({"id":"a","instances":[{"id":"a","name":"工作机","state":"ready"}],"selectedId":"a",
        "state":"ready","summary":"已就绪","capabilities":{"install":true,"launchApp":true,"deviceView":true,"stop":true,"refreshApps":true},
        "facts":{"services":{"activity":true,"package":true,"window":true},"adbVisible":true},"groups":[],"catalogStatus":{"kind":"ready"},"running":[],"desktop":null,"tasksLoaded":true,
        "preparation":{"phase":"ready","componentsReady":true,"images":[image("r34",34,true)],"components":[],"currentComponent":null,"totalBytes":0,"downloadedBytes":0,"borrowed":false,"error":null}})
}

#[test]
fn preparation_requires_consent_and_supports_license_pause_resume_retry_and_rescan() {
    let mut view = ready_view();
    view["instances"] = json!([]);
    view["selectedId"] = Value::Null;
    view["preparation"] = json!({"phase":"missing","components":[{"id":"image","name":"系统镜像","version":"34","source":"https://example.org/image.zip",
        "license":"镜像许可","licenseUrl":"https://example.org/terms","licenseText":"镜像完整许可正文","sizeBytes":1048576,"ready":false}],
        "currentComponent":null,"downloadedBytes":0,"totalBytes":1048576,"borrowed":false,"error":null});
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
        "sizeBytes":1024, "location":"/tmp/tools", "license":"SDK", "ready":false,"borrowed":false}]);
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
    wait(&mut ui.app, "text:/Windroid 当前将运行记录/");
    press(&mut ui.app, "show-guide");
    press(&mut ui.app, "guide-begin");
    press(&mut ui.app, "use-r34");
    wait(&mut ui.app, "id:create-form");
    press(&mut ui.app, "cancel-create");
    wait(&mut ui.app, "text:/Windroid 当前将运行记录/");
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
        "defaultRuntimeRoot":"/home/runtime", "defaultImageRoot":"/home/system-images"})
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
            7 => v["storage"]["runtimeRoot"] = json!("/home/runtime"),
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
            "status":if missing {"missing"} else {"reuse"}, "bytes":1048576, "downloadable":missing, "cleanup":false, "detail":""}]);
        plan["blockers"] = if missing {json!(["目标缺少 r14"])} else {json!([])};
        call.resolve(plan);
    });
    wait(&mut ui.app, "id:library");
    press(&mut ui.app, "show-settings");
    press(&mut ui.app, "settings-storage");
    press(&mut ui.app, "choose-image-directory");
    wait(&mut ui.app, "id:download-candidate-r34");
    assert!(ui.commands.lock().unwrap().is_empty());
    press(&mut ui.app, "download-candidate-r34");
    wait(&mut ui.app, "text:512 KiB / 1.0 MiB");
    ui.update(|v| {
        let p = &mut v["preparation"];
        p["phase"] = json!("ready");
        p["candidateTarget"] = Value::Null;
        p["currentComponent"] = Value::Null;
    });
    gone(&mut ui.app, "id:pause-candidate");
    assert!(!ui.app.screen().enabled("id:storage-apply").unwrap());
    press(&mut ui.app, "storage-recheck");
    wait(&mut ui.app, "text:可原地复用");
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
    press(&mut ui.app, "create");
    wait(&mut ui.app, "text:实例已创建");
    gone(&mut ui.app, "text:创建实例请求已受理");
    assert_eq!(
        *ui.commands.lock().unwrap(),
        [
            json!({"kind":"createInstance","template":"tablet-1200","name":"我的 Android 工作","imageId":"r34","image":image("r34",34,true)["image"]})
        ]
    );
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
        image("external", 30, false)
    ]);
    view["preparation"]["images"][3]["borrowed"] = json!(true);
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
    wait(&mut ui.app, "text:需在指定的 SDK 中准备");
    gone(&mut ui.app, "id:download-external");
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
