//! 一条窗口对应一条工作线程和一组 socket。关闭信号只触达这条会话。
//! 主线程取最新帧并投递输入。adb、解码、控制写入与子进程回收均在后台。

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use stream::{DisplayTarget, Frame, InputEvent, SessionControl, VideoSession};

pub struct Source {
    pub adb: PathBuf,
    pub serial: String,
    pub jar: PathBuf,
    pub package: Option<String>,
}

pub enum Notice {
    Connected(u32),
    FrameReady,
    Ended(Result<(), String>),
}

#[derive(Default)]
struct Latest(Mutex<Option<Frame>>);

impl Latest {
    /// 槽位从空变为非空时才唤醒主线程。积压的帧会被新帧覆盖。
    fn publish(&self, frame: Frame) -> bool {
        self.0.lock().unwrap().replace(frame).is_none()
    }
    fn take(&self) -> Option<Frame> {
        self.0.lock().unwrap().take()
    }
}

const MAX_INPUT: usize = 128;

#[derive(Default)]
struct Pending {
    events: VecDeque<InputEvent>,
    resize: Option<(u16, u16)>,
    closed: bool,
}

#[derive(Default)]
struct InputQueue {
    pending: Mutex<Pending>,
    ready: Condvar,
}

enum Outgoing {
    Event(InputEvent),
    Resize(u16, u16),
}

fn is_move(event: &InputEvent) -> bool {
    matches!(event, InputEvent::Touch { action: 2, .. })
}

impl InputQueue {
    fn push(&self, event: InputEvent) -> Result<(), ()> {
        let mut q = self.pending.lock().unwrap();
        if q.closed {
            return Ok(());
        }
        // 每条队列只有一只鼠标。相邻移动可合并，按下/释放和键盘事件必须保序。
        if is_move(&event) && q.events.back().is_some_and(is_move) {
            q.events.pop_back();
        }
        if q.events.len() == MAX_INPUT {
            if is_move(&event) {
                return Ok(());
            }
            if let Some(i) = q.events.iter().position(is_move) {
                q.events.remove(i);
            } else {
                return Err(());
            }
        }
        q.events.push_back(event);
        self.ready.notify_one();
        Ok(())
    }
    fn resize(&self, width: u16, height: u16) {
        let mut q = self.pending.lock().unwrap();
        if !q.closed {
            q.resize = Some((width, height));
            self.ready.notify_one();
        }
    }
    fn next(&self) -> Option<Outgoing> {
        let mut q = self
            .ready
            .wait_while(self.pending.lock().unwrap(), |q| {
                q.events.is_empty() && q.resize.is_none() && !q.closed
            })
            .unwrap();
        q.events
            .pop_front()
            .map(Outgoing::Event)
            .or_else(|| q.resize.take().map(|(w, h)| Outgoing::Resize(w, h)))
    }
    fn close(&self) {
        let mut q = self.pending.lock().unwrap();
        q.closed = true;
        q.events.clear();
        q.resize = None;
        self.ready.notify_all();
    }
}

#[derive(Default)]
struct Connection {
    stopped: AtomicBool,
    socket: Mutex<Option<SessionControl>>,
    failure: Mutex<Option<String>>,
}

impl Connection {
    fn attach(&self, control: SessionControl) -> bool {
        let mut socket = self.socket.lock().unwrap();
        if self.stopped.load(Ordering::Acquire) {
            control.shutdown();
            return false;
        }
        *socket = Some(control);
        true
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(control) = self.socket.lock().unwrap().take() {
            control.shutdown();
        }
    }
    fn fail(&self, message: String) {
        self.failure.lock().unwrap().get_or_insert(message);
        self.stop();
    }
}

pub struct Worker {
    connection: Arc<Connection>,
    input: Arc<InputQueue>,
    latest: Arc<Latest>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn spawn(
        source: Source,
        report: impl Fn(Notice) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let connection = Arc::new(Connection::default());
        let input = Arc::new(InputQueue::default());
        let latest = Arc::new(Latest::default());
        let (conn, events, frames) = (connection.clone(), input.clone(), latest.clone());
        let report = Arc::new(report);
        let thread = thread::Builder::new()
            .name("windroid-viewport".into())
            .spawn(move || {
                let run = || -> io::Result<()> {
                    if conn.stopped.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let target = match source.package {
                        Some(package) => DisplayTarget::App { package },
                        None => DisplayTarget::Device,
                    };
                    let session = VideoSession::start_cancellable(
                        &source.adb,
                        &source.serial,
                        &source.jar,
                        target,
                        || conn.stopped.load(Ordering::Acquire),
                    )?;
                    let control = session.control();
                    // 用户可能在推送 jar 或握手时已关窗。
                    if !conn.attach(control.clone()) {
                        return Ok(());
                    }
                    report(Notice::Connected(session.display_id()));
                    let (outgoing, writer_conn) = (events.clone(), conn.clone());
                    let writer =
                        thread::Builder::new()
                            .name("windroid-input".into())
                            .spawn(move || {
                                while let Some(event) = outgoing.next() {
                                    let sent = match event {
                                        Outgoing::Event(input) => control.send(input),
                                        Outgoing::Resize(w, h) => control.resize(w, h),
                                    };
                                    if let Err(error) = sent {
                                        if !writer_conn.stopped.load(Ordering::Acquire) {
                                            writer_conn.fail(format!("输入通道已断开：{error}"));
                                        }
                                        break;
                                    }
                                }
                            })?;
                    let (notify_frame, alive) = (report.clone(), conn.clone());
                    let result = session.run(move |frame| {
                        if !alive.stopped.load(Ordering::Acquire) && frames.publish(frame) {
                            notify_frame(Notice::FrameReady);
                        }
                    });
                    events.close();
                    conn.stop();
                    let _ = writer.join();
                    result
                };
                let result = run().map_err(|err| err.to_string());
                conn.stop();
                events.close();
                let result = conn.failure.lock().unwrap().take().map_or(result, Err);
                report(Notice::Ended(result));
            })?;
        Ok(Self {
            connection,
            input,
            latest,
            thread: Some(thread),
        })
    }

    pub fn input_handler(&self) -> Box<dyn Fn(InputEvent)> {
        let (input, connection) = (self.input.clone(), self.connection.clone());
        Box::new(move |event| {
            if input.push(event).is_err() {
                input.close();
                connection.fail("设备未及时接收输入，请重新打开窗口".into());
            }
        })
    }

    pub fn resize_handler(&self) -> Box<dyn Fn(u16, u16)> {
        let queue = self.input.clone();
        Box::new(move |width, height| queue.resize(width, height))
    }

    pub fn take_frame(&self) -> Option<Frame> {
        self.latest.take()
    }

    pub fn cancel(&self) {
        self.connection.stop();
        self.input.close();
        self.latest.take();
    }

    /// 只在工作线程报告清理完毕，或 AppKit 事件循环已经退出时使用。
    pub fn join(mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(byte: u8) -> Frame {
        Frame {
            bytes: vec![byte; 4],
            width: 1,
            height: 1,
        }
    }
    fn touch(action: u8, x: i32) -> InputEvent {
        InputEvent::Touch {
            action,
            x,
            y: 0,
            width: 100,
            height: 80,
            action_button: 1,
            buttons: 1,
        }
    }

    #[test]
    fn each_viewport_owns_a_single_latest_frame_and_wakeup() {
        let (a, b) = (Latest::default(), Latest::default());
        assert!(a.publish(frame(1)));
        assert!(!a.publish(frame(2)));
        assert!(b.publish(frame(3)));
        assert_eq!(b.take().unwrap().bytes[0], 3);
        assert_eq!(a.take().unwrap().bytes[0], 2);
        assert!(a.take().is_none());
        assert!(a.publish(frame(4)));
    }
    #[test]
    fn motion_coalesces_between_a_down_and_its_release() {
        let q = InputQueue::default();
        for e in [touch(0, 0), touch(2, 10), touch(2, 20), touch(1, 20)] {
            q.push(e).unwrap();
        }
        for (action, x) in [(0, 0), (2, 20), (1, 20)] {
            assert!(
                matches!(q.next(),Some(Outgoing::Event(InputEvent::Touch{action:a,x:b,..})) if (a,b)==(action,x))
            );
        }
        q.close();
        assert!(q.next().is_none());
    }
    #[test]
    fn resize_keeps_only_latest_request_and_uses_its_own_queue() {
        let app = InputQueue::default();
        let device = InputQueue::default();
        app.resize(720, 1280);
        app.resize(1024, 768);
        assert!(matches!(app.next(), Some(Outgoing::Resize(1024, 768))));
        device.close();
        assert!(device.next().is_none());
    }

    #[test]
    fn stalled_control_fails_explicitly_and_shutdown_wakes_reader() {
        let q = Arc::new(InputQueue::default());
        for _ in 0..MAX_INPUT {
            q.push(touch(0, 0)).unwrap();
        }
        assert!(q.push(touch(1, 0)).is_err());
        q.close();
        assert!(q.next().is_none());
        let other = Arc::new(InputQueue::default());
        let reader = other.clone();
        let waiting = thread::spawn(move || reader.next());
        other.close();
        assert!(waiting.join().unwrap().is_none());
    }
}
