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
    time::{Duration, Instant},
};
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Data { sequence: u64, bytes: Vec<u8> },
    Exit { code: u32 },
    Error { message: &'static str },
}
struct Session {
    pid: Option<u32>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    input: SyncSender<Vec<u8>>,
    output: Mutex<Receiver<Event>>,
    cancel: Arc<AtomicBool>,
}
#[derive(Default)]
pub struct Terminals {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
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
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|_| "terminal process failed to start")?;
        drop(pair.slave);
        let pid = child.process_id();
        let child = Arc::new(Mutex::new(child));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(32);
        let (input, input_rx) = mpsc::sync_channel::<Vec<u8>>(8);
        let read_cancel = cancel.clone();
        let read_child = child.clone();
        let read_tx = tx.clone();
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
                pid,
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
        // Only this mutex can reap the child. Hold it across liveness check and
        // signals so a completed process ID can never be reused underneath us.
        let mut child = s.child.lock().map_err(|_| "terminal process unavailable")?;
        if child
            .try_wait()
            .map_err(|_| "terminal process unavailable")?
            .is_some()
        {
            return Ok(());
        }
        let pid = s.pid.map(|p| nix::unistd::Pid::from_raw(p as i32));
        if let Some(pid) = pid {
            let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if child
                .try_wait()
                .map_err(|_| "terminal process unavailable")?
                .is_some()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                if let Some(pid) = pid {
                    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
                }
                let _ = child.kill();
                child
                    .wait()
                    .map_err(|_| "terminal process cleanup failed")?;
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn close_all(&self) {
        let ids = self
            .sessions
            .lock()
            .map(|s| s.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for id in ids {
            let _ = self.close(&id);
        }
    }
}
impl Drop for Terminals {
    fn drop(&mut self) {
        self.close_all();
    }
}
fn size(rows: u16, cols: u16) -> Result<(), &'static str> {
    if rows == 0 || cols == 0 || rows > 500 || cols > 1000 {
        Err("invalid terminal size")
    } else {
        Ok(())
    }
}
