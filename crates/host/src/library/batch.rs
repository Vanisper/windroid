//! 确认页在宿主保留目标和检查结果。执行按实例命令队列投递，以设备终态结算。
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum Action {
    Start,
    Stop,
}

impl Action {
    fn command(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }

    fn allows(self, state: State) -> bool {
        let caps = contract::Capabilities::of(state);
        match self {
            Self::Start => caps.start,
            Self::Stop => caps.stop,
        }
    }

    fn skip_reason(self, snap: &Snapshot) -> String {
        match (self, snap.state) {
            (Self::Start, State::Ready | State::Streaming) => "已在运行".into(),
            (Self::Stop, State::Failed) if snap.facts.aborted => {
                "启动超时，模拟器进程已退出".into()
            }
            (Self::Stop, State::Failed) => "模拟器进程已退出".into(),
            _ => crate::describe(snap.state, snap.phase, &snap.facts),
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Scope {
    All,
    Selected,
    Retry,
}

#[derive(Clone, Serialize)]
struct Target {
    id: String,
    name: String,
}

#[derive(Serialize)]
struct PreviewItem {
    #[serde(flatten)]
    target: Target,
    eligible: bool,
    reason: String,
    windows: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Preview {
    preview_id: u64,
    action: Action,
    scope: Scope,
    items: Vec<PreviewItem>,
}

#[derive(Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum Status {
    Pending,
    Running,
    Succeeded,
    Skipped,
    Failed,
}

#[derive(Serialize)]
pub(super) struct Outcome {
    status: Status,
    message: String,
}

impl Outcome {
    fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

#[derive(Serialize)]
struct ResultItem {
    #[serde(flatten)]
    target: Target,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Job {
    job_id: u64,
    action: Action,
    scope: Scope,
    active: bool,
    items: Vec<ResultItem>,
}

// 初次确认、重新检查和失败重试都有明确的身份来源，后两者不接收前端拼装的目标集合。
#[derive(Deserialize)]
#[serde(untagged, rename_all_fields = "camelCase", deny_unknown_fields)]
enum PreviewRequest {
    New {
        action: Action,
        scope: Scope,
        ids: BTreeSet<String>,
    },
    Recheck {
        preview_id: u64,
    },
    Retry {
        retry_of: u64,
    },
}

pub(super) enum Update {
    Device(contract::Event),
    Replacing {
        before: u64,
        accepted: mpsc::Receiver<()>,
    },
}

impl Library {
    /// 重启可从 Stalled 直接跳到下一轮 Starting。先为其回执在事件流中占位，
    /// 才能在原会话被替换时截断等待；若预检拒绝，仍按原会话的后续事件结算。
    pub(super) fn observe_batch_replacement(
        &self,
        instance: &Instance,
    ) -> Option<mpsc::Sender<()>> {
        let state = self.state.lock().unwrap();
        let (id, observer) = state.batch_observer.as_ref()?;
        if id != instance.handle.id().as_str() {
            return None;
        }
        let snap = instance.handle.snapshot();
        if !matches!(snap.state, State::Starting | State::Stalled) {
            return None;
        }
        let (ack, accepted) = mpsc::channel();
        observer
            .send(Update::Replacing {
                before: snap.seq,
                accepted,
            })
            .ok()?;
        Some(ack)
    }
    pub(super) fn connect_batch(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.channel.handle("windroid.previewBatch", move |call| {
            let Some(lib) = weak.upgrade() else {
                return call.reject("应用库已关闭");
            };
            thread::spawn(move || {
                let result = lib.preview_batch(call.data());
                call.reply(result);
            });
        });
    }

    pub(super) fn preview_batch(&self, data: &JsonValue) -> Result<JsonValue, String> {
        let request: PreviewRequest =
            serde_json::from_value(data.clone()).map_err(|_| "请选择批量操作及实例范围")?;
        let mut pending = self.batch_preview.lock().unwrap();
        let (action, scope, targets) = {
            let state = self.state.lock().unwrap();
            if state.batch.as_ref().is_some_and(|job| job.active) {
                return Err("正在执行批量操作，请等待本批结束".into());
            }
            match request {
                PreviewRequest::Recheck { preview_id } => {
                    let plan = pending
                        .as_ref()
                        .filter(|p| p.preview_id == preview_id)
                        .ok_or("确认页已过期，请重新选择批量操作")?;
                    (
                        plan.action,
                        plan.scope,
                        plan.items.iter().map(|i| i.target.clone()).collect(),
                    )
                }
                PreviewRequest::Retry { retry_of } => {
                    let job = state
                        .batch
                        .as_ref()
                        .filter(|j| j.job_id == retry_of)
                        .ok_or("此次执行结果已过期")?;
                    let targets = job
                        .items
                        .iter()
                        .filter(|i| i.outcome.status == Status::Failed)
                        .map(|i| i.target.clone())
                        .collect();
                    (job.action, Scope::Retry, targets)
                }
                PreviewRequest::New { action, scope, ids } => {
                    if matches!(scope, Scope::Retry) {
                        return Err("重试需要指定原执行结果".into());
                    }
                    // 快照按 BTreeMap 的实例 ID 顺序展示；点击顺序不会改变预算分配顺序。
                    let targets = ids
                        .into_iter()
                        .map(|id| Target {
                            name: state
                                .instances
                                .get(&id)
                                .map(|i| i.name.clone())
                                .unwrap_or_else(|| id.clone()),
                            id,
                        })
                        .collect::<Vec<_>>();
                    (action, scope, targets)
                }
            }
        };
        if targets.is_empty() {
            return Err("没有需要处理的实例".into());
        }
        let items = self.check_batch(action, targets);
        let plan = Preview {
            preview_id: self.revision(),
            action,
            scope,
            items,
        };
        let view = json!(plan);
        *pending = Some(plan);
        Ok(view)
    }

    fn check_batch(&self, action: Action, targets: Vec<Target>) -> Vec<PreviewItem> {
        // 精确版本、镜像和存储位置由 runtime 只读核验。普通启动的资源重扫会更新完整性缓存。
        let resource_error = self
            .runtime
            .as_ref()
            .and_then(|r| r.lock().unwrap().error.clone());
        let mut items = Vec::with_capacity(targets.len());
        let mut candidates = Vec::new();
        let mut active = None;
        let state = self.state.lock().unwrap();
        for target in targets {
            let mut row = PreviewItem {
                target,
                eligible: false,
                reason: "实例已移除".into(),
                windows: Vec::new(),
            };
            if let Some(instance) = state.instances.get(&row.target.id) {
                let snap = instance.handle.snapshot();
                if !action.allows(snap.state) {
                    row.reason = action.skip_reason(&snap);
                } else if action == Action::Start
                    && let Some(message) = &resource_error
                {
                    row.reason.clone_from(message);
                } else {
                    row.eligible = true;
                    row.reason = match action {
                        Action::Start => {
                            candidates.push((items.len(), instance.handle.id().clone()));
                            active.get_or_insert_with(|| instance.active.clone());
                            "将启动"
                        }
                        Action::Stop => {
                            row.windows = affected_windows(&state, &snap);
                            if matches!(snap.state, State::Starting | State::Stalled) {
                                "将中止启动"
                            } else {
                                "将停止"
                            }
                        }
                    }
                    .into();
                }
            }
            items.push(row);
        }
        drop(state);
        if let (Some(store), Some(active)) = (&self.store, active) {
            let (positions, ids): (Vec<_>, Vec<_>) = candidates.into_iter().unzip();
            for (pos, result) in positions
                .into_iter()
                .zip(store.preview_starts(&ids, &active))
            {
                if let Err(e) = result {
                    items[pos].eligible = false;
                    items[pos].reason = e.to_string();
                }
            }
        }
        items
    }

    pub(super) fn execute_batch(self: &Arc<Self>, data: &JsonValue) -> Reply {
        let mut pending = self.batch_preview.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        if state.batch.as_ref().is_some_and(|j| j.active) {
            return Err(error("正在执行批量操作，请勿重复提交"));
        }
        let plan = pending
            .as_ref()
            .filter(|p| Some(p.preview_id) == data["previewId"].as_u64())
            .ok_or_else(|| error("确认已过期或已提交，请重新检查"))?;
        if !plan.items.iter().any(|row| row.eligible) {
            return Err(error("当前没有可执行的实例，请重新检查"));
        }
        let plan = pending.take().unwrap();
        let action = plan.action;
        let job_id = self.revision();
        let mut work = Vec::new();
        let items = plan
            .items
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let outcome = if row.eligible {
                    work.push((index, row.target.id.clone()));
                    Outcome::new(Status::Pending, "等待处理")
                } else {
                    Outcome::new(Status::Skipped, row.reason)
                };
                ResultItem {
                    target: row.target,
                    outcome,
                }
            })
            .collect();
        state.batch = Some(Job {
            job_id,
            action,
            scope: plan.scope,
            active: true,
            items,
        });
        drop(state);
        drop(pending);
        self.notify();
        let lib = Arc::clone(self);
        thread::spawn(move || {
            for (index, id) in work {
                let verb = if action == Action::Start {
                    "正在启动"
                } else {
                    "正在停止"
                };
                lib.update_batch_item(index, Outcome::new(Status::Running, verb));
                let outcome = lib.run_batch_item(&id, action);
                lib.update_batch_item(index, outcome);
            }
            lib.state.lock().unwrap().batch.as_mut().unwrap().active = false;
            lib.notify();
        });
        Ok(job_id)
    }

    fn update_batch_item(&self, index: usize, outcome: Outcome) {
        self.state.lock().unwrap().batch.as_mut().unwrap().items[index].outcome = outcome;
        self.notify();
    }

    fn run_batch_item(&self, id: &str, action: Action) -> Outcome {
        let (observe, events) = mpsc::channel();
        let lane = {
            let mut state = self.state.lock().unwrap();
            let Some(lane) = state.instances.get(id).map(|i| i.lane.clone()) else {
                return Outcome::new(Status::Skipped, "实例已移除");
            };
            state.batch_observer = Some((id.to_owned(), observe));
            lane
        };
        // 先订阅再入队，避免极快的启动/停止在命令回执之前已经结束。等待设备不占用实例队列。
        let (reply, ack) = mpsc::channel();
        let request = Request::Batch {
            id: id.into(),
            action,
            reply,
        };
        let result = if let Err(error) = lane.try_send(request) {
            self.reject_unqueued(error);
            Outcome::new(Status::Failed, "实例操作队列繁忙或已关闭")
        } else {
            match ack.recv() {
                Ok(Ok((seq, snap))) => transition_outcome(action, snap.state, &snap.facts)
                    .unwrap_or_else(|| wait_for_outcome(action, seq, events)),
                Ok(Err(outcome)) => outcome,
                Err(_) => Outcome::new(Status::Failed, "实例操作队列已关闭"),
            }
        };
        self.state.lock().unwrap().batch_observer = None;
        result
    }

    pub(super) fn perform_batch_item(
        self: &Arc<Self>,
        id: &str,
        action: Action,
    ) -> Result<(u64, Snapshot), Outcome> {
        let instance = self
            .state
            .lock()
            .unwrap()
            .instances
            .get(id)
            .cloned()
            .ok_or_else(|| Outcome::new(Status::Skipped, "实例已移除"))?;
        let snap = instance.handle.snapshot();
        if !action.allows(snap.state) {
            return Err(Outcome::new(Status::Skipped, action.skip_reason(&snap)));
        }
        self.perform_logged(
            &json!({"kind": action.command(), "instanceId": id}),
            "批量调度",
        )
        .map(|seq| (seq, instance.handle.snapshot()))
        .map_err(|r| {
            if matches!(r, Rejection::WrongState { .. } | Rejection::NotReady { .. }) {
                Outcome::new(
                    Status::Skipped,
                    action.skip_reason(&instance.handle.snapshot()),
                )
            } else {
                Outcome::new(Status::Failed, rejection_message(&r))
            }
        })
    }
}

fn affected_windows(state: &ViewState, snap: &Snapshot) -> Vec<String> {
    state
        .open
        .union(&state.opening)
        .filter(|(id, _)| id == snap.id.as_str())
        .map(|(_, package)| match package {
            None => "设备视图".to_owned(),
            Some(package) => snap
                .apps
                .iter()
                .find(|a| a.package.as_str() == package)
                .and_then(|a| a.label.as_ref())
                .unwrap_or(package)
                .clone(),
        })
        .collect()
}

fn wait_for_outcome(action: Action, seq: u64, events: mpsc::Receiver<Update>) -> Outcome {
    // 设备域负责启动硬上限和回收。再留两轮观测接收最终事件，丢失回执也有明确终态。
    let timeout = device::Timeouts::default();
    let deadline = Instant::now() + timeout.hard + 2 * timeout.poll;
    while let Ok(event) = events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        let (changed, to, facts) = match event {
            Update::Device(contract::Event::StateChanged { seq, to, facts, .. }) => {
                (seq, to, facts)
            }
            Update::Replacing { before, accepted } if before >= seq => {
                if accepted
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .is_ok()
                {
                    return Outcome::new(Status::Failed, "本次启动已被重新启动替换");
                }
                continue;
            }
            _ => continue,
        };
        if changed < seq {
            continue;
        }
        if let Some(result) = transition_outcome(action, to, &facts) {
            return result;
        }
    }
    Outcome::new(Status::Failed, "未能在时限内确认实例的最终状态")
}

fn transition_outcome(action: Action, to: State, facts: &Facts) -> Option<Outcome> {
    let (status, message) = match (action, to) {
        (Action::Start, State::Ready | State::Streaming) => (Status::Succeeded, "已启动"),
        (Action::Start, State::Starting | State::Stalled) if !facts.aborted => return None,
        (Action::Start, _) if facts.aborted => (Status::Failed, "启动超过时限，系统服务未就绪"),
        (Action::Start, State::Failed) => (Status::Failed, "模拟器在就绪前退出"),
        (Action::Start, _) => (Status::Failed, "启动期间实例已被停止或移除"),
        (Action::Stop, State::Stopped) => (Status::Succeeded, "已停止"),
        (Action::Stop, State::Stopping) => return None,
        (Action::Stop, _) => (Status::Failed, "停止期间实例状态发生变化"),
    };
    Some(Outcome::new(status, message))
}
