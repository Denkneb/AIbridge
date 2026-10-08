use crate::RuntimeError;
use bridge_domain::{ProjectId, TaskId};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessRecord {
    pub pid: i32,
    pub start: String,
    pub boot_id: String,
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub checkout: PathBuf,
    pub kind: String,
    pub port: u16,
    pub endpoint: String,
}
pub(crate) fn boot_id() -> Result<String, RuntimeError> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().into())
        .map_err(|_| RuntimeError::Unsupported)
}
pub(crate) fn identity(pid: i32) -> Option<String> {
    if pid <= 1 {
        return None;
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    let parts = fields.split_whitespace().collect::<Vec<_>>();
    if matches!(*parts.first()?, "Z" | "X") {
        return None;
    }
    let start = *parts.get(19)?;
    start
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| start.into())
}
pub(crate) fn read_record(path: &Path) -> Result<Option<ProcessRecord>, RuntimeError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(RuntimeError::Record),
    };
    let meta = file.metadata().map_err(|_| RuntimeError::Record)?;
    if !meta.is_file() || meta.len() > 16384 {
        return Err(RuntimeError::Record);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(RuntimeError::Record);
        }
    }
    let mut raw = String::new();
    file.take(16385)
        .read_to_string(&mut raw)
        .map_err(|_| RuntimeError::Record)?;
    let r: ProcessRecord = serde_json::from_str(&raw).map_err(|_| RuntimeError::Record)?;
    if r.pid <= 1
        || r.start.is_empty()
        || !r.start.bytes().all(|b| b.is_ascii_digit())
        || r.boot_id.is_empty()
        || !r.checkout.is_absolute()
        || r.port == 0
    {
        return Err(RuntimeError::Record);
    }
    Ok(Some(r))
}
pub(crate) fn write_record(path: &Path, record: &ProcessRecord) -> Result<(), RuntimeError> {
    let temporary = path.with_extension(format!("json.tmp-{}", record.pid));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    let mut file = options.open(&temporary).map_err(|_| RuntimeError::Io)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec(record).map_err(|_| RuntimeError::Record)?)
            .map_err(|_| RuntimeError::Io)?;
        file.sync_all().map_err(|_| RuntimeError::Io)?;
        fs::rename(&temporary, path).map_err(|_| RuntimeError::Io)?;
        fs::File::open(path.parent().ok_or(RuntimeError::Binding)?)
            .and_then(|f| f.sync_all())
            .map_err(|_| RuntimeError::Io)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
#[cfg(target_os = "linux")]
pub(crate) fn require_pidfd() -> Result<(), RuntimeError> {
    use rustix::process::{Pid, PidfdFlags, pidfd_open};
    let pid =
        Pid::from_raw(i32::try_from(std::process::id()).map_err(|_| RuntimeError::Unsupported)?)
            .ok_or(RuntimeError::Unsupported)?;
    pidfd_open(pid, PidfdFlags::empty())
        .map(|_| ())
        .map_err(|_| RuntimeError::Unsupported)
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn require_pidfd() -> Result<(), RuntimeError> {
    Err(RuntimeError::Unsupported)
}
#[cfg(target_os = "linux")]
pub(crate) fn stop_record(record: &ProcessRecord) -> Result<(), RuntimeError> {
    crate::process_tree::ProcessTree::from_identity(record.pid, &record.start, &record.boot_id)?
        .stop()
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn stop_record(_: &ProcessRecord) -> Result<(), RuntimeError> {
    Err(RuntimeError::Unsupported)
}
