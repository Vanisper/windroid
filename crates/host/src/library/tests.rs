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
    lib.perform(&json!({"kind":"createInstance","name":name,"template":"compact-720"}))
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
fn picker_keeps_its_instance_when_selection_changes() {
    let (lib, windows) = library();
    let a = create(&lib, "平板");
    lib.perform(&json!({"kind":"start","instanceId":a}))
        .unwrap();
    until(|| lib.snapshot()["capabilities"]["install"] == true);
    lib.perform(&json!({"kind":"addApk","instanceId":a}))
        .unwrap();
    assert!(
        matches!(windows.recv().unwrap(), WindowCommand::PickApk{instance_id} if instance_id==a)
    );
    let b = create(&lib, "手机");
    lib.window_event(WindowEvent::ApkPicked {
        instance_id: a.clone(),
        path: "/fixture/two words.apk".into(),
    });
    until(|| {
        lib.state
            .lock()
            .unwrap()
            .operations
            .get(&a)
            .is_some_and(|op| op["message"].as_str().is_some_and(|s| s.contains("APK")))
    });
    assert_eq!(lib.snapshot()["selectedId"], b);
    assert_ne!(lib.snapshot()["operation"]["instanceId"], a);
    assert!(
        lib.perform(&json!({"kind":"install","apkPath":"/fixture/package.apk"}))
            .is_err()
    );
}

#[test]
fn background_close_and_quit_have_distinct_observable_outcomes() {
    let (lib, _) = library();
    let id = create(&lib, "手机");
    lib.perform(&json!({"kind":"start","instanceId":id}))
        .unwrap();
    until(|| lib.snapshot()["state"] == "ready");
    let action = |kind: &str| {
        lib.perform(&json!({"kind":kind,"instanceId":id,"package":"com.example.reader"}))
            .unwrap()
    };
    action("launch");
    action("launch");
    assert_eq!(lib.snapshot()["facts"]["sessionCount"], 1);
    action("backgroundApp");
    assert_eq!(lib.snapshot()["running"][0]["windowOpen"], false);
    assert_eq!(lib.snapshot()["facts"]["sessionCount"], 0);
    action("focusApp");
    assert_eq!(lib.snapshot()["running"][0]["windowOpen"], true);
    action("quitApp");
    assert!(lib.snapshot()["running"].as_array().unwrap().is_empty());
}

#[test]
fn deleting_a_group_restores_source_without_affecting_another_instance() {
    let (lib, _) = library();
    let id = create(&lib, "主机");
    lib.perform(&json!({"kind":"createGroup","instanceId":id,"name":"书架"}))
        .unwrap();
    let group = lib.snapshot()["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "书架")
        .unwrap()["id"]
        .clone();
    lib.perform(
        &json!({"kind":"moveApp","instanceId":id,"package":"com.example.reader","groupId":group}),
    )
    .unwrap();
    assert_eq!(
        lib.snapshot()["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["id"] == group)
            .unwrap()["count"],
        1
    );
    let other = create(&lib, "副机");
    assert!(
        !lib.snapshot()["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["id"] == group)
    );
    lib.perform(&json!({"kind":"deleteGroup","instanceId":id,"groupId":group}))
        .unwrap();
    assert_eq!(lib.snapshot()["selectedId"], other);
    lib.perform(&json!({"kind":"selectInstance","instanceId":id}))
        .unwrap();
    assert_eq!(
        lib.snapshot()["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["id"] == "installed")
            .unwrap()["count"],
        1
    );
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
    let mut lib = Library::new(HostChannel::new(), layout.clone(), true, tx, false).unwrap();
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
