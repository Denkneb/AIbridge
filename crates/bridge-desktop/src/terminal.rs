//! Ordered byte streaming with bounded queues and explicit session ownership.
use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use serde::Serialize;
use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::Duration,
};
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Data { sequence: u64, bytes: Vec<u8> },
    Exit { code: u32 },
    Error { message: &'static str },
}
struct Session {
    tree: Arc<bridge_runtime::process_tree::ProcessTree>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    input: SyncSender<Vec<u8>>,
    output: Mutex<Receiver<Event>>,
    cancel: Arc<AtomicBool>,
}
#[derive(Default)]
pub struct Terminals {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    closing: AtomicBool,
    external: Mutex<Vec<bridge_runtime::process_tree::ProcessTree>>,
}
fn enqueue(tx: &SyncSender<Event>, mut event: Event, cancel: &AtomicBool) -> bool {
    loop {
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        match tx.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Full(e)) => {
                event = e;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}
impl Terminals {
    /// Commands come from the trusted Rust router, never frontend argv or shell text.
    pub fn open(&self, cmd: CommandBuilder, rows: u16, cols: u16) -> Result<String, &'static str> {
        size(rows, cols)?;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "terminal state unavailable")?;
        if self.closing.load(Ordering::Acquire) {
            return Err("application is shutting down");
        }
        if sessions.len() >= 8 {
            return Err("terminal session limit reached");
        }
        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|_| "PTY unavailable")?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|_| "PTY reader unavailable")?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|_| "PTY writer unavailable")?;
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|_| "terminal process failed to start")?;
        drop(pair.slave);
        let tree = match child
            .process_id()
            .ok_or("terminal process unavailable")
            .and_then(|pid| {
                bridge_runtime::process_tree::ProcessTree::capture(pid)
                    .map_err(|_| "terminal ownership unavailable")
            }) {
            Ok(tree) => Arc::new(tree),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let child = Arc::new(Mutex::new(child));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(32);
        let (input, input_rx) = mpsc::sync_channel::<Vec<u8>>(8);
        let read_cancel = cancel.clone();
        let read_child = child.clone();
        let read_tx = tx.clone();
        let read_tree = tree.clone();
        std::thread::spawn(move || {
            let mut sequence = 0;
            let mut bytes = [0u8; 8192];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => {
                        if !enqueue(
                            &read_tx,
                            Event::Data {
                                sequence,
                                bytes: bytes[..n].to_vec(),
                            },
                            &read_cancel,
                        ) {
                            return;
                        }
                        sequence += 1;
                    }
                    Err(_) => break,
                }
            }
            let _ = read_tree.stop();
            let code = loop {
                if read_cancel.load(Ordering::Acquire) {
                    return;
                }
                let state = read_child
                    .lock()
                    .ok()
                    .and_then(|mut c| c.try_wait().ok())
                    .flatten();
                if let Some(status) = state {
                    break status.exit_code();
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            enqueue(&read_tx, Event::Exit { code }, &read_cancel);
        });
        let write_cancel = cancel.clone();
        std::thread::spawn(move || {
            while !write_cancel.load(Ordering::Acquire) {
                match input_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(bytes) => {
                        if writer
                            .write_all(&bytes)
                            .and_then(|()| writer.flush())
                            .is_err()
                        {
                            enqueue(
                                &tx,
                                Event::Error {
                                    message: "terminal input failed",
                                },
                                &write_cancel,
                            );
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => break,
                }
            }
        });
        let id = uuid::Uuid::new_v4().to_string();
        sessions.insert(
            id.clone(),
            Arc::new(Session {
                tree,
                master: Mutex::new(pair.master),
                child,
                input,
                output: Mutex::new(rx),
                cancel,
            }),
        );
        Ok(id)
    }
    fn session(&self, id: &str) -> Result<Arc<Session>, &'static str> {
        self.sessions
            .lock()
            .map_err(|_| "terminal state unavailable")?
            .get(id)
            .cloned()
            .ok_or("terminal session closed")
    }
    pub fn read(&self, id: &str) -> Result<Vec<Event>, &'static str> {
        let s = self.session(id)?;
        let receiver = s.output.lock().map_err(|_| "terminal queue unavailable")?;
        Ok(receiver.try_iter().take(16).collect())
    }
    pub fn write(&self, id: &str, bytes: Vec<u8>) -> Result<(), &'static str> {
        if bytes.len() > 16384 {
            return Err("terminal input too large");
        }
        self.session(id)?
            .input
            .try_send(bytes)
            .map_err(|_| "terminal input queue full")
    }
    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<(), &'static str> {
        size(rows, cols)?;
        self.session(id)?
            .master
            .lock()
            .map_err(|_| "terminal unavailable")?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|_| "terminal resize failed")
    }
    pub fn close(&self, id: &str) -> Result<(), &'static str> {
        let session = self
            .sessions
            .lock()
            .map_err(|_| "terminal state unavailable")?
            .remove(id);
        let Some(s) = session else {
            return Ok(());
        };
        s.cancel.store(true, Ordering::Release);
        let mut child = s.child.lock().map_err(|_| "terminal process unavailable")?;
        s.tree
            .stop()
            .map_err(|_| "terminal process cleanup failed")?;
        child
            .wait()
            .map_err(|_| "terminal process cleanup failed")?;
        Ok(())
    }
    /// External terminal launchers are owned by the same shutdown fence.
    pub fn open_external(&self, mut command: std::process::Command) -> Result<(), &'static str> {
        let _sessions = self
            .sessions
            .lock()
            .map_err(|_| "terminal state unavailable")?;
        if self.closing.load(Ordering::Acquire) {
            return Err("application is shutting down");
        }
        let mut external = self
            .external
            .lock()
            .map_err(|_| "terminal state unavailable")?;
        external.retain(|tree| tree.is_alive());
        let mut child = command
            .spawn()
            .map_err(|_| "external terminal unavailable: install x-terminal-emulator")?;
        match bridge_runtime::process_tree::ProcessTree::capture(child.id()) {
            Ok(tree) => external.push(tree),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("external terminal ownership unavailable");
            }
        }
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
    pub fn begin_shutdown(&self) {
        self.closing.store(true, Ordering::Release);
    }
    pub fn shutdown(&self) -> Result<(), &'static str> {
        {
            let _sessions = self
                .sessions
                .lock()
                .map_err(|_| "terminal state unavailable")?;
            self.begin_shutdown();
        }
        let mut result = self.close_all_result();
        match self.external.lock() {
            Ok(mut external) => {
                for tree in external.drain(..) {
                    if tree.stop().is_err() {
                        result = Err("external terminal cleanup failed");
                    }
                }
            }
            Err(_) => result = Err("terminal state unavailable"),
        }
        result
    }
    fn close_all_result(&self) -> Result<(), &'static str> {
        let ids = self
            .sessions
            .lock()
            .map_err(|_| "terminal state unavailable")?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut result = Ok(());
        for id in ids {
            if let Err(error) = self.close(&id) {
                result = Err(error);
            }
        }
        result
    }
    pub fn close_all(&self) {
        let _ = self.close_all_result();
    }
}
impl Drop for Terminals {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
fn size(rows: u16, cols: u16) -> Result<(), &'static str> {
    if rows == 0 || cols == 0 || rows > 500 || cols > 1000 {
        Err("invalid terminal size")
    } else {
        Ok(())
    }
}
