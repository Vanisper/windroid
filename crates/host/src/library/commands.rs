use super::*;
use crate::actions::{self, Kind};

impl Library {
    pub(super) fn perform(self: &Arc<Self>, data: &JsonValue) -> Reply {
        if self.stopping.load(Ordering::Acquire) {
            return Err(error("应用库正在退出"));
        }
        let kind = data["kind"].as_str().ok_or_else(|| error("缺少操作名称"))?;
        match kind {
            "prepareRuntime" | "pausePreparation" | "recheckRuntime" => {
                let p = self
                    .runtime
                    .as_ref()
                    .ok_or_else(|| error("演示环境已就绪"))?;
                match kind {
                    "prepareRuntime" if data["acceptedLicenses"] != true => {
                        return Err(error("请先阅读并接受组件许可"));
                    }
                    "prepareRuntime" => p.start(),
                    "pausePreparation" => p.pause(),
                    _ => p.recheck(),
                }
                .map_err(error)?;
                return Ok(self.revision());
            }
            "createInstance" => return self.create_instance(data),
            _ => {}
        }
        let instance = {
            let s = self.state.lock().unwrap();
            let id = data["instanceId"]
                .as_str()
                .or_else(|| {
                    if self.fixture {
                        s.selected.as_deref()
                    } else {
                        None
                    }
                })
                .ok_or_else(|| error("请选择目标实例"))?;
            s.instances
                .get(id)
                .cloned()
                .ok_or_else(|| error("目标实例已不存在"))?
        };
        let id = instance.handle.id().as_str();
        if kind != "freezeIdle" {
            self.state
                .lock()
                .unwrap()
                .last_interaction
                .insert(id.into(), Instant::now());
        }
        let snap = instance.handle.snapshot();
        let caps = contract::Capabilities::of(snap.state);
        let package = data["package"].as_str().unwrap_or("");
        match kind {
            "freezeIdle" => {
                return self.freeze_idle(&instance, || match &self.store {
                    Some(store) => store.quick_boot_saved(instance.handle.id()),
                    None => Ok(false),
                });
            }
            "selectInstance" => {
                if let Some(store) = &self.store {
                    store.select(instance.handle.id()).map_err(error)?;
                }
                self.select(id);
            }
            "deleteInstance" => {
                if !caps.delete {
                    return Err(error("请先停止实例"));
                }
                self.device(&instance, Command::Delete)?;
                self.remove(id)?;
            }
            "createGroup" | "moveApp" | "deleteGroup" => {
                if kind == "moveApp" {
                    app(&snap, package)?;
                }
                let _writer = self.group_writer.lock().unwrap();
                let mut groups = self.groups.lock().unwrap().clone();
                groups
                    .change(
                        self.store.as_ref().map(|_| self.layout.root.as_path()),
                        id,
                        kind,
                        data,
                    )
                    .map_err(error)?;
                *self.groups.lock().unwrap() = groups;
            }
            "openDevice" | "launch" | "focusApp" => {
                if !caps.launch_app {
                    return Err(Rejection::NotReady { state: snap.state });
                }
                let target = if kind == "openDevice" {
                    None
                } else {
                    Some(app(&snap, package)?)
                };
                self.open(&instance, &snap, target)?;
            }
            "backgroundApp" => {
                let entry = app(&snap, package)?;
                let key = WindowKey {
                    instance_id: id.into(),
                    package: Some(entry.package.to_string()),
                };
                if self.store.is_none() {
                    self.window_event(WindowEvent::Closed { key });
                } else {
                    self.window(WindowCommand::Background { key })?;
                }
            }
            "quitApp" | "forceStop" => {
                let entry = app(&snap, package)?;
                let key = WindowKey {
                    instance_id: id.into(),
                    package: Some(entry.package.to_string()),
                };
                // 原生退出请求可能在实例队列中等过一次关闭/重启，不能借用新运行的任务。
                if let Some(observed) = data["windowSeq"].as_u64() {
                    let s = self.state.lock().unwrap();
                    if snap.seq != observed
                        || s.closing.contains_key(id)
                        || !s.open.contains(&(id.into(), key.package.clone()))
                    {
                        drop(s);
                        self.release_quit(data);
                        return Ok(self.revision());
                    }
                }
                // 完全退出是已存在的任务/窗口操作，卡片菜单的强停只向适用的应用开放。
                let allowed = if kind == "quitApp" {
                    let state = self.state.lock().unwrap();
                    state.open.contains(&(id.into(), Some(package.into())))
                        || state.tasks.get(id).is_some_and(|t| t.contains(package))
                } else {
                    action_allowed(entry, &snap, Kind::ForceStop)
                };
                let result = if allowed {
                    self.device(
                        &instance,
                        Command::ForceStop {
                            package: entry.package.clone(),
                        },
                    )
                } else {
                    Err(error("此应用当前不能结束"))
                };
                result?;
                if self.store.is_none() {
                    self.window_event(WindowEvent::Closed { key });
                } else {
                    self.window(WindowCommand::Dismiss { key })?;
                }
                if let Some(tasks) = self.state.lock().unwrap().tasks.get_mut(id) {
                    tasks.remove(package);
                }
                self.note(Some(id), false, "应用已退出");
            }
            "addApk" => {
                if !caps.install {
                    return Err(Rejection::NotReady { state: snap.state });
                }
                self.window(WindowCommand::PickApk {
                    instance_id: id.into(),
                })?;
            }
            "install" | "start" | "stop" | "refreshApps" | "setDefaultHome" | "clearData" => {
                let _admission = (kind == "start").then(|| self.starting.lock().unwrap());
                if kind == "start" && caps.start {
                    let active = self
                        .state
                        .lock()
                        .unwrap()
                        .instances
                        .values()
                        .filter(|i| i.handle.snapshot().facts.process_alive)
                        .count();
                    if active >= 3 {
                        return Err(error("已有 3 台实例运行，请先停止一台"));
                    }
                    if let Some(r) = &instance.record {
                        self.check_budget(&r.spec.template)?;
                    }
                    self.require_ready_runtime()?;
                }
                if kind == "setDefaultHome" && !snap.desktop.is_candidate(package) {
                    return Err(error("应用不在 Android 桌面候选中"));
                }
                if kind == "clearData"
                    && !action_allowed(app(&snap, package)?, &snap, Kind::ClearData)
                {
                    return Err(error("此应用不提供清除数据操作"));
                }
                let command = crate::device_command(data)?;
                // 文件面板回调没有等待它的界面请求，安装阶段也写入发起实例的快照。
                if kind == "install" {
                    self.note(Some(id), false, "正在安装 APK…");
                }
                let seq = self.device(&instance, command)?;
                match kind {
                    "start" => {
                        let mut s = self.state.lock().unwrap();
                        s.frozen.remove(id);
                        s.operations.remove(id);
                    }
                    "install" => self.note(Some(id), false, "APK 已安装并核实版本"),
                    "setDefaultHome" => self.note(Some(id), false, "默认桌面已更新"),
                    "stop" => {
                        self.clear_stopped_instance(id);
                        self.state.lock().unwrap().frozen.remove(id);
                        let message = if instance.handle.snapshot().state == State::Stopped {
                            "实例已停止"
                        } else {
                            "正在停止实例"
                        };
                        self.note(Some(id), false, message);
                    }
                    _ => {}
                }
                return Ok(seq);
            }
            // 单 actor 测试可从 Absent 开始演练设备命令。
            "create" | "delete" if self.fixture => {
                return self.device(&instance, crate::device_command(data)?);
            }
            _ => return Err(error(format!("无法识别的操作：{kind}"))),
        }
        Ok(self.revision())
    }

    fn create_instance(self: &Arc<Self>, data: &JsonValue) -> Reply {
        self.require_ready_runtime()?;
        let name = data["name"].as_str().unwrap_or_default().trim();
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err(error("实例名称需要 1 至 64 个字符"));
        }
        let template = data["template"].as_str().unwrap_or("phone-1080");
        if !matches!(
            template,
            "phone-1080" | "tablet-1200" | "tv-1080" | "tv-4k" | "compact-720"
        ) {
            return Err(error("请选择列表中的设备模板"));
        }
        let spec = contract::Spec {
            template: template.into(),
            image: contract::Image {
                api: 34,
                tag: "google_apis_playstore".into(),
                abi: "arm64-v8a".into(),
            },
            disk_bytes: 8 << 30,
        };
        let id = if let Some(store) = &self.store {
            let _admission = self.starting.lock().unwrap();
            self.check_budget(template)?;
            let record = store.create(name, spec).map_err(error)?;
            let id = record.id.to_string();
            self.add_record(record);
            id
        } else {
            if self
                .state
                .lock()
                .unwrap()
                .instances
                .values()
                .any(|i| i.name == name)
            {
                return Err(error("已存在同名实例"));
            }
            let id = format!("demo-{}", self.revision());
            let (h, ev) = device::spawn(
                InstanceId::new(&id),
                id.clone(),
                device::FakeBackend::new(),
                device::Timeouts::default(),
            );
            if let Ack::Rejected { reason } = h.send(Command::Create { spec }) {
                return Err(reason);
            }
            self.add(name.into(), h, ev, None);
            id
        };
        self.select(&id);
        self.note(Some(&id), false, "实例已创建");
        Ok(self.revision())
    }

    fn select(&self, id: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(previous) = state.selected.replace(id.into()) {
            state.last_interaction.insert(previous, Instant::now());
        }
    }

    fn remove(&self, id: &str) -> Result<(), Rejection> {
        // 设备域已完成持久化删除；从宿主注销句柄，再切换其余实例。
        let selected = {
            let mut s = self.state.lock().unwrap();
            s.instances.remove(id);
            s.operations.remove(id);
            s.tasks.remove(id);
            s.task_errors.remove(id);
            s.open.retain(|k| k.0 != id);
            s.opening.retain(|k| k.0 != id);
            s.streaming.retain(|k| k.0 != id);
            s.closing.remove(id);
            s.frozen.remove(id);
            s.last_interaction.remove(id);
            if s.selected.as_deref() == Some(id) {
                s.selected = s.instances.keys().next().cloned();
            }
            s.selected.clone()
        };
        if let (Some(store), Some(id)) = (&self.store, selected) {
            store.select(&InstanceId::new(id)).map_err(error)?;
        }
        Ok(())
    }

    pub(super) fn freeze_idle(
        &self,
        instance: &Instance,
        receipt: impl FnOnce() -> std::io::Result<bool>,
    ) -> Reply {
        let id = instance.handle.id().as_str();
        if instance.handle.snapshot().state != State::Ready || !self.idle(id) {
            return Ok(self.revision());
        }
        let seq = self.device(instance, Command::Stop)?;
        self.clear_stopped_instance(id);
        let stopped = instance.handle.snapshot();
        if stopped.state != State::Stopped {
            self.note(Some(id), true, "正在停止实例，尚未确认快照");
            return Ok(seq);
        }
        // 读取运行时对这次退出签发的凭证。磁盘 I/O 和 Stop 都在宿主状态锁之外。
        let saved = receipt();
        let mut state = self.state.lock().unwrap();
        if !state.instances.contains_key(id) || instance.handle.snapshot().seq != stopped.seq {
            return Ok(seq);
        }
        let (failed, message) = match saved {
            Ok(true) => {
                state.frozen.insert(id.into());
                (false, "实例已冻结，可从本次快照恢复".into())
            }
            unverified => {
                state.frozen.remove(id);
                let reason = match unverified {
                    Err(e) => format!("无法核实本次快照（{e}）"),
                    _ => "本次未保存可恢复的快照".into(),
                };
                (true, format!("实例已停止，{reason}"))
            }
        };
        state.operations.insert(
            id.into(),
            json!({"instanceId":id,"message":message,"failed":failed}),
        );
        drop(state);
        self.notify();
        Ok(seq)
    }

    pub(super) fn idle(&self, id: &str) -> bool {
        let state = self.state.lock().unwrap();
        state.selected.as_deref() != Some(id)
            && !state.open.iter().chain(&state.opening).any(|k| k.0 == id)
            && state
                .last_interaction
                .get(id)
                .is_some_and(|last| last.elapsed() >= Duration::from_secs(600))
    }

    fn check_budget(&self, template: &str) -> Result<(), Rejection> {
        let physical = runtime::instances::physical_memory_mib().map_err(error)?;
        let state = self.state.lock().unwrap();
        let used: u64 = state
            .instances
            .values()
            .filter(|i| i.handle.snapshot().facts.process_alive)
            .filter_map(|i| i.record.as_ref())
            .map(|r| runtime::instances::ram_mib(&r.spec.template))
            .sum();
        let requested = runtime::instances::ram_mib(template);
        if used + requested > physical / 2 {
            return Err(error(format!(
                "实例需要 {requested} MiB，当前预算可用 {} MiB，请停止其他实例",
                (physical / 2).saturating_sub(used)
            )));
        }
        Ok(())
    }

    fn require_ready_runtime(&self) -> Result<(), Rejection> {
        if self
            .runtime
            .as_ref()
            .is_some_and(|p| p.snapshot().phase != "ready")
        {
            Err(error("请先准备运行环境"))
        } else {
            Ok(())
        }
    }

    fn device(&self, i: &Instance, cmd: Command) -> Reply {
        match i.handle.send(cmd) {
            Ack::Accepted { seq } => Ok(seq),
            Ack::Rejected { reason } => Err(reason),
        }
    }
    fn window(&self, cmd: WindowCommand) -> Result<(), Rejection> {
        self.windows
            .send(cmd)
            .map_err(|_| error("窗口管理器已关闭"))
    }

    fn open(&self, i: &Instance, snap: &Snapshot, app: Option<&AppEntry>) -> Result<(), Rejection> {
        if let Some(record) = &i.record {
            record.verify_device().map_err(error)?;
        }
        let id = snap.id.to_string();
        let package = app.map(|a| a.package.to_string());
        let key = WindowKey {
            instance_id: id.clone(),
            package: package.clone(),
        };
        let index = (id.clone(), package.clone());
        let native_open = self.store.as_ref().map(|_| {
            let paths = RuntimePaths::discover(&self.layout.root);
            let title = app
                .map(|a| a.label.as_deref().unwrap_or(a.package.as_str()))
                .unwrap_or("设备视图");
            WindowCommand::Open {
                key: key.clone(),
                title: format!("{title} · {}", i.name),
                serial: snap.serial.clone(),
                adb: paths.adb,
                jar: paths.server_jar,
            }
        });
        let mut s = self.state.lock().unwrap();
        let state = i.handle.snapshot().state;
        if !state.accepts_stream() {
            return Err(Rejection::NotReady { state });
        }
        if s.opening.contains(&index) {
            return Ok(());
        }
        if !s.open.contains(&index)
            && package.is_some()
            && s.open.union(&s.opening).filter(|k| k.1.is_some()).count() >= 8
        {
            return Err(error("已打开 8 个应用窗口，请先关闭一个"));
        }
        // 聚焦也有 Opened 回执。若原生旧窗口已关，新请求可以认领随后创建的窗口。
        s.opening.insert(index.clone());
        if let Some(command) = native_open {
            // 与停止清理共用锁，保证原生命令按宿主预留、释放的顺序进队。
            if let Err(reason) = self.window(command) {
                s.opening.remove(&index);
                return Err(reason);
            }
            return Ok(());
        }
        drop(s);
        if let Some(a) = app {
            if let Err(reason) = self.device(
                i,
                Command::Launch {
                    package: a.package.clone(),
                },
            ) {
                self.state.lock().unwrap().opening.remove(&index);
                return Err(reason);
            }
            let mut s = self.state.lock().unwrap();
            if (s.opening.contains(&index) || s.open.contains(&index))
                && i.handle.snapshot().state.accepts_stream()
            {
                s.tasks.entry(id).or_default().insert(a.package.to_string());
            }
        }
        self.window_event(WindowEvent::Opened { key: key.clone() });
        self.window_event(WindowEvent::FirstFrame { key });
        Ok(())
    }
}

fn app<'a>(snap: &'a Snapshot, package: &str) -> Result<&'a AppEntry, Rejection> {
    snap.apps
        .iter()
        .find(|a| a.launcher && a.package.as_str() == package)
        .ok_or_else(|| error("应用已不在此实例的可启动清单中"))
}
fn action_allowed(app: &AppEntry, snap: &Snapshot, kind: Kind) -> bool {
    actions::for_app(
        app,
        &snap.desktop,
        contract::Capabilities::of(snap.state).launch_app,
    )
    .iter()
    .any(|a| a.kind == kind && a.enabled)
}
