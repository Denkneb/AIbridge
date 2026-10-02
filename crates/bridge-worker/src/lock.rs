//! Project-scoped, non-blocking worker lock (task 7.2).
//!
//! This module is the reusable lock foundation the future worker state machine
//! and MCP wiring build on. It deliberately stops before session resolution,
//! round execution, the worker state machine and any MCP/runtime wiring: it
//! only owns the project `worker.lock` artifact.
//!
//! # Lock contract
//!
//! [`WorkerLock::try_acquire`] opens the Rust-owned project worker lock derived
//! from an existing [`RustStateLayout`] (`<state-root>/<project>/worker.lock`,
//! the [`RuntimeLock::Worker`] artifact) and takes an **exclusive,
//! non-blocking** OS lock with BSD `flock`, exactly compatible with the Python
//! reference `worker.py::worker_lock` (`fcntl.flock` with `LOCK_EX | LOCK_NB`).
//! The acquisition is typed: [`WorkerLockOutcome::Acquired`] carries an RAII
//! [`WorkerLock`] guard, while [`WorkerLockOutcome::Busy`] means another worker
//! (in this or any other process) currently holds the lock.
//!
//! The guard holds the lock until it is dropped; dropping closes the underlying
//! file description, which releases the `flock`, and the kernel also releases
//! every `flock` when the process exits, so a crashed worker never leaves a
//! stale held lock. Different projects use different lock paths, so they never
//! block each other. The mere *existence* of `worker.lock` does **not** mean the
//! lock is held: [`WorkerLock::is_free`]/[`WorkerLock::is_held`] probe the
//! actual lock state, and a leftover file is simply reused.
//!
//! The lock file is opened `O_RDWR | O_CREAT` with mode `0o600` on Unix, like
//! the reference. An existing lock file is neither deleted nor recreated nor
//! truncated: opening it only changes its access time and leaves its inode and
//! content untouched. The containing project directory is never created here
//! either; a valid layout already has it.
//!
//! # Fail-closed ownership
//!
//! Before any lock artifact is opened or created, the layout is verified with
//! the existing production ownership guard [`RustStateLayout::open`] (marker,
//! format version, project namespace, normalized state root, `meta.runtime_owner`
//! and schema v6). A missing, foreign or mismatched state — for example a Python
//! state root or a Rust state copied under another root — returns
//! [`WorkerLockErrorKind::StateOwnership`] before the lock file is touched, so
//! no foreign state is ever modified and only the Rust-owned state root is used.
//!
//! # Errors and redaction
//!
//! [`WorkerLockError`] is a typed error whose [`Display`](fmt::Display) and
//! [`Debug`](fmt::Debug) expose only fixed, developer-authored labels. The state
//! root, project id, lock path and lock content never appear in an error; the
//! underlying I/O or storage error is reachable only through
//! [`std::error::Error::source`]. `unsafe_code = "forbid"` is preserved: the
//! lock uses the safe `nix::fcntl::flock` wrapper.

use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};

use bridge_storage::{RuntimeLock, RustStateLayout};

#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::fcntl::{FlockArg, flock};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

/// Broad, typed category of a [`WorkerLockError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WorkerLockErrorKind {
    /// The state layout is not a valid, initialized Rust-owned state.
    StateOwnership,
    /// The Rust-owned worker lock file could not be opened.
    LockFile,
    /// The worker lock could not be acquired or probed.
    Lock,
    /// The platform has no compatible non-blocking OS lock.
    Unsupported,
}

impl WorkerLockErrorKind {
    /// Returns a short, non-sensitive label for this category.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StateOwnership => "rust state ownership could not be verified",
            Self::LockFile => "worker lock file could not be opened",
            Self::Lock => "worker lock could not be acquired",
            Self::Unsupported => "worker lock is unsupported on this platform",
        }
    }
}

impl fmt::Display for WorkerLockErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed, safe error raised while acquiring or probing the worker lock.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message and [`Debug`](fmt::Debug) shows only the category. Neither ever
/// contains the state root, project id, lock path or lock content. The
/// underlying I/O or storage error, when present, is reachable only through
/// [`Error::source`].
pub struct WorkerLockError {
    kind: WorkerLockErrorKind,
    source: Option<Box<dyn Error + Send + Sync + 'static>>,
}

impl WorkerLockError {
    /// Creates an error of the given `kind` without a source.
    #[cfg(not(unix))]
    fn new(kind: WorkerLockErrorKind) -> Self {
        Self { kind, source: None }
    }

    /// Creates an error of the given `kind` with an internal diagnostic source.
    fn with_source(kind: WorkerLockErrorKind, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            source: Some(Box::new(source)),
        }
    }

    /// Returns the category of this error.
    #[must_use]
    pub const fn kind(&self) -> WorkerLockErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.as_str())
    }
}

impl fmt::Debug for WorkerLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerLockError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Error for WorkerLockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.source {
            Some(source) => Some(source.as_ref()),
            None => None,
        }
    }
}

/// The typed result of a non-blocking worker lock acquisition.
#[derive(Debug)]
pub enum WorkerLockOutcome {
    /// The caller now owns the project worker lock until the guard is dropped.
    Acquired(WorkerLock),
    /// Another worker (in this or any other process) holds the lock.
    Busy,
}

/// RAII guard that holds the exclusive project worker lock.
///
/// The lock is released when this value is dropped and also when the owning
/// process exits, so it can never be leaked. The guard deliberately exposes no
/// lock path: callers only observe the typed acquisition outcome.
pub struct WorkerLock {
    /// The open file description that owns the `flock`; kept alive so the lock
    /// is held for exactly as long as the guard.
    _file: File,
}

impl WorkerLock {
    /// Tries to acquire the project worker lock without blocking.
    ///
    /// The Rust-owned state is verified with [`RustStateLayout::open`] first, so
    /// a missing, foreign or mismatched state fails closed before the lock file
    /// is opened or created.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerLockErrorKind::StateOwnership`] when the layout is not a
    /// valid Rust-owned state, [`WorkerLockErrorKind::LockFile`] when the lock
    /// file cannot be opened, [`WorkerLockErrorKind::Lock`] when the OS lock
    /// call itself fails, and [`WorkerLockErrorKind::Unsupported`] on platforms
    /// without a compatible non-blocking lock. No error message contains a path
    /// or identifier.
    pub fn try_acquire(layout: &RustStateLayout) -> Result<WorkerLockOutcome, WorkerLockError> {
        verify_ownership(layout)?;
        acquire(layout)
    }

    /// Reports whether the project worker lock is currently held.
    ///
    /// This is a probe: it never leaves the lock held. A leftover `worker.lock`
    /// file with no holder reports `false`.
    ///
    /// # Errors
    ///
    /// Returns the same typed categories as [`WorkerLock::try_acquire`].
    pub fn is_held(layout: &RustStateLayout) -> Result<bool, WorkerLockError> {
        match Self::try_acquire(layout)? {
            WorkerLockOutcome::Acquired(guard) => {
                drop(guard);
                Ok(false)
            }
            WorkerLockOutcome::Busy => Ok(true),
        }
    }

    /// Reports whether the project worker lock is currently free.
    ///
    /// This is the boolean probe matching the Python reference
    /// `worker.py::worker_lock_is_free`; it never leaves the lock held.
    ///
    /// # Errors
    ///
    /// Returns the same typed categories as [`WorkerLock::try_acquire`].
    pub fn is_free(layout: &RustStateLayout) -> Result<bool, WorkerLockError> {
        Self::is_held(layout).map(|held| !held)
    }
}

impl fmt::Debug for WorkerLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerLock").finish_non_exhaustive()
    }
}

/// Verifies Rust state ownership/namespace through the production guard.
fn verify_ownership(layout: &RustStateLayout) -> Result<(), WorkerLockError> {
    layout
        .open()
        .map(drop)
        .map_err(|error| WorkerLockError::with_source(WorkerLockErrorKind::StateOwnership, error))
}

/// Opens the Rust-owned project worker lock file with mode `0o600` on Unix.
#[cfg(unix)]
fn open_lock_file(layout: &RustStateLayout) -> Result<File, WorkerLockError> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(layout.lock(RuntimeLock::Worker))
        .map_err(|error| WorkerLockError::with_source(WorkerLockErrorKind::LockFile, error))
}

/// Takes the exclusive non-blocking lock on the Rust-owned worker lock file.
#[cfg(unix)]
fn acquire(layout: &RustStateLayout) -> Result<WorkerLockOutcome, WorkerLockError> {
    let file = open_lock_file(layout)?;
    match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
        Ok(()) => Ok(WorkerLockOutcome::Acquired(WorkerLock { _file: file })),
        Err(Errno::EWOULDBLOCK) => Ok(WorkerLockOutcome::Busy),
        Err(errno) => Err(WorkerLockError::with_source(
            WorkerLockErrorKind::Lock,
            errno,
        )),
    }
}

/// Reports that no compatible non-blocking lock exists on this platform.
#[cfg(not(unix))]
fn acquire(_layout: &RustStateLayout) -> Result<WorkerLockOutcome, WorkerLockError> {
    Err(WorkerLockError::new(WorkerLockErrorKind::Unsupported))
}

#[cfg(test)]
mod tests {
    use super::{WorkerLock, WorkerLockErrorKind, WorkerLockOutcome};
    use bridge_domain::ProjectId;
    use bridge_storage::{RuntimeLock, RustStateLayout};
    use std::error::Error;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bridge-worker-lock-{label}-{nanos}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temporary root");
        path
    }

    fn project(id: &str) -> ProjectId {
        ProjectId::from_str(id).expect("valid project id")
    }

    fn initialized(root: &Path, id: &str) -> RustStateLayout {
        let layout = RustStateLayout::new(root.join("state"), project(id)).expect("layout");
        layout.initialize().expect("rust state must initialize");
        layout
    }

    #[test]
    fn acquired_guard_is_exclusive_and_released_on_drop() {
        let root = unique_root("exclusive");
        let layout = initialized(&root, "proj-1");

        let first = WorkerLock::try_acquire(&layout).expect("acquire");
        assert!(matches!(first, WorkerLockOutcome::Acquired(_)));
        assert!(WorkerLock::is_held(&layout).expect("probe"));
        assert!(!WorkerLock::is_free(&layout).expect("probe"));

        let second = WorkerLock::try_acquire(&layout).expect("acquire");
        assert!(matches!(second, WorkerLockOutcome::Busy));

        drop(first);
        assert!(WorkerLock::is_free(&layout).expect("probe"));
        assert!(matches!(
            WorkerLock::try_acquire(&layout).expect("acquire"),
            WorkerLockOutcome::Acquired(_)
        ));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn different_projects_do_not_block_each_other() {
        let root = unique_root("projects");
        let first_layout = initialized(&root, "proj-1");
        let second_layout = initialized(&root, "proj-2");

        let first = WorkerLock::try_acquire(&first_layout).expect("acquire");
        assert!(matches!(first, WorkerLockOutcome::Acquired(_)));

        let second = WorkerLock::try_acquire(&second_layout).expect("acquire");
        assert!(matches!(second, WorkerLockOutcome::Acquired(_)));
        assert!(WorkerLock::is_held(&first_layout).expect("probe"));
        assert!(WorkerLock::is_held(&second_layout).expect("probe"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn existing_lock_file_without_a_holder_is_free() {
        let root = unique_root("leftover");
        let layout = initialized(&root, "proj-1");
        let lock_path = layout.lock(RuntimeLock::Worker);
        fs::write(&lock_path, "leftover\n").expect("leftover lock file");

        assert!(WorkerLock::is_free(&layout).expect("probe"));
        let guard = WorkerLock::try_acquire(&layout).expect("acquire");
        assert!(matches!(guard, WorkerLockOutcome::Acquired(_)));

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_is_created_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_root("mode");
        let layout = initialized(&root, "proj-1");
        let lock_path = layout.lock(RuntimeLock::Worker);

        let guard = WorkerLock::try_acquire(&layout).expect("acquire");
        assert!(matches!(guard, WorkerLockOutcome::Acquired(_)));
        let mode = fs::metadata(&lock_path)
            .expect("lock metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the worker lock must be owner-only");

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn existing_lock_file_is_neither_recreated_nor_truncated() {
        use std::os::unix::fs::MetadataExt;

        let root = unique_root("preserve");
        let layout = initialized(&root, "proj-1");
        let lock_path = layout.lock(RuntimeLock::Worker);
        fs::write(&lock_path, "sentinel-content\n").expect("sentinel lock file");
        let before = fs::metadata(&lock_path).expect("metadata");
        let inode_before = before.ino();

        let guard = WorkerLock::try_acquire(&layout).expect("acquire");
        assert!(matches!(guard, WorkerLockOutcome::Acquired(_)));
        let after = fs::metadata(&lock_path).expect("metadata");
        assert_eq!(
            after.ino(),
            inode_before,
            "the lock file must not be recreated"
        );
        assert_eq!(
            fs::read_to_string(&lock_path).expect("lock content"),
            "sentinel-content\n",
            "the lock file must not be truncated"
        );
        drop(guard);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_state_fails_closed_without_creating_a_lock_file() {
        let root = unique_root("missing");
        let layout = RustStateLayout::new(root.join("state"), project("proj-1")).expect("layout");

        let error = WorkerLock::try_acquire(&layout).expect_err("missing state must fail");
        assert_eq!(error.kind(), WorkerLockErrorKind::StateOwnership);
        assert!(!layout.lock(RuntimeLock::Worker).exists());
        assert!(!layout.project_dir().exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn foreign_marker_fails_closed_without_creating_a_lock_file() {
        let root = unique_root("foreign");
        let layout = RustStateLayout::new(root.join("state"), project("proj-1")).expect("layout");
        fs::create_dir_all(layout.project_dir()).expect("project dir");
        fs::write(
            layout.marker(),
            r#"{"implementation":"python","format_version":1,"project_id":"proj-1","state_root":"00"}"#,
        )
        .expect("foreign marker");

        let error = WorkerLock::try_acquire(&layout).expect_err("foreign state must fail");
        assert_eq!(error.kind(), WorkerLockErrorKind::StateOwnership);
        assert!(!layout.lock(RuntimeLock::Worker).exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn copied_state_under_another_root_fails_closed() {
        let root = unique_root("copied");
        let source = initialized(&root.join("source"), "proj-1");
        let other = RustStateLayout::new(root.join("other"), project("proj-1")).expect("layout");
        fs::create_dir_all(other.project_dir()).expect("other project dir");
        fs::copy(source.marker(), other.marker()).expect("copy marker");

        let error = WorkerLock::try_acquire(&other).expect_err("copied state must fail");
        assert_eq!(error.kind(), WorkerLockErrorKind::StateOwnership);
        assert!(!other.lock(RuntimeLock::Worker).exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ownership_error_is_redacted() {
        let root = unique_root("redaction");
        let layout = RustStateLayout::new(root.join("secret-state"), project("secret-proj"))
            .expect("layout");

        let error = WorkerLock::try_acquire(&layout).expect_err("missing state must fail");
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains("secret-state"));
        assert!(!rendered.contains("secret-proj"));
        assert!(!rendered.contains("worker.lock"));
        assert!(error.source().is_some(), "the guard cause stays in source");

        let _ = fs::remove_dir_all(&root);
    }
}
