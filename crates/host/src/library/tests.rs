use super::*;

fn library() -> (Arc<Library>, mpsc::Receiver<WindowCommand>) {
    let (windows, rx) = mpsc::channel();
    let (_events, window_events) = mpsc::channel();
    let library = Library::attach(
        HostChannel::new(),
        Layout {
            root: PathBuf::default(),
        },
        true,
        windows,
        window_events,
    )
    .unwrap();
    (library, rx)
}
use std::path::PathBuf;
fn create(lib: &Arc<Library>, name: &str) -> String {
    lib.perform(&json!({"kind":"createInstance","imageId":"system-image","name":name,"template":"compact-720"}))
        .unwrap();
    lib.snapshot()["selectedId"].as_str().unwrap().into()
}
fn until(predicate: impl Fn() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(start.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn creation_requires_an_explicit_available_image() {
    let (lib, _) = library();
    for image in [JsonValue::Null, json!("outside-catalog")] {
        assert!(
            lib.perform(&json!({"kind":"createInstance","name":"手机","imageId":image}))
                .is_err()
        );
    }
    assert_eq!(lib.snapshot()["instances"], json!([]));
    create(&lib, "A");
    create(&lib, "B");
    assert_eq!(lib.snapshot()["instances"].as_array().unwrap().len(), 2);
}

#[test]
fn picker_keeps_its_instance_when_selection_changes() {
    struct WaitForInstall {
        inner: device::FakeBackend,
        started: mpsc::Sender<String>,
        proceed: mpsc::Receiver<()>,
    }
    impl device::Backend for WaitForInstall {
        fn exec(&mut self, command: &Command, facts: &mut Facts) -> Result<(), Rejection> {
            if let Command::Install { apk_path } = command {
                self.started.send(apk_path.clone()).unwrap();
                self.proceed.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            self.inner.exec(command, facts)
        }
        fn observe(&mut self, facts: &mut Facts) {
            self.inner.observe(facts);
        }
        fn abort(&mut self, facts: &mut Facts) {
            self.inner.abort(facts);
        }
    }
    let (lib, windows) = library();
    let a = "installer";
    let (started, install_path) = mpsc::channel();
    let (allow, proceed) = mpsc::channel();
    let (handle, events) = device::spawn(
        InstanceId::new(a),
        a.into(),
        WaitForInstall {
            inner: device::FakeBackend::new(),
            started,
            proceed,
        },
        Default::default(),
    );
    lib.add("平板".into(), handle.clone(), events, None);
    assert!(matches!(
        handle.send(Command::Create { spec: fake_spec() }),
        Ack::Accepted { .. }
    ));
    let select = |id: &str| {
        lib.perform(&json!({"kind":"selectInstance","instanceId":id}))
            .unwrap()
    };
    select(a);
    ready(&lib, a);
    lib.perform(&json!({"kind":"addApk","instanceId":a}))
        .unwrap();
    assert!(
        matches!(windows.recv().unwrap(), WindowCommand::PickApk {instance_id, instance_name}
            if instance_id == a && instance_name == "平板")
    );
    let b = create(&lib, "手机");
    lib.window_event(WindowEvent::ApkPicked {
        instance_id: a.into(),
        path: "/fixture/two words.apk".into(),
    });
    assert_eq!(
        install_path.recv_timeout(Duration::from_secs(5)).unwrap(),
        "/fixture/two words.apk"
    );
    assert_eq!(lib.snapshot()["selectedId"], b);
    assert_ne!(lib.snapshot()["operation"]["instanceId"], a);
    select(a);
    assert_eq!(lib.snapshot()["operation"]["message"], "正在安装 APK…");
    select(&b);
    allow.send(()).unwrap();
    until(|| {
        lib.state
            .lock()
            .unwrap()
            .operations
            .get(a)
            .is_some_and(|op| op["message"] == "APK 已安装并核实版本")
    });
    assert_eq!(lib.snapshot()["selectedId"], b);
    assert_ne!(lib.snapshot()["operation"]["instanceId"], a);
    select(a);
    assert_eq!(
        lib.snapshot()["operation"]["message"],
        "APK 已安装并核实版本"
    );
    assert!(
        lib.perform(&json!({"kind":"install","apkPath":"/fixture/package.apk"}))
            .is_err()
    );
}

#[test]
fn same_package_windows_close_and_restore_on_the_original_instance() {
    let (lib, _) = library();
    let a = create(&lib, "手机 A");
    let first = ready(&lib, &a);
    let b = create(&lib, "手机 B");
    let second = ready(&lib, &b);
    let action = |kind: &str, id: &str| {
        lib.perform(&json!({"kind":kind,"instanceId":id,"package":"com.example.reader"}))
            .unwrap()
    };
    action("launch", &a);
    action("launch", &b);
    action("focusApp", &a);
    assert_eq!(first.handle.snapshot().facts.session_count, 1);
    assert_eq!(second.handle.snapshot().facts.session_count, 1);

    action("backgroundApp", &a);
    assert_eq!(first.handle.snapshot().facts.session_count, 0);
    assert_eq!(lib.snapshot()["selectedId"], b);
    assert_eq!(lib.snapshot()["running"][0]["windowOpen"], true);
    action("selectInstance", &a);
    assert_eq!(lib.snapshot()["running"][0]["windowOpen"], false);
    action("focusApp", &a);
    action("selectInstance", &b);
    lib.window_event(WindowEvent::QuitRequested {
        key: WindowKey {
            instance_id: a.clone(),
            package: Some("com.example.reader".into()),
        },
    });
    until(|| first.handle.snapshot().facts.session_count == 0);
    assert_eq!(second.handle.snapshot().facts.session_count, 1);
    assert_eq!(lib.snapshot()["running"][0]["instanceId"], b);
    action("selectInstance", &a);
    until(|| lib.snapshot()["running"].as_array().unwrap().is_empty());
}

#[test]
fn running_list_waits_for_launcher_classification_and_keeps_cached_rows_on_error() {
    let (lib, _) = library();
    let id = create(&lib, "平板");
    let mut state = lib.state.lock().unwrap();
    let mut snapshot = state.instances[&id].handle.snapshot();
    let apps = std::mem::take(&mut snapshot.apps);
    let mut view = json!({});
    snapshot.catalog_status = contract::CatalogStatus::Pending;
    state.write_running(&mut view, &snapshot);
    assert_eq!(view["tasksLoaded"], false);

    state.tasks.insert(id.clone(), BTreeSet::new());
    state.write_running(&mut view, &snapshot);
    assert_eq!(view["tasksLoaded"], true);
    assert_eq!(view["running"], json!([]));
    state
        .tasks
        .get_mut(&id)
        .unwrap()
        .insert("com.example.reader".into());
    state.write_running(&mut view, &snapshot);
    assert_eq!(view["tasksLoaded"], false);
    assert!(view["runningError"].is_null());

    snapshot.apps = apps;
    snapshot.catalog_status = contract::CatalogStatus::Ready;
    state.write_running(&mut view, &snapshot);
    assert_eq!(view["tasksLoaded"], true);
    assert_eq!(view["running"][0]["package"], "com.example.reader");
    snapshot.catalog_status = contract::CatalogStatus::Failed {
        message: "缺少包索引".into(),
    };
    state.write_running(&mut view, &snapshot);
    assert_eq!(view["tasksLoaded"], false);
    assert_eq!(view["runningError"], "应用列表读取失败 · 缺少包索引");
    assert_eq!(view["running"][0]["package"], "com.example.reader");
    state.task_errors.insert(id, "读取活动栈超时".into());
    state.write_running(&mut view, &snapshot);
    assert_eq!(view["runningError"], "读取活动栈超时");
    assert_eq!(view["running"][0]["package"], "com.example.reader");
}

#[test]
fn old_group_files_do_not_change_sources_or_block_reopening() {
    use std::fs;
    let layout = temporary_workspace();
    let path = layout.root.join("groups.json");
    let store = InstanceStore::new(layout.clone());
    let record = store.create("已有实例", stored_spec()).unwrap();
    let id = record.id.as_str();
    let old = serde_json::to_vec_pretty(&json!({(id): {
        "names": {"group-1": "书架"},
        "membership": {"com.example.reader": "group-1", "com.example.launcher": "group-1"},
        "serial": 1
    }}))
    .unwrap();
    for previous in [None, Some(old.as_slice()), Some(b"{damaged".as_slice())] {
        if let Some(bytes) = previous {
            fs::write(&path, bytes).unwrap();
        }
        let (tx, windows) = mpsc::channel();
        let (_events, rx) = mpsc::channel();
        let lib = Library::attach(HostChannel::new(), layout.clone(), false, tx, rx).unwrap();
        assert_eq!(lib.snapshot()["selectedId"], id);

        // 保留真实工作区的挂载和命令路由，用已知 LAUNCHER 清单代替启动模拟器。
        let (handle, events) = device::spawn(
            record.id.clone(),
            record.serial(),
            device::FakeBackend::new(),
            Default::default(),
        );
        assert!(matches!(
            handle.send(Command::Create { spec: fake_spec() }),
            Ack::Accepted { .. }
        ));
        assert!(matches!(handle.send(Command::Start), Ack::Accepted { .. }));
        until(|| handle.snapshot().state == State::Ready);
        lib.add(record.name.clone(), handle, events, None);
        let snap = lib.snapshot();
        let sections = snap["groups"].as_array().unwrap();
        assert_eq!(sections.len(), 2);
        for (group, package) in [
            ("installed", "com.example.reader"),
            ("system", "com.example.launcher"),
        ] {
            let section = sections
                .iter()
                .find(|section| section["id"] == group)
                .unwrap();
            assert_eq!(section["count"], 1);
            assert_eq!(section["apps"][0]["package"], package);
            lib.perform(&json!({"kind": "launch", "instanceId": id, "package": package}))
                .unwrap();
            let command = windows.recv_timeout(Duration::from_secs(1)).unwrap();
            let WindowCommand::Open { key, serial, .. } = command else {
                panic!("应收到打开应用窗口的命令：{command:?}")
            };
            assert_eq!(key.instance_id, id);
            assert_eq!(key.package.as_deref(), Some(package));
            assert_eq!(serial, record.serial());
            lib.window_event(WindowEvent::Opened { key: key.clone() });
            lib.window_event(WindowEvent::FirstFrame { key });
        }
        assert_eq!(lib.snapshot()["facts"]["sessionCount"], 2);
        for kind in ["createGroup", "moveApp", "deleteGroup"] {
            let request = json!({"kind": kind, "instanceId": id, "groupId": "group-1",
                "name": "新分组", "package": "com.example.reader"});
            assert!(
                matches!(lib.perform(&request), Err(Rejection::Error { detail, .. }) if detail.contains("无法识别的操作"))
            );
        }
        lib.shutdown();
        match previous {
            Some(bytes) => assert_eq!(fs::read(&path).unwrap(), bytes),
            None => assert!(!path.try_exists().unwrap()),
        }
    }
    fs::remove_dir_all(layout.root).unwrap();
}

fn ready(lib: &Arc<Library>, id: &str) -> Instance {
    let i = lib.state.lock().unwrap().instances[id].clone();
    lib.perform(&json!({"kind":"start","instanceId":id}))
        .unwrap();
    until(|| i.handle.snapshot().state == State::Ready);
    i
}

fn fake_spec() -> contract::Spec {
    contract::Spec {
        template: "compact-720".into(),
        image: contract::Image {
            api: 34,
            tag: "test".into(),
            abi: "test".into(),
        },
        disk_bytes: 1,
    }
}

fn age_interaction(lib: &Library, id: &str) {
    lib.state
        .lock()
        .unwrap()
        .last_interaction
        .insert(id.into(), Instant::now() - Duration::from_secs(601));
}

#[test]
fn idle_freeze_requires_a_receipt_from_the_stop_just_completed() {
    let (lib, _windows) = library();
    let id = create(&lib, "后台实例");
    let i = ready(&lib, &id);
    let selected = create(&lib, "当前实例");
    age_interaction(&lib, &id);
    // 内存后端没有 Quick Boot 快照，与实际磁盘上查不到本轮凭证时相同。
    lib.perform(&json!({"kind":"freezeIdle","instanceId":id}))
        .unwrap();
    assert_eq!(i.handle.snapshot().state, State::Stopped);
    let state = lib.state.lock().unwrap();
    assert!(!state.frozen.contains(&id));
    assert!(
        state.operations[&id]["message"]
            .as_str()
            .unwrap()
            .contains("本次未保存")
    );
    assert_eq!(state.selected.as_deref(), Some(selected.as_str()));
    drop(state);

    for verified in [true, false] {
        ready(&lib, &id);
        {
            let s = lib.state.lock().unwrap();
            assert!(!s.frozen.contains(&id));
            assert!(!s.operations.contains_key(&id));
        }
        age_interaction(&lib, &id);
        lib.freeze_idle(&i, || {
            assert_eq!(i.handle.snapshot().state, State::Stopped);
            if verified {
                Ok(true)
            } else {
                Err(std::io::Error::other("读取测试凭证失败"))
            }
        })
        .unwrap();
        let s = lib.state.lock().unwrap();
        assert_eq!(s.frozen.contains(&id), verified);
        assert_eq!(s.operations[&id]["failed"], !verified);
        if !verified {
            assert!(
                s.operations[&id]["message"]
                    .as_str()
                    .unwrap()
                    .contains("读取测试凭证失败")
            );
        }
    }
}

#[test]
fn freeze_rechecks_selection_and_pending_windows_before_stopping() {
    let (lib, _windows) = library();
    let id = create(&lib, "有窗口");
    let i = ready(&lib, &id);
    let freeze = || {
        lib.freeze_idle(&i, || panic!("活跃实例不能被停止"))
            .unwrap()
    };
    age_interaction(&lib, &id);
    freeze();
    create(&lib, "第二台");
    age_interaction(&lib, &id);
    lib.state.lock().unwrap().opening.insert((id.clone(), None));
    freeze();
    assert_eq!(i.handle.snapshot().state, State::Ready);
}

#[test]
fn stop_clears_only_the_target_and_ignores_observations_from_its_old_boot() {
    let (lib, windows) = library();
    let a = create(&lib, "实例 A");
    let i = ready(&lib, &a);
    let b = create(&lib, "实例 B");
    ready(&lib, &b);
    let package = "com.example.reader";
    for id in [&a, &b] {
        lib.perform(&json!({"kind":"launch","instanceId":id,"package":package}))
            .unwrap();
    }
    let old_seq = i.handle.snapshot().seq;
    let key = WindowKey {
        instance_id: a.clone(),
        package: Some(package.into()),
    };
    lib.perform(&json!({"kind":"stop","instanceId":a})).unwrap();
    assert!(
        matches!(windows.recv_timeout(Duration::from_secs(1)), Ok(WindowCommand::CloseInstance{instance_id}) if instance_id==a)
    );
    ready(&lib, &a);
    lib.window_event(WindowEvent::Closed { key: key.clone() });
    lib.window_event(WindowEvent::Opened { key: key.clone() });
    lib.window_event(WindowEvent::FirstFrame { key });
    lib.observe_tasks(
        &a,
        old_seq,
        Ok(vec![runtime::running::AppTask {
            package: package.into(),
            task_id: 42,
            display_id: 2,
            resumed: true,
        }]),
    );
    lib.observe_tasks(&a, old_seq, Err("上一轮的 adb 读取失败".into()));
    assert_eq!(i.handle.snapshot().facts.session_count, 0);
    let s = lib.state.lock().unwrap();
    assert!(
        !s.open
            .iter()
            .chain(&s.opening)
            .chain(&s.streaming)
            .any(|(id, _)| id == &a)
    );
    assert!(!s.tasks.contains_key(&a));
    assert!(!s.task_errors.contains_key(&a));
    assert!(s.open.contains(&(b.clone(), Some(package.into()))));
    assert!(s.tasks[&b].contains(package));
}

#[test]
fn refusing_a_stop_preserves_live_windows_and_running_tasks() {
    struct RejectStop(device::FakeBackend);
    impl device::Backend for RejectStop {
        fn exec(&mut self, cmd: &Command, facts: &mut Facts) -> Result<(), Rejection> {
            if matches!(cmd, Command::Stop) {
                return Err(error("测试后端拒绝停止"));
            }
            self.0.exec(cmd, facts)
        }
        fn observe(&mut self, facts: &mut Facts) {
            self.0.observe(facts);
        }
        fn abort(&mut self, facts: &mut Facts) {
            self.0.abort(facts);
        }
        fn apps(&self) -> Vec<AppEntry> {
            self.0.apps()
        }
    }
    let (lib, windows) = library();
    let id = "reject-stop";
    let (handle, events) = device::spawn(
        InstanceId::new(id),
        id.into(),
        RejectStop(device::FakeBackend::new()),
        Default::default(),
    );
    lib.add(id.into(), handle, events, None);
    let spec = fake_spec();
    let i = lib.state.lock().unwrap().instances[id].clone();
    assert!(matches!(
        i.handle.send(Command::Create { spec }),
        Ack::Accepted { .. }
    ));
    ready(&lib, id);
    lib.perform(&json!({"kind":"launch","instanceId":id,"package":"com.example.reader"}))
        .unwrap();
    assert!(
        lib.perform(&json!({"kind":"stop","instanceId":id}))
            .is_err()
    );
    assert!(windows.try_recv().is_err());
    assert_eq!(i.handle.snapshot().facts.session_count, 1);
    let s = lib.state.lock().unwrap();
    assert!(
        s.open
            .contains(&(id.into(), Some("com.example.reader".into())))
    );
    assert!(s.tasks[id].contains("com.example.reader"));
}

#[test]
fn fast_reopen_survives_previous_close_and_the_next_stop_batch() {
    let id = "window-protocol";
    let (tx, commands) = mpsc::channel();
    let layout = Layout {
        root: PathBuf::default(),
    };
    let mut lib = Library::new(HostChannel::new(), layout.clone(), true, tx, false);
    // 只切换原生窗口消息路径。设备使用内存后端，测试端逐条送回窗口事件。
    Arc::get_mut(&mut lib).unwrap().store = Some(InstanceStore::new(layout));
    let (handle, events) = device::spawn(
        InstanceId::new(id),
        id.into(),
        device::FakeBackend::new(),
        Default::default(),
    );
    lib.add(id.into(), handle.clone(), events, None);
    assert!(matches!(
        handle.send(Command::Create { spec: fake_spec() }),
        Ack::Accepted { .. }
    ));
    ready(&lib, id);
    let package = "com.example.reader";
    let key = WindowKey {
        instance_id: id.into(),
        package: Some(package.into()),
    };
    let action = |kind| {
        lib.perform(&json!({"kind":kind,"instanceId":id,"package":package}))
            .unwrap()
    };
    let recv = || commands.recv_timeout(Duration::from_secs(1)).unwrap();
    let opened = || lib.window_event(WindowEvent::Opened { key: key.clone() });
    let frame = || lib.window_event(WindowEvent::FirstFrame { key: key.clone() });
    let closed = || lib.window_event(WindowEvent::Closed { key: key.clone() });

    action("launch");
    assert!(matches!(recv(), WindowCommand::Open { .. }));
    opened();
    frame();
    let refocus = || {
        action("backgroundApp");
        assert!(matches!(recv(), WindowCommand::Background { .. }));
        action("focusApp");
        assert!(matches!(recv(), WindowCommand::Open { .. }));
    };
    refocus();
    closed();
    opened();
    frame();
    assert_eq!(handle.snapshot().facts.session_count, 1);

    let old_seq = handle.snapshot().seq;
    refocus();
    action("stop");
    assert!(matches!(recv(), WindowCommand::CloseInstance { .. }));
    ready(&lib, id);
    action("focusApp");
    assert!(matches!(recv(), WindowCommand::Open { .. }));
    // 主线程先关闭旧窗口，再创建此前提交的重开窗口，最后执行整台实例的关闭。
    closed();
    opened();
    frame();
    closed();
    assert_eq!(handle.snapshot().facts.session_count, 0);
    let index = (id.into(), Some(package.into()));
    lib.window_event(WindowEvent::QuitRequested { key: key.clone() });
    lib.window_event(WindowEvent::Error {
        instance_id: id.into(),
        message: "旧窗口错误".into(),
    });
    {
        let s = lib.state.lock().unwrap();
        assert!(s.opening.contains(&index));
        assert!(!s.operations.contains_key(id));
    }
    lib.window_event(WindowEvent::InstanceClosed {
        instance_id: id.into(),
    });
    opened();
    frame();
    assert_eq!(handle.snapshot().facts.session_count, 1);
    action("focusApp");
    assert!(matches!(recv(), WindowCommand::Open { .. }));
    opened();
    let s = lib.state.lock().unwrap();
    assert!(!s.opening.contains(&index));
    assert!(s.open.contains(&index));
    drop(s);
    lib.perform(&json!({"kind":"quitApp","instanceId":id,"package":package,"windowSeq":old_seq}))
        .unwrap();
    assert!(matches!(recv(), WindowCommand::QuitFailed { .. }));
    assert_eq!(handle.snapshot().facts.session_count, 1);
}

fn temporary_workspace() -> Layout {
    use std::{fs, time::SystemTime};
    let token = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("windroid-workspace-{}-{token}", std::process::id()));
    fs::create_dir(&root).unwrap();
    Layout { root }
}

fn stored_spec() -> contract::Spec {
    contract::Spec {
        template: "compact-720".into(),
        image: contract::Image {
            api: 34,
            tag: "google_apis_playstore".into(),
            abi: "arm64-v8a".into(),
        },
        disk_bytes: 6 << 30,
    }
}

#[test]
fn reopened_workspace_reports_each_instances_saved_config_and_storage_paths() {
    let layout = temporary_workspace();
    let root = layout.root.clone();
    let store = InstanceStore::new(layout.clone());
    let spec = stored_spec();
    let a = store.create("副屏", spec.clone()).unwrap();
    let b = store
        .create(
            "主屏",
            contract::Spec {
                template: "phone-1080".into(),
                disk_bytes: 8 << 30,
                ..spec
            },
        )
        .unwrap();
    let (tx, _) = mpsc::channel();
    let (_events, rx) = mpsc::channel();
    let lib = Library::attach(HostChannel::new(), layout, false, tx, rx).unwrap();
    until(|| lib.snapshot()["state"] == "stopped");
    let snap = lib.snapshot();
    let rows = snap["instances"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(snap["selectedId"], b.id.as_str());
    let config = |id: &InstanceId| &rows.iter().find(|r| r["id"] == id.as_str()).unwrap()["config"];
    assert_eq!(config(&a.id)["screenWidth"], 1280);
    assert_eq!(config(&b.id)["screenWidth"], 1080);
    assert_eq!(config(&a.id)["ramMiB"], 1536);
    assert_eq!(config(&b.id)["cpuCores"], 2);
    assert_eq!(config(&b.id)["diskBytes"], 8_u64 << 30);
    assert_eq!(config(&a.id)["image"]["api"], 34);
    assert_eq!(snap["storage"]["home"], json!(root));
    let sdk = RuntimePaths::discover(&root).sdk;
    assert_eq!(snap["storage"]["sdk"], json!(sdk));
    assert_eq!(snap["storage"]["images"], json!(sdk.join("system-images")));
    lib.shutdown();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn missing_runtime_blocks_create_and_start_but_keeps_selection_available() {
    let root =
        std::env::temp_dir().join(format!("windroid-missing-runtime-{}", std::process::id()));
    let (tx, _) = mpsc::channel();
    let mut lib = Library::new(
        HostChannel::new(),
        Layout { root: root.clone() },
        true,
        tx,
        false,
    );
    Arc::get_mut(&mut lib).unwrap().runtime = Some(Provisioner::managed(&root));
    assert_ne!(lib.snapshot()["preparation"]["phase"], "ready");
    assert_eq!(lib.snapshot()["capabilities"]["create"], false);
    assert!(
        lib.perform(&json!({"kind":"createInstance", "name":"新实例"}))
            .is_err()
    );

    let id = "existing";
    let (handle, events) = device::spawn(
        InstanceId::new(id),
        id.into(),
        device::FakeBackend::new(),
        Default::default(),
    );
    assert!(matches!(
        handle.send(Command::Create { spec: fake_spec() }),
        Ack::Accepted { .. }
    ));
    lib.add(id.into(), handle, events, None);
    lib.perform(&json!({"kind":"selectInstance","instanceId":id}))
        .unwrap();
    let snap = lib.snapshot();
    assert_eq!(snap["selectedId"], id);
    assert_eq!(snap["state"], "stopped");
    assert_eq!(snap["capabilities"]["start"], false);
    assert_eq!(snap["capabilities"]["delete"], true);
    assert!(
        lib.perform(&json!({"kind":"start", "instanceId":id}))
            .is_err()
    );
}
