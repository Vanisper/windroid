//! AppKit 独占窗口注册表。工作线程上报的每条消息携带打开代次，旧流的迟到
//! 回调不能影响同键的新窗口。后台恢复在旧显示完成迁移和回收后才新建显示。

mod registry;
mod state;

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use host::surface::{
    self, CloseChoice, CloseIntent, VideoWindow, WindowCommand, WindowEvent, WindowKey,
};
use nsh_engine::HostChannel;
use nsh_platform_macos::objc2::MainThreadMarker;

use crate::streaming::{Notice, Source, Worker};
use registry::{MAX_APP_WINDOWS, Registry, Route};
use state::{ActivityWatch, Preferences, QuitState};

const FIRST_FRAME_DEADLINE: Duration = Duration::from_secs(45);

#[derive(Clone, Debug, Eq, PartialEq)]
struct Ticket {
    key: WindowKey,
    generation: u64,
}
struct Request {
    key: WindowKey,
    title: String,
    source: Source,
}

struct Entry {
    generation: u64,
    title: String,
    window: VideoWindow,
    worker: Worker,
    activity: ActivityWatch,
    asking: bool,
    quit: QuitState,
    worker_ended: bool,
}

struct Manager {
    windows: Registry<Entry, Worker, Request>,
    next_generation: u64,
    preferences: Preferences,
    channel: HostChannel,
    events: Sender<WindowEvent>,
    picking: bool,
    _termination: Option<nsh_platform_macos::objc2::rc::Retained<surface::TerminationDelegate>>,
}

thread_local! { static MANAGER: RefCell<Option<Manager>> = const { RefCell::new(None) }; }

// 委托、sheet 应答和引擎都可重入。异步投递，AppKit 任一回调都可先完整返回。
fn post(action: impl FnOnce(&mut Manager, MainThreadMarker) + Send + 'static) {
    surface::on_main(move |mtm| {
        MANAGER.with_borrow_mut(|slot| {
            if let Some(manager) = slot {
                action(manager, mtm);
            }
        })
    });
}

pub fn install(
    mtm: MainThreadMarker,
    channel: HostChannel,
    home: PathBuf,
    rx: Receiver<WindowCommand>,
    events: Sender<WindowEvent>,
) {
    // 设置页与关窗 sheet 都在 AppKit 队列中读写同一个偏好，顺序与用户操作一致。
    channel.handle("windroid.windowSettings", |call| {
        post(move |manager, _| call.resolve(manager.preferences.snapshot()));
    });
    channel.handle("windroid.setWindowSettings", |call| {
        let choice = match call.data()["closeAction"].as_str() {
            Some("ask") => None,
            Some("background") => Some(CloseChoice::Background),
            Some("quit") => Some(CloseChoice::Quit),
            _ => return call.reject("无法识别应用窗口关闭方式"),
        };
        post(move |manager, _| match manager.save_preference(choice) {
            Ok(()) => call.resolve(manager.preferences.snapshot()),
            Err(error) => call.reject(format!("无法保存关窗偏好：{error}")),
        });
    });
    MANAGER.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "重复安装窗口管理器");
        *slot = Some(Manager {
            windows: Registry::new(),
            next_generation: 1,
            preferences: Preferences::load(&home),
            channel,
            events,
            picking: false,
            _termination: surface::TerminationDelegate::install(mtm),
        });
    });
    std::thread::Builder::new()
        .name("windroid-windows".into())
        .spawn(move || {
            for command in rx {
                post(move |manager, mtm| manager.command(mtm, command));
            }
        })
        .expect("启动窗口命令分发线程");
}

/// 退出事件循环后调用。让每条读写链先并行断开，再等所有归属子进程回收。
pub fn shutdown(_mtm: MainThreadMarker) {
    let Some(manager) = MANAGER.with_borrow_mut(Option::take) else {
        return;
    };
    let mut workers = Vec::new();
    for (_, entry) in manager.windows.live {
        entry.worker.cancel();
        workers.push(entry.worker);
    }
    for (_, entry) in manager.windows.retiring {
        entry.worker.cancel();
        workers.push(entry.worker);
    }
    for worker in workers {
        worker.join();
    }
}

impl Manager {
    fn command(&mut self, mtm: MainThreadMarker, cmd: WindowCommand) {
        match cmd {
            WindowCommand::Open {
                key,
                title,
                serial,
                adb,
                jar,
            } => {
                let source = Source {
                    adb,
                    serial,
                    jar,
                    package: key.package.clone(),
                };
                self.open(mtm, Request { key, title, source });
            }
            WindowCommand::Background { key } => {
                if !self
                    .windows
                    .live
                    .get(&key)
                    .is_some_and(|entry| entry.quit != QuitState::Idle)
                {
                    self.close(&key);
                }
            }
            WindowCommand::Dismiss { key } => self.close(&key),
            WindowCommand::QuitFailed { key } => {
                if let Some(entry) = self.windows.live.get_mut(&key) {
                    entry.quit = QuitState::Idle;
                    if entry.worker_ended {
                        entry
                            .window
                            .set_title(&format!("{} · 退出失败，可再次关闭重试", entry.title));
                    } else {
                        entry.window.set_title(&entry.title);
                    }
                }
            }
            WindowCommand::CloseInstance { instance_id } => {
                let keys: Vec<_> = self
                    .windows
                    .live
                    .keys()
                    .chain(self.windows.retiring.keys())
                    .filter(|key| key.instance_id == instance_id)
                    .cloned()
                    .collect();
                for key in keys {
                    self.close(&key);
                }
                self.send(WindowEvent::InstanceClosed { instance_id });
            }
            WindowCommand::Reconcile { instance_id, tasks } => {
                let now = Instant::now();
                let closed: Vec<_> = self
                    .windows
                    .live
                    .iter_mut()
                    .filter_map(|(key, entry)| {
                        if key.instance_id != instance_id || entry.quit != QuitState::Idle {
                            return None;
                        }
                        entry
                            .activity
                            .on_tasks(key.package.as_deref()?, &tasks, now)
                            .then(|| key.clone())
                    })
                    .collect();
                for key in closed {
                    self.close(&key);
                }
            }
            WindowCommand::PickApk { instance_id } => self.pick(mtm, instance_id),
        }
    }

    fn send(&self, event: WindowEvent) {
        let _ = self.events.send(event);
    }
    fn error(&self, id: &str, message: String) {
        self.send(WindowEvent::Error {
            instance_id: id.to_owned(),
            message,
        });
    }
    fn refused(&self, key: WindowKey, message: String) {
        self.error(&key.instance_id, message);
        self.send(WindowEvent::Closed { key });
    }

    fn open(&mut self, mtm: MainThreadMarker, request: Request) {
        match self.windows.route(&request.key) {
            Route::Focus => {
                self.windows.live[&request.key].window.show(mtm);
                self.send(WindowEvent::Opened { key: request.key });
                return;
            }
            Route::AfterCleanup => {
                let old = self.windows.retiring.get_mut(&request.key).unwrap();
                old.reopen = Some(request);
                return;
            }
            Route::AtCapacity => {
                self.refused(
                    request.key,
                    format!("最多可打开 {MAX_APP_WINDOWS} 个应用窗口，请先关闭一个窗口"),
                );
                return;
            }
            Route::Create => {}
        }
        let ticket = Ticket {
            key: request.key.clone(),
            generation: self.next_generation,
        };
        self.next_generation += 1;
        let report_id = ticket.clone();
        let worker = match Worker::spawn(request.source, move |notice| {
            let ticket = report_id.clone();
            post(move |manager, mtm| manager.notice(mtm, ticket, notice));
        }) {
            Ok(worker) => worker,
            Err(err) => {
                self.refused(request.key, format!("无法准备视频窗口：{err}"));
                return;
            }
        };
        let close_id = ticket.clone();
        let window = VideoWindow::new(
            mtm,
            &format!("{} · 正在连接…", request.title),
            (360.0, 640.0),
            Box::new(move || {
                let ticket = close_id.clone();
                post(move |manager, mtm| manager.close_requested(mtm, ticket));
                CloseIntent::Keep
            }),
            worker.input_handler(),
            request
                .key
                .package
                .as_ref()
                .map(|_| worker.resize_handler()),
        );
        window.show(mtm);
        self.windows.live.insert(
            request.key.clone(),
            Entry {
                generation: ticket.generation,
                title: request.title,
                window,
                worker,
                activity: ActivityWatch::default(),
                asking: false,
                quit: QuitState::Idle,
                worker_ended: false,
            },
        );
        self.send(WindowEvent::Opened { key: request.key });
        surface::on_main_after(FIRST_FRAME_DEADLINE, move |_mtm| {
            MANAGER.with_borrow_mut(|slot| {
                let Some(manager) = slot else {
                    return;
                };
                if manager.entry_mut(&ticket).is_some_and(|e| {
                    e.activity.painted.is_none() && e.quit == QuitState::Idle && !e.worker_ended
                }) {
                    manager.error(
                        &ticket.key.instance_id,
                        "视频首帧超时，可从应用库重新打开".into(),
                    );
                    manager.close(&ticket.key);
                }
            });
        });
    }

    fn entry_mut(&mut self, ticket: &Ticket) -> Option<&mut Entry> {
        self.windows
            .live
            .get_mut(&ticket.key)
            .filter(|entry| entry.generation == ticket.generation)
    }

    fn close_requested(&mut self, mtm: MainThreadMarker, ticket: Ticket) {
        let preference = self.preferences.choice;
        let Some(entry) = self.entry_mut(&ticket) else {
            return;
        };
        if entry.asking || entry.quit != QuitState::Idle {
            return;
        }
        // 设备视图直接关闭。应用窗口即使还在连接，也沿用相同的关闭选择。
        if ticket.key.package.is_none() {
            self.close(&ticket.key);
            return;
        }
        if let Some(choice) = preference {
            self.apply_choice(ticket, choice, false);
            return;
        }
        entry.asking = true;
        entry
            .window
            .ask_close(mtm, &entry.title, move |choice, remember| {
                let ticket = ticket.clone();
                post(move |manager, _mtm| {
                    let Some(entry) = manager.entry_mut(&ticket) else {
                        return;
                    };
                    entry.asking = false;
                    if let Some(choice) = choice {
                        manager.apply_choice(ticket, choice, remember);
                    }
                });
            });
    }

    fn apply_choice(&mut self, ticket: Ticket, choice: CloseChoice, remember: bool) {
        if remember && let Err(e) = self.save_preference(Some(choice)) {
            self.error(&ticket.key.instance_id, format!("无法保存关窗偏好：{e}"));
        }
        match choice {
            CloseChoice::Background => self.close(&ticket.key),
            CloseChoice::Quit => {
                if let Some(entry) = self.entry_mut(&ticket) {
                    // 握手完成时 START_APP 只写入了套接字，任务快照才确认设备已执行。
                    let dispatch = entry
                        .quit
                        .begin(entry.activity.saw_task || entry.worker_ended);
                    entry
                        .window
                        .set_title(&format!("{} · 正在退出…", entry.title));
                    if dispatch {
                        self.send(WindowEvent::QuitRequested { key: ticket.key });
                    } else {
                        entry.worker.cancel();
                    }
                }
            }
        }
    }

    fn save_preference(&mut self, choice: Option<CloseChoice>) -> std::io::Result<()> {
        self.preferences.set(choice)?;
        let _ = self.channel.send(
            "windroid.windowSettingsChanged",
            self.preferences.snapshot(),
        );
        Ok(())
    }

    fn close(&mut self, key: &WindowKey) {
        if let Some(old) = self.windows.retiring.get_mut(key) {
            if old.reopen.take().is_some() {
                self.send(WindowEvent::Closed { key: key.clone() });
            }
            return;
        }
        let Some(entry) = self.windows.live.remove(key) else {
            return;
        };
        entry.worker.cancel();
        drop(entry.window);
        if entry.worker_ended {
            entry.worker.join();
        } else {
            self.windows
                .retire(key.clone(), entry.generation, entry.worker);
        }
        self.send(WindowEvent::Closed { key: key.clone() });
    }

    fn notice(&mut self, mtm: MainThreadMarker, ticket: Ticket, notice: Notice) {
        match notice {
            Notice::Connected(display) => {
                if let Some(entry) = self.entry_mut(&ticket) {
                    entry.activity.display = Some(display);
                }
            }
            Notice::FrameReady => {
                let Some(entry) = self.entry_mut(&ticket) else {
                    return;
                };
                let Some(frame) = entry.worker.take_frame() else {
                    return;
                };
                if let Err(error) = entry.window.draw(frame) {
                    self.error(&ticket.key.instance_id, error);
                    self.close(&ticket.key);
                } else if entry.activity.painted.is_none() {
                    entry.activity.painted = Some(Instant::now());
                    if entry.quit == QuitState::Idle {
                        entry.window.set_title(&entry.title);
                    }
                    self.send(WindowEvent::FirstFrame { key: ticket.key });
                }
            }
            Notice::Ended(result) => {
                if let Some(entry) = self.entry_mut(&ticket) {
                    entry.worker_ended = true;
                    if entry.quit != QuitState::Idle {
                        if entry.quit.worker_ended() {
                            self.send(WindowEvent::QuitRequested { key: ticket.key });
                        }
                        return;
                    }
                    let message = result.err().unwrap_or_else(|| "视频会话已结束".into());
                    let message = format!("{}：{message}", entry.title);
                    self.error(&ticket.key.instance_id, message);
                    self.close(&ticket.key);
                }
                if let Some(old) = self.windows.finished(&ticket) {
                    old.worker.join();
                    if let Some(request) = old.reopen {
                        self.open(mtm, request);
                    }
                }
            }
        }
    }

    fn pick(&mut self, mtm: MainThreadMarker, instance_id: String) {
        if self.picking {
            return;
        }
        self.picking = true;
        surface::pick_apk(mtm, move |path| {
            let instance_id = instance_id.clone();
            post(move |manager, _mtm| {
                manager.picking = false;
                if let Some(path) = path {
                    manager.send(WindowEvent::ApkPicked { instance_id, path });
                }
            });
        });
    }
}
