use super::*;

fn accepts_window(state: &ViewState, id: &str) -> bool {
    !state.closing.contains_key(id)
        && state
            .instances
            .get(id)
            .is_some_and(|i| i.handle.snapshot().state.accepts_stream())
}

impl Library {
    pub(super) fn watch_device(self: &Arc<Self>, id: String, events: Events) {
        let weak = Arc::downgrade(self);
        thread::spawn(move || {
            while let Some(event) = events.recv() {
                let Some(lib) = weak.upgrade() else { break };
                if matches!(
                    event,
                    contract::Event::StateChanged {
                        to: State::Stopped | State::Failed | State::Absent,
                        ..
                    }
                ) {
                    lib.clear_stopped_instance(&id);
                }
                lib.notify();
            }
        });
    }

    /// 同步释放宿主记录的配额。终止事件可能晚于重新启动，所以在锁内读取当前设备状态。
    pub(super) fn clear_stopped_instance(&self, id: &str) {
        let mut s = self.state.lock().unwrap();
        let Some(i) = s.instances.get(id) else { return };
        if !matches!(
            i.handle.snapshot().state,
            State::Stopping | State::Stopped | State::Failed | State::Absent
        ) {
            return;
        }
        self.clear_session_locked(&mut s, id);
    }

    /// 已成功替换进程，即使新一轮已开始引导，也要关闭属于上一轮的窗口。
    pub(super) fn clear_replaced_session(&self, id: &str) {
        self.clear_session_locked(&mut self.state.lock().unwrap(), id);
    }

    fn clear_session_locked(&self, s: &mut ViewState, id: &str) {
        s.tasks.remove(id);
        s.task_errors.remove(id);
        let had_window = s.open.iter().chain(&s.opening).any(|k| k.0 == id);
        s.opening.retain(|k| k.0 != id);
        s.open.retain(|k| k.0 != id);
        s.streaming.retain(|k| k.0 != id);
        if had_window
            && self
                .windows
                .send(WindowCommand::CloseInstance {
                    instance_id: id.into(),
                })
                .is_ok()
            && self.store.is_some()
        {
            *s.closing.entry(id.into()).or_default() += 1;
        }
    }

    pub(super) fn watch_preparation(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        thread::spawn(move || {
            let mut previous = JsonValue::Null;
            loop {
                let Some(lib) = weak.upgrade() else { break };
                if lib.stopping.load(Ordering::Acquire) {
                    break;
                }
                let view = lib.preparation();
                if previous != view {
                    previous = view;
                    lib.notify();
                }
                drop(lib);
                thread::sleep(Duration::from_millis(350));
            }
        });
    }

    pub(super) fn watch_tasks(self: &Arc<Self>, id: String) {
        let weak = Arc::downgrade(self);
        thread::spawn(move || {
            loop {
                let Some(lib) = weak.upgrade() else { break };
                if lib.stopping.load(Ordering::Acquire) {
                    break;
                }
                let instance = lib.state.lock().unwrap().instances.get(&id).cloned();
                let Some(i) = instance else { break };
                let observed = i.handle.snapshot();
                if let Some(record) = &i.record
                    && observed.state.accepts_stream()
                    && let Some(resources) = i.active.current()
                {
                    let serial = record.serial();
                    let device = resources.device(&serial);
                    let result = record
                        .verify_device(&resources.paths.adb)
                        .map_err(|e| e.to_string())
                        .and_then(|_| runtime::running::snapshot(device));
                    for package in lib.observe_tasks(&id, observed.seq, result) {
                        if i.handle.snapshot().seq == observed.seq
                            && record.verify_device(&resources.paths.adb).is_ok()
                            && let Err(reason) =
                                runtime::running::clear_inactive_recents(device, &package)
                        {
                            lib.note(Some(&id), true, reason);
                        }
                    }
                }
                if i.handle.snapshot().state == State::Ready && lib.idle(&id) {
                    lib.enqueue(&id, json!({"kind":"freezeIdle"}));
                }
                drop(lib);
                thread::sleep(Duration::from_secs(2));
            }
        });
    }

    /// 设备查询可能跨过停止、重启或强停。只有发起查询时的设备修订仍然有效才写入应用库。
    pub(super) fn observe_tasks(
        &self,
        id: &str,
        observed: u64,
        result: Result<Vec<runtime::running::AppTask>, String>,
    ) -> Vec<String> {
        let mut s = self.state.lock().unwrap();
        let Some(i) = s.instances.get(id) else {
            return Vec::new();
        };
        let snap = i.handle.snapshot();
        if snap.seq != observed || !snap.state.accepts_stream() {
            return Vec::new();
        }
        let (changed, ended) = match result {
            Err(reason) => {
                let changed = s.task_errors.get(id) != Some(&reason);
                s.task_errors.insert(id.into(), reason);
                (changed, Vec::new())
            }
            Ok(tasks) => {
                let visible: BTreeSet<_> = tasks.iter().map(|t| t.package.clone()).collect();
                let recovered = s.task_errors.remove(id).is_some();
                let previous = s.tasks.insert(id.into(), visible.clone());
                let changed = recovered || previous.as_ref() != Some(&visible);
                let ended = previous
                    .into_iter()
                    .flatten()
                    .filter(|p| {
                        !visible.contains(p) && s.open.contains(&(id.into(), Some(p.clone())))
                    })
                    .collect();
                let _ = self.windows.send(WindowCommand::Reconcile {
                    instance_id: id.into(),
                    tasks: tasks
                        .into_iter()
                        .map(|t| (t.package, t.display_id))
                        .collect(),
                });
                (changed, ended)
            }
        };
        drop(s);
        if changed {
            self.notify();
        }
        ended
    }

    pub(super) fn window_event(&self, event: WindowEvent) {
        match event {
            WindowEvent::Opened { key } => {
                let index = (key.instance_id, key.package);
                let mut s = self.state.lock().unwrap();
                if !accepts_window(&s, &index.0) || !s.opening.remove(&index) {
                    return;
                }
                s.open.insert(index);
            }
            WindowEvent::FirstFrame { key } => {
                let id = key.instance_id;
                let mut s = self.state.lock().unwrap();
                let index = (id.clone(), key.package);
                if !s.open.contains(&index)
                    || !accepts_window(&s, &id)
                    || !s.streaming.insert(index)
                {
                    return;
                }
                s.operations.insert(
                    id.clone(),
                    json!({"instanceId":id,"message":"窗口已连接","failed":false}),
                );
                drop(s);
                self.update_sessions(&id);
            }
            WindowEvent::Closed { key } => {
                let index = (key.instance_id, key.package);
                let mut s = self.state.lock().unwrap();
                if s.closing.contains_key(&index.0) {
                    return;
                }
                let open = s.open.remove(&index);
                // 旧窗口的 Closed 可能在下一次 Open 递交后才到。原生管理器会给新请求独立回执。
                let opening = !open && s.opening.remove(&index);
                let streaming = s.streaming.remove(&index);
                if !opening && !open && !streaming {
                    return;
                }
                s.last_interaction.insert(index.0.clone(), Instant::now());
                drop(s);
                if streaming {
                    self.update_sessions(&index.0);
                }
            }
            WindowEvent::InstanceClosed { instance_id } => {
                // 关闭批次包含的窗口数量由原生管理器决定。在主线程发出批次回执前，
                // 即使设备已再次就绪，队列中旧窗口的事件仍属于上次运行。
                let mut s = self.state.lock().unwrap();
                if let Some(pending) = s.closing.get_mut(&instance_id) {
                    *pending -= 1;
                    if *pending == 0 {
                        s.closing.remove(&instance_id);
                    }
                }
                return;
            }
            WindowEvent::QuitRequested { key } => {
                let s = self.state.lock().unwrap();
                if !s
                    .open
                    .contains(&(key.instance_id.clone(), key.package.clone()))
                    || !accepts_window(&s, &key.instance_id)
                {
                    return;
                }
                let seq = s.instances[&key.instance_id].handle.snapshot().seq;
                drop(s);
                self.enqueue(
                    &key.instance_id,
                    json!({"kind":"quitApp","package":key.package,"windowSeq":seq}),
                );
            }
            WindowEvent::ApkPicked { instance_id, path } => {
                self.enqueue(&instance_id, json!({"kind":"install","apkPath":path}))
            }
            WindowEvent::Error {
                instance_id,
                message,
            } => {
                let mut s = self.state.lock().unwrap();
                if !accepts_window(&s, &instance_id) {
                    return;
                }
                s.operations.insert(
                    instance_id.clone(),
                    json!({"instanceId":instance_id,"message":message,"failed":true}),
                );
            }
        }
        self.notify();
    }

    fn update_sessions(&self, id: &str) {
        let count =
            |state: &ViewState| state.streaming.iter().filter(|key| key.0 == id).count() as u32;
        loop {
            let (handle, reported) = {
                let s = self.state.lock().unwrap();
                let Some(instance) = s.instances.get(id) else {
                    return;
                };
                (instance.handle.clone(), count(&s))
            };
            // actor 可能还在处理 Stop，等回执时绝不持有库锁。若期间已关窗，纠正刚上报的旧计数。
            let _ = handle.set_session_count(reported);
            if count(&self.state.lock().unwrap()) == reported {
                return;
            }
        }
    }

    pub(super) fn release_quit(&self, data: &JsonValue) {
        if data["kind"] == "quitApp"
            && let (Some(id), Some(pkg)) = (data["instanceId"].as_str(), data["package"].as_str())
        {
            let key = WindowKey {
                instance_id: id.into(),
                package: Some(pkg.into()),
            };
            let _ = self.windows.send(WindowCommand::QuitFailed { key });
        }
    }

    /// 文件选择器和原生关窗均携带打开时的实例身份。
    fn enqueue(&self, id: &str, mut data: JsonValue) {
        data["instanceId"] = json!(id);
        let instance = self.state.lock().unwrap().instances.get(id).cloned();
        let Some(i) = instance else {
            self.release_quit(&data);
            return;
        };
        if let Err(e) = i.lane.try_send(Request { data, reply: None }) {
            let (mpsc::TrySendError::Full(req) | mpsc::TrySendError::Disconnected(req)) = e;
            self.release_quit(&req.data);
            self.note(Some(id), true, "操作队列繁忙，请重试");
        }
    }
}
