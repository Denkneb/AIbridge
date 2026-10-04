//! Sorted, all-or-nothing manager locking with optional bounded startup wait.
use crate::RuntimeError;
use bridge_storage::RustStateLayout;
#[cfg(unix)]
use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    time::{Duration, Instant},
};
/// RAII handles; closing them releases every acquired kernel lock.
pub struct ManagerLock {
    _handles: Vec<File>,
}
impl ManagerLock {
    /// # Errors
    /// Verifies all Rust namespaces before touching locks. Default zero wait
    /// fails immediately; partial acquisition releases every fd before retry.
    pub fn acquire(layouts: &[&RustStateLayout], wait: Duration) -> Result<Self, RuntimeError> {
        if layouts.is_empty() {
            return Err(RuntimeError::Binding);
        }
        let mut roots = BTreeSet::new();
        for layout in layouts {
            layout.open().map_err(|_| RuntimeError::Ownership)?;
            roots.insert(std::fs::canonicalize(layout.state_root()).map_err(|_| RuntimeError::Io)?);
        }
        let deadline = Instant::now()
            .checked_add(wait)
            .ok_or(RuntimeError::LockTimeout)?;
        let mut delay = Duration::from_millis(20);
        loop {
            let mut handles = Vec::new();
            let mut busy = false;
            for root in &roots {
                let mut options = OpenOptions::new();
                options.read(true).write(true).create(true);
                #[cfg(unix)]
                options
                    .mode(0o600)
                    .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
                let file = options
                    .open(root.join("runtime.lock"))
                    .map_err(|_| RuntimeError::Io)?;
                if !file.metadata().map_err(|_| RuntimeError::Io)?.is_file() {
                    return Err(RuntimeError::Io);
                }
                #[cfg(unix)]
                match nix::fcntl::flock(
                    file.as_raw_fd(),
                    nix::fcntl::FlockArg::LockExclusiveNonblock,
                ) {
                    Ok(()) => handles.push(file),
                    Err(nix::errno::Errno::EWOULDBLOCK) => {
                        busy = true;
                        break;
                    }
                    Err(_) => return Err(RuntimeError::Io),
                }
                #[cfg(not(unix))]
                {
                    return Err(RuntimeError::Unsupported);
                }
            }
            if !busy {
                return Ok(Self { _handles: handles });
            }
            drop(handles);
            if wait.is_zero() {
                return Err(RuntimeError::LockBusy);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RuntimeError::LockTimeout);
            }
            std::thread::sleep(delay.min(remaining));
            delay = (delay * 2).min(Duration::from_millis(200));
        }
    }
}
