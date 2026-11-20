use super::*;

fn commit(lib: &Arc<Library>, preview: &JsonValue) -> Reply {
    lib.perform(&json!({"kind":"executeBatch","previewId":preview["previewId"]}))
}

fn result(lib: &Arc<Library>) -> JsonValue {
    until(|| lib.snapshot()["batch"]["active"] == false);
    lib.snapshot()["batch"].clone()
}

fn add_backend(
    lib: &Arc<Library>,
    id: &str,
    backend: impl device::Backend,
    timeouts: device::Timeouts,
) {
    let (handle, events) = device::spawn(InstanceId::new(id), id.into(), backend, timeouts);
    assert!(matches!(
        handle.send(Command::Create { spec: fake_spec() }),
        Ack::Accepted { .. }
    ));
    lib.add(id.into(), handle, events, None, Default::default());
}

#[test]
fn reviewed_targets_survive_recheck_and_state_changes_without_expanding() {
    let (lib, _) = library();
    let a = create(&lib, "A");
    let b = create(&lib, "B");
    let plan = lib
        .preview_batch(&json!({"action":"start","scope":"all","ids":[b,a,b]}))
        .unwrap();
    assert_eq!(plan["items"][0]["name"], "A");
    assert_eq!(plan["items"][1]["name"], "B");
    let c = create(&lib, "C");
    let checked = lib
        .preview_batch(&json!({"previewId":plan["previewId"]}))
        .unwrap();
    assert_ne!(checked["previewId"], plan["previewId"]);
    assert_eq!(checked["items"], plan["items"]);
    assert!(commit(&lib, &plan).is_err());

    ready(&lib, &b);
    commit(&lib, &checked).unwrap();
    assert!(commit(&lib, &checked).is_err());
    let job = result(&lib);
    assert_eq!(job["scope"], "all");
    assert_eq!(job["items"].as_array().unwrap().len(), 2);
    assert_eq!(job["items"][0]["status"], "succeeded");
    assert_eq!(job["items"][1]["status"], "skipped");
    assert_eq!(job["items"][1]["message"], "已在运行");
    assert!(lib.preview_batch(&json!({"retryOf":job["jobId"]})).is_err());
    assert_eq!(
        lib.state.lock().unwrap().instances[&c]
            .handle
            .snapshot()
            .state,
        State::Stopped
    );
    assert_eq!(lib.snapshot()["selectedId"], c);
}

#[test]
fn stop_receipt_scopes_windows_and_retry_to_the_failed_instance() {
    struct FailFirstStop {
        inner: device::FakeBackend,
        failed: bool,
    }
    impl device::Backend for FailFirstStop {
        fn exec(&mut self, cmd: &Command, facts: &mut Facts) -> Result<(), Rejection> {
            if matches!(cmd, Command::Stop) && !self.failed {
                self.failed = true;
                return Err(error("客户机刷盘失败"));
            }
            self.inner.exec(cmd, facts)
        }
        fn observe(&mut self, facts: &mut Facts) {
            self.inner.observe(facts);
        }
        fn abort(&mut self, facts: &mut Facts) {
            self.inner.abort(facts);
        }
        fn apps(&self) -> Vec<AppEntry> {
            self.inner.apps()
        }
    }
    let (lib, windows) = library();
    let b = "partial-stop";
    add_backend(
        &lib,
        b,
        FailFirstStop {
            inner: device::FakeBackend::new(),
            failed: false,
        },
        Default::default(),
    );
    let a = create(&lib, "A");
    let c = create(&lib, "C");
    let unselected = create(&lib, "D");
    for id in [a.as_str(), b, unselected.as_str()] {
        ready(&lib, id);
        lib.perform(&json!({"kind":"openDevice","instanceId":id}))
            .unwrap();
        lib.perform(&json!({"kind":"launch","instanceId":id,"package":"com.example.reader"}))
            .unwrap();
    }
    let plan = lib
        .preview_batch(&json!({"action":"stop","scope":"selected","ids":[b,c,a]}))
        .unwrap();
    let rows = plan["items"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    for id in [a.as_str(), b] {
        let row = rows.iter().find(|r| r["id"] == id).unwrap();
        assert_eq!(row["windows"], json!(["设备视图", "阅读器"]));
        assert_eq!(row["eligible"], true);
    }
    let skipped = rows.iter().find(|r| r["id"] == c).unwrap();
    assert_eq!(skipped["reason"], "已停止");
    assert_eq!(skipped["eligible"], false);
    commit(&lib, &plan).unwrap();
    let receipt = result(&lib);
    let rows = receipt["items"].as_array().unwrap();
    let row = |id: &str| rows.iter().find(|r| r["id"] == id).unwrap();
    assert_eq!(row(&a)["status"], "succeeded");
    assert_eq!(row(b)["status"], "failed");
    assert_eq!(row(b)["message"], "客户机刷盘失败");
    assert_eq!(row(&c)["status"], "skipped");
    assert!(
        matches!(windows.recv().unwrap(), WindowCommand::CloseInstance { instance_id } if instance_id == a)
    );
    assert!(windows.try_recv().is_err());
    for id in [b, unselected.as_str()] {
        let state = lib.state.lock().unwrap();
        assert!(state.tasks[id].contains("com.example.reader"));
        assert_eq!(state.instances[id].handle.snapshot().facts.session_count, 2);
    }

    ready(&lib, &a);
    ready(&lib, &c);
    let retry = lib
        .preview_batch(&json!({"retryOf":receipt["jobId"]}))
        .unwrap();
    assert_eq!(retry["scope"], "retry");
    assert_eq!(retry["items"].as_array().unwrap().len(), 1);
    assert_eq!(retry["items"][0]["id"], b);
    commit(&lib, &retry).unwrap();
    assert_eq!(result(&lib)["items"][0]["status"], "succeeded");
    assert!(
        matches!(windows.recv().unwrap(), WindowCommand::CloseInstance { instance_id } if instance_id == b)
    );
    let state = lib.state.lock().unwrap();
    assert!(state.instances[&a].handle.snapshot().state.accepts_stream());
    assert!(state.instances[&c].handle.snapshot().state.accepts_stream());
    assert_eq!(
        state.instances[&unselected]
            .handle
            .snapshot()
            .facts
            .session_count,
        2
    );
    assert!(!state.tasks.contains_key(b));
}

#[test]
fn a_start_ack_is_not_success_when_the_guest_dies_or_times_out() {
    let (lib, _) = library();
    let deadlines = device::Timeouts {
        hard: Duration::from_millis(300),
        adb: Duration::from_millis(30),
        services: Duration::from_millis(50),
        poll: Duration::from_millis(5),
    };
    add_backend(&lib, "dies", device::FakeBackend::new().dying(), deadlines);
    add_backend(
        &lib,
        "stalls",
        device::FakeBackend::new().stalling(),
        deadlines,
    );
    let plan = lib
        .preview_batch(&json!({"action":"start","scope":"selected","ids":["stalls","dies"]}))
        .unwrap();
    commit(&lib, &plan).unwrap();
    until(|| {
        let job = &lib.snapshot()["batch"];
        job["active"] == true && job["items"][1]["status"] == "running"
    });
    assert!(
        lib.preview_batch(&json!({"action":"stop","scope":"all","ids":["stalls"]}))
            .is_err()
    );
    let job = result(&lib);
    assert_eq!(job["items"][0]["status"], "failed");
    assert_eq!(job["items"][0]["message"], "模拟器在就绪前退出");
    assert_eq!(job["items"][1]["status"], "failed");
    assert!(
        job["items"][1]["message"]
            .as_str()
            .unwrap()
            .contains("超过时限")
    );
    // 失败态再次启动并当即退出，事实可能完全相同，仍应从命令完成时的快照给出结果。
    let retry = lib.preview_batch(&json!({"retryOf":job["jobId"]})).unwrap();
    commit(&lib, &retry).unwrap();
    assert_eq!(result(&lib)["items"][0]["status"], "failed");
}

#[test]
fn restart_during_a_stalled_batch_start_cannot_complete_the_original_boot() {
    struct ReplaceBoot {
        inner: device::FakeBackend,
        reject: bool,
    }
    impl device::Backend for ReplaceBoot {
        fn exec(&mut self, cmd: &Command, facts: &mut Facts) -> Result<(), Rejection> {
            if matches!(cmd, Command::Restart { .. }) {
                if self.reject {
                    self.reject = false;
                    return Err(error("本轮重启预检失败"));
                }
                self.inner = device::FakeBackend::new();
            }
            self.inner.exec(cmd, facts)
        }
        fn observe(&mut self, facts: &mut Facts) {
            self.inner.observe(facts);
        }
        fn abort(&mut self, facts: &mut Facts) {
            self.inner.abort(facts);
        }
    }
    let (lib, _) = library();
    let id = "slow-boot";
    add_backend(
        &lib,
        id,
        ReplaceBoot {
            inner: device::FakeBackend::new().stalling(),
            reject: true,
        },
        device::Timeouts {
            services: Duration::from_millis(60),
            poll: Duration::from_millis(10),
            ..Default::default()
        },
    );
    let plan = lib
        .preview_batch(&json!({"action":"start","scope":"all","ids":[id]}))
        .unwrap();
    commit(&lib, &plan).unwrap();
    let handle = lib.state.lock().unwrap().instances[id].handle.clone();
    until(|| handle.snapshot().state == State::Stalled);
    let restart = json!({"kind":"restart","instanceId":id});
    assert!(lib.perform(&restart).is_err());
    assert_eq!(lib.snapshot()["batch"]["active"], true);
    lib.perform(&restart).unwrap();
    let job = result(&lib);
    assert_eq!(job["items"][0]["status"], "failed");
    assert_eq!(job["items"][0]["message"], "本次启动已被重新启动替换");
    until(|| handle.snapshot().state == State::Ready);
}

#[test]
fn batch_stop_reports_process_exit_after_a_ready_guest_dies() {
    struct CrashAfterReady {
        inner: device::FakeBackend,
        crash: Arc<AtomicBool>,
    }
    impl device::Backend for CrashAfterReady {
        fn exec(&mut self, cmd: &Command, facts: &mut Facts) -> Result<(), Rejection> {
            self.inner.exec(cmd, facts)
        }
        fn observe(&mut self, facts: &mut Facts) {
            if self.crash.load(Ordering::Acquire) {
                self.inner.abort(facts);
            } else {
                self.inner.observe(facts);
            }
        }
        fn abort(&mut self, facts: &mut Facts) {
            self.inner.abort(facts);
        }
    }
    let (lib, _) = library();
    let id = "exited";
    let crash = Arc::new(AtomicBool::new(false));
    add_backend(
        &lib,
        id,
        CrashAfterReady {
            inner: device::FakeBackend::new(),
            crash: crash.clone(),
        },
        Default::default(),
    );
    let instance = ready(&lib, id);
    let request = json!({"scope":"selected","action":"stop","ids":[id]});
    let plan = lib.preview_batch(&request).unwrap();
    assert_eq!(plan["items"][0]["eligible"], true);
    crash.store(true, Ordering::Release);
    until(|| instance.handle.snapshot().state == State::Failed);
    commit(&lib, &plan).unwrap();
    let receipt = result(&lib);
    assert_eq!(receipt["items"][0]["status"], "skipped");
    assert_eq!(receipt["items"][0]["message"], "模拟器进程已退出");
    assert!(
        lib.preview_batch(&json!({"retryOf":receipt["jobId"]}))
            .is_err()
    );
    let preview = lib.preview_batch(&request).unwrap();
    assert_eq!(preview["items"][0]["eligible"], false);
    assert_eq!(preview["items"][0]["reason"], "模拟器进程已退出");
}
