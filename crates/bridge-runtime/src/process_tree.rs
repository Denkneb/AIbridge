//! Linux process ownership pinned with pidfds, including detached descendants.
use crate::RuntimeError;
use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
use std::{
    collections::HashMap,
    fs,
    os::fd::OwnedFd,
    time::{Duration, Instant},
};

struct Member {
    pid: i32,
    start: String,
    fd: OwnedFd,
}
/// An owned process identity; capture immediately after spawning, before reaping.
pub struct ProcessTree {
    root: Member,
    boot: String,
    stop_lock: std::sync::Mutex<()>,
}
struct Stat {
    pid: i32,
    parent: i32,
    session: i32,
    start: String,
}
fn stat(pid: i32) -> Option<Stat> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, tail) = raw.rsplit_once(") ")?;
    let fields = tail.split_whitespace().collect::<Vec<_>>();
    Some(Stat {
        pid,
        parent: fields.get(1)?.parse().ok()?,
        session: fields.get(3)?.parse().ok()?,
        start: fields.get(19)?.to_string(),
    })
}
fn pin(pid: i32, start: &str) -> Result<Member, RuntimeError> {
    if pid <= 1 || pid == std::process::id() as i32 {
        return Err(RuntimeError::ForeignProcess);
    }
    let fd = pidfd_open(
        Pid::from_raw(pid).ok_or(RuntimeError::Record)?,
        PidfdFlags::empty(),
    )
    .map_err(|_| RuntimeError::Io)?;
    if stat(pid).is_none_or(|s| s.start != start) {
        return Err(RuntimeError::ForeignProcess);
    }
    Ok(Member {
        pid,
        start: start.into(),
        fd,
    })
}
impl ProcessTree {
    pub fn capture(pid: u32) -> Result<Self, RuntimeError> {
        let pid = i32::try_from(pid).map_err(|_| RuntimeError::Record)?;
        let start = stat(pid).ok_or(RuntimeError::Spawn)?.start;
        Self::from_identity(pid, &start, &crate::process::boot_id()?)
    }
    pub fn from_identity(pid: i32, start: &str, boot: &str) -> Result<Self, RuntimeError> {
        if crate::process::boot_id()? != boot {
            return Err(RuntimeError::ForeignProcess);
        }
        Ok(Self {
            root: pin(pid, start)?,
            boot: boot.into(),
            stop_lock: std::sync::Mutex::new(()),
        })
    }
    /// Stops descendants even when they create their own process sessions. A
    /// proven session leader also owns orphaned members of its original session.
    pub fn is_alive(&self) -> bool {
        crate::process::identity(self.root.pid).as_deref() == Some(&self.root.start)
    }
    pub fn stop(&self) -> Result<(), RuntimeError> {
        let _guard = self.stop_lock.lock().map_err(|_| RuntimeError::Io)?;
        if crate::process::boot_id()? != self.boot {
            return Err(RuntimeError::ForeignProcess);
        }
        let Some(root_stat) = stat(self.root.pid).filter(|s| s.start == self.root.start) else {
            return Ok(());
        };
        // Freeze the parent before discovering children, so it cannot spawn more.
        let _ = pidfd_send_signal(&self.root.fd, Signal::STOP);
        let mut members = HashMap::<i32, Member>::new();
        let result = (|| {
            for _ in 0..8 {
                let stats = fs::read_dir("/proc")
                    .map_err(|_| RuntimeError::Io)?
                    .filter_map(Result::ok)
                    .filter_map(|entry| entry.file_name().to_str()?.parse().ok().and_then(stat))
                    .collect::<Vec<_>>();
                let mut changed = false;
                // Repeated passes cover arbitrary directory iteration order.
                loop {
                    let mut added = false;
                    for s in &stats {
                        if s.pid == self.root.pid || members.contains_key(&s.pid) {
                            continue;
                        }
                        let parent = if s.parent == self.root.pid {
                            Some(&self.root)
                        } else {
                            members.get(&s.parent)
                        };
                        let descendant = parent.is_some_and(|p| {
                            stat(p.pid).is_some_and(|current| current.start == p.start)
                                && s.start.parse::<u64>().ok() >= p.start.parse::<u64>().ok()
                        });
                        let session = root_stat.session == self.root.pid
                            && s.session == self.root.pid
                            && stat(self.root.pid)
                                .is_some_and(|root| root.start == self.root.start);
                        if (descendant || session)
                            && let Ok(member) = pin(s.pid, &s.start)
                        {
                            let _ = pidfd_send_signal(&member.fd, Signal::STOP);
                            members.insert(s.pid, member);
                            added = true;
                            changed = true;
                        }
                    }
                    if !added {
                        break;
                    }
                }
                if !changed {
                    break;
                }
            }
            let all = members
                .values()
                .chain(std::iter::once(&self.root))
                .collect::<Vec<_>>();
            for m in &all {
                let _ = pidfd_send_signal(&m.fd, Signal::TERM);
                let _ = pidfd_send_signal(&m.fd, Signal::CONT);
            }
            let deadline = Instant::now() + Duration::from_millis(500);
            while all
                .iter()
                .any(|m| crate::process::identity(m.pid).as_deref() == Some(&m.start))
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            for m in &all {
                let _ = pidfd_send_signal(&m.fd, Signal::KILL);
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while all
                .iter()
                .any(|m| crate::process::identity(m.pid).as_deref() == Some(&m.start))
            {
                if Instant::now() >= deadline {
                    return Err(RuntimeError::Io);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        })();
        // A discovery error must never leave an owned process frozen.
        for member in members.values().chain(std::iter::once(&self.root)) {
            let _ = pidfd_send_signal(&member.fd, Signal::CONT);
        }
        result
    }
}
