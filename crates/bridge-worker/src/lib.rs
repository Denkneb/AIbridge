//! Typed worker argv construction, detached spawn, project lock, startup grace,
//! round session resolution, the initial/revision prompt happy paths and the
//! current-round blockers, guarded observation and review publication services.
//!
//! This crate is the narrow, reusable foundation for running one observed
//! implementation round in a separate worker process. It builds the exact
//! reference argv and starts a detached child ([`WorkerInvocation`],
//! [`spawn_worker`]), owns the project-scoped non-blocking worker lock
//! ([`WorkerLock`]), provides the bounded startup-grace tracker
//! ([`StartupGrace`]), resolves exactly one dedicated OpenCode session for the
//! current round ([`resolve_round_session`]) and dispatches exactly one prompt
//! for an `implement` round ([`dispatch_initial_round`], [`initial_prompt`]) or
//! for a `revise` round ([`dispatch_revision_round`], [`revision_prompt`]).
//! Once a round is observing, [`handle_permission_blocker`] filters the pending
//! OpenCode permissions down to the current round session, persists `needs_user`
//! through the atomic [`bridge_storage::StorageConnection::finish_round`] write
//! and returns the typed user action. Session resolution, dispatch and the
//! permission blocker take the explicit [`bridge_storage::RustStateLayout`] and
//! open it through the same fail-closed
//! [`bridge_storage::RustStateLayout::open`] ownership guard used by the lock
//! and spawn paths, so only a verified Rust-owned state can be read or written.
//! Dispatch reuses the session resolver, renders the exact reference prompt,
//! persists the outbound message id and the delivery attempt through the
//! existing atomic storage APIs *before* the single `prompt_async` request and
//! records the successful `observing` transition. Separate services provide
//! question/permission blockers, auto-approval, recovery, message observation,
//! failed/delivery-unknown/deadline handling and main/external repository
//! publication after saved verification. The full worker state machine,
//! delegated MCP wiring and production CLI `worker` remain open.
//!
//! # Argv contract
//!
//! [`WorkerInvocation`] validates and stores the typed inputs of one worker
//! launch and renders the reference order verbatim:
//!
//! ```text
//! <absolute-agent-bridge> worker --project ID --config PATH --state-root PATH \
//!     --task TASK_ID --round N
//! ```
//!
//! The executable, the config path and the state root must all be absolute OS
//! paths. Production callers resolve the currently running Rust binary through
//! [`WorkerInvocation::from_current_exe`] (`current_exe`) and are responsible
//! for resolving config/state inputs against the caller's own working
//! directory before construction (the reference config loader does exactly this
//! with `Path.absolute()`), while tests and future wiring may inject an explicit
//! absolute executable. Requiring absolute inputs is deliberate: the parent
//! writes the [`RustStateLayout`] relative to *its* cwd while the child runs in
//! the workspace, so a relative `--config`/`--state-root` would silently resolve
//! against a different directory and break both config resolution and state
//! isolation. Arguments are carried as [`OsString`] and passed straight to
//! [`Command`], so spaces and non-ASCII text are preserved exactly and no shell,
//! PATH search or Python interpreter is ever involved. The identifiers are the
//! [`ProjectId`]/[`TaskId`] domain types and the round is a positive `u32`,
//! matching the persisted `round_number` range `1..=u32::MAX`.
//!
//! # Spawn contract
//!
//! [`spawn_worker`] first fails closed on the *state namespace*: the invocation
//! state root and project id must match the [`RustStateLayout`], and the layout
//! must already be a valid, initialized Rust-owned state as proven by the
//! existing [`RustStateLayout::open`] ownership guard (marker, format version,
//! project namespace, normalized state root, `meta.runtime_owner` and schema
//! v6). Only then does it enforce the private project state directory
//! (mode `0o700` on Unix) and append the child's stdout and stderr to the
//! Rust-owned `worker.log` derived from the same layout. The child runs with the
//! project workspace as its working directory, stdin on `/dev/null` and, on
//! Unix, in a brand-new session and process group (`setsid`, the reference
//! `start_new_session=True`) so it is detached from the caller. The caller is
//! never blocked until the worker finishes and dropping the returned
//! [`SpawnedWorker`] never kills it; the explicit [`SpawnedWorker::wait`],
//! [`SpawnedWorker::try_wait`] and [`SpawnedWorker::kill`] methods expose the
//! PID and the lifecycle the future runtime manager needs.
//!
//! No Python state is read, copied or initialized: every path comes from the
//! explicitly passed [`RustStateLayout`], whose state root must match the
//! invocation's `--state-root`. A foreign, missing or mismatched state (for
//! example a Python state or a project directory without the Rust marker) is
//! rejected before any directory is created, chmodded or logged to, so the
//! caller's state is never touched.
//!
//! # Errors and redaction
//!
//! [`WorkerError`] is a typed error whose [`Display`](fmt::Display) and
//! [`Debug`](fmt::Debug) expose only fixed, developer-authored labels. The
//! executable, config/state paths, project id, task id and round number never
//! appear in an error; the underlying I/O error is reachable only through
//! [`std::error::Error::source`].

pub mod admission;
pub mod auto_approval;
pub mod checkpoint;
pub mod dispatch;
pub mod findings;
pub mod lock;
pub mod permission;
pub mod prompt;
pub mod question;
pub mod recovery;
pub mod recovery_startup;
pub mod session;
pub mod startup;
pub mod usage;

pub mod acceptance;
pub mod automation;
mod blockers;
mod completion;
pub mod execution;
pub mod inheritance;
pub mod lifecycle;
pub mod observation;
pub mod observation_loop;
pub mod runner;
pub use dispatch::{
    DispatchError, DispatchErrorKind, DispatchedRound, dispatch_initial_round,
    dispatch_revision_round, dispatch_revision_round_with_trusted_roots, new_message_id,
};
pub use findings::{FindingsError, RevisionFindings, validate_revision_findings};
pub use lock::{WorkerLock, WorkerLockError, WorkerLockErrorKind, WorkerLockOutcome};
pub use permission::{
    NEEDS_USER_ERROR_CODE, PERMISSION_BLOCKER_REASON, PendingPermission, PermissionBlocker,
    PermissionBlockerError, PermissionBlockerErrorKind, PermissionBlockerOutcome, UserAction,
    UserActionKind, handle_permission_blocker,
};
pub use prompt::{
    initial_prompt, initial_prompt_with_profile, revision_prompt, revision_prompt_with_profile,
};
pub use session::{
    ResolvedSession, SessionResolutionError, SessionResolutionErrorKind, SessionResolutionSource,
    resolve_round_session, round_session_title,
};
pub use startup::{DEFAULT_STARTUP_GRACE, StartupGrace, StartupObservation};

pub mod quarantine;

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use bridge_domain::{ProjectId, TaskId};
use bridge_storage::{RuntimeLog, RustStateLayout};
#[cfg(unix)]
use process_wrap::std::ProcessSession;
use process_wrap::std::{ChildWrapper, CommandWrap};

/// Broad, typed category of a [`WorkerError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WorkerErrorKind {
    /// The worker executable is not an absolute path.
    InvalidExecutable,
    /// The workspace working directory is not an absolute path.
    InvalidWorkspace,
    /// The config path is empty or not an absolute path.
    InvalidConfigPath,
    /// The state root is empty or not an absolute path.
    InvalidStateRoot,
    /// The round number is zero.
    InvalidRound,
    /// The current executable path could not be resolved.
    CurrentExecutable,
    /// The invocation's state root differs from the state layout's root.
    StateRootMismatch,
    /// The invocation's project id differs from the state layout's project.
    ProjectMismatch,
    /// The state layout is not a valid, initialized Rust-owned state.
    StateOwnership,
    /// The private project state directory could not be prepared.
    StateDirectory,
    /// The Rust-owned worker log could not be opened.
    LogFile,
    /// The worker process could not be started.
    Spawn,
}

impl WorkerErrorKind {
    /// Returns a short, non-sensitive label for this category.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidExecutable => "worker executable must be an absolute path",
            Self::InvalidWorkspace => "worker workspace must be an absolute path",
            Self::InvalidConfigPath => "worker config path must be an absolute path",
            Self::InvalidStateRoot => "worker state root must be an absolute path",
            Self::InvalidRound => "worker round number must be positive",
            Self::CurrentExecutable => "current executable path could not be resolved",
            Self::StateRootMismatch => {
                "worker invocation state root does not match the rust state layout"
            }
            Self::ProjectMismatch => {
                "worker invocation project does not match the rust state layout"
            }
            Self::StateOwnership => "rust state ownership could not be verified",
            Self::StateDirectory => "worker state directory could not be prepared",
            Self::LogFile => "worker log file could not be opened",
            Self::Spawn => "worker process could not be started",
        }
    }
}

impl fmt::Display for WorkerErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed, safe error raised while building or spawning a worker.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message and [`Debug`](fmt::Debug) shows only the category. Neither ever
/// contains the executable, config/state paths, project id, task id or round
/// number. The underlying I/O error, when present, is reachable only through
/// [`Error::source`].
pub struct WorkerError {
    kind: WorkerErrorKind,
    source: Option<Box<dyn Error + Send + Sync + 'static>>,
}

impl WorkerError {
    /// Creates an error of the given `kind` without a source.
    fn new(kind: WorkerErrorKind) -> Self {
        Self { kind, source: None }
    }

    /// Creates an error of the given `kind` with an internal diagnostic source.
    fn with_source(kind: WorkerErrorKind, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            source: Some(Box::new(source)),
        }
    }

    /// Returns the category of this error.
    #[must_use]
    pub const fn kind(&self) -> WorkerErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.as_str())
    }
}

impl fmt::Debug for WorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Error for WorkerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.source {
            Some(source) => Some(source.as_ref()),
            None => None,
        }
    }
}

/// Validated inputs of one worker launch.
///
/// Construction enforces the contract before anything is spawned: the
/// executable, the config path and the state root must be absolute and the
/// round number must be positive. The identifiers are already typed, so an
/// empty project id or malformed task id cannot be represented.
///
/// The config path and state root are required to be absolute on purpose: the
/// caller resolves them against its own working directory (the reference config
/// loader does this with `Path.absolute()`), while the child runs in the
/// workspace. Passing a relative path would make the child resolve it against a
/// different directory, breaking config resolution and state isolation.
#[derive(Clone)]
pub struct WorkerInvocation {
    executable: PathBuf,
    project_id: ProjectId,
    config_path: PathBuf,
    state_root: PathBuf,
    task_id: TaskId,
    round_number: u32,
}

impl WorkerInvocation {
    /// Builds and validates a worker invocation from explicit absolute inputs.
    ///
    /// `executable` is the absolute path to the Rust `agent-bridge` binary.
    /// Production callers should prefer [`WorkerInvocation::from_current_exe`];
    /// this constructor is the explicit absolute injection point for focused
    /// fixtures and future wiring.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerErrorKind::InvalidExecutable`] when `executable` is not
    /// absolute, [`WorkerErrorKind::InvalidConfigPath`] or
    /// [`WorkerErrorKind::InvalidStateRoot`] when the corresponding path is
    /// empty or not absolute and [`WorkerErrorKind::InvalidRound`] when
    /// `round_number` is zero. No error message contains an input value.
    pub fn new(
        executable: impl Into<PathBuf>,
        project_id: ProjectId,
        config_path: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
        task_id: TaskId,
        round_number: u32,
    ) -> Result<Self, WorkerError> {
        let executable = executable.into();
        if !executable.is_absolute() {
            return Err(WorkerError::new(WorkerErrorKind::InvalidExecutable));
        }
        let config_path = config_path.into();
        if !config_path.is_absolute() {
            return Err(WorkerError::new(WorkerErrorKind::InvalidConfigPath));
        }
        let state_root = state_root.into();
        if !state_root.is_absolute() {
            return Err(WorkerError::new(WorkerErrorKind::InvalidStateRoot));
        }
        if round_number == 0 {
            return Err(WorkerError::new(WorkerErrorKind::InvalidRound));
        }
        Ok(Self {
            executable,
            project_id,
            config_path,
            state_root,
            task_id,
            round_number,
        })
    }

    /// Builds a worker invocation from the currently running Rust binary.
    ///
    /// This is the production resolution path: the spawned worker is the same
    /// executable as the caller, never a PATH-resolved Python `agent-bridge`.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerErrorKind::CurrentExecutable`] when the current
    /// executable cannot be resolved, plus every validation error of
    /// [`WorkerInvocation::new`]. No error message contains a path.
    pub fn from_current_exe(
        project_id: ProjectId,
        config_path: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
        task_id: TaskId,
        round_number: u32,
    ) -> Result<Self, WorkerError> {
        let executable = env::current_exe()
            .map_err(|error| WorkerError::with_source(WorkerErrorKind::CurrentExecutable, error))?;
        Self::new(
            executable,
            project_id,
            config_path,
            state_root,
            task_id,
            round_number,
        )
    }

    /// Returns the absolute executable path.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Returns the project id.
    #[must_use]
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Returns the config path.
    #[must_use]
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Returns the state root.
    #[must_use]
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Returns the task id.
    #[must_use]
    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    /// Returns the one-based round number.
    #[must_use]
    pub const fn round_number(&self) -> u32 {
        self.round_number
    }

    /// Renders the exact reference argv, including the program as element zero.
    ///
    /// Elements are [`OsString`], so spaces and non-ASCII text survive
    /// untouched and are never re-parsed by a shell.
    #[must_use]
    pub fn argv(&self) -> Vec<OsString> {
        vec![
            self.executable.clone().into_os_string(),
            OsString::from("worker"),
            OsString::from("--project"),
            OsString::from(self.project_id.as_str()),
            OsString::from("--config"),
            self.config_path.clone().into_os_string(),
            OsString::from("--state-root"),
            self.state_root.clone().into_os_string(),
            OsString::from("--task"),
            OsString::from(self.task_id.to_string()),
            OsString::from("--round"),
            OsString::from(self.round_number.to_string()),
        ]
    }

    /// Builds the base [`Command`] carrying the exact argv.
    fn command(&self) -> Command {
        let mut argv = self.argv().into_iter();
        let program = argv.next().unwrap_or_default();
        let mut command = Command::new(program);
        command.args(argv);
        command
    }
}

impl fmt::Debug for WorkerInvocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerInvocation")
            .field("round_number", &self.round_number)
            .finish_non_exhaustive()
    }
}

/// A running detached worker process.
///
/// The handle exposes the child PID and the lifecycle operations the future
/// runtime manager needs. It deliberately does **not** wait or kill on drop:
/// returning from the caller or dropping the handle leaves the worker running,
/// exactly like the reference detached spawn.
pub struct SpawnedWorker {
    pid: u32,
    child: Box<dyn ChildWrapper>,
}

impl SpawnedWorker {
    /// Returns the child process id.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Checks whether the child has exited without blocking.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the check itself fails.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Waits for the child to exit and returns its status.
    ///
    /// This is explicit: ordinary return or drop never waits for a detached
    /// worker.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when waiting fails.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Kills the child (and, on Unix, its process group) and waits for it.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the kill fails.
    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}

impl fmt::Debug for SpawnedWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnedWorker")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// Starts a detached worker for `invocation` and returns its handle.
///
/// Before any filesystem side effect the state namespace is verified: the
/// invocation state root and project id must match `layout`, and `layout` must
/// already be a valid, initialized Rust-owned state as proven by the existing
/// [`RustStateLayout::open`] guard. A foreign, missing or mismatched state fails
/// closed and is never created, chmodded or logged to. Only then is the private
/// Rust-owned project state directory enforced with mode `0o700` on Unix, the
/// child's stdout and stderr are appended to the Rust-owned `worker.log`, the
/// working directory is `workspace`, stdin is `/dev/null` and on Unix the child
/// is placed in a new session and process group.
///
/// The caller is not blocked until the worker finishes.
///
/// # Errors
///
/// Returns [`WorkerErrorKind::InvalidWorkspace`] when `workspace` is not
/// absolute, [`WorkerErrorKind::InvalidStateRoot`] when the layout root is not
/// absolute, [`WorkerErrorKind::StateRootMismatch`] or
/// [`WorkerErrorKind::ProjectMismatch`] when `invocation` and `layout` disagree
/// on the state root or project, [`WorkerErrorKind::StateOwnership`] when the
/// layout is not a valid Rust-owned state, and the typed
/// [`WorkerErrorKind::StateDirectory`], [`WorkerErrorKind::LogFile`] and
/// [`WorkerErrorKind::Spawn`] categories for the corresponding failures. No
/// error message contains a path or identifier.
pub fn spawn_worker(
    invocation: &WorkerInvocation,
    layout: &RustStateLayout,
    workspace: &Path,
) -> Result<SpawnedWorker, WorkerError> {
    if !workspace.is_absolute() {
        return Err(WorkerError::new(WorkerErrorKind::InvalidWorkspace));
    }
    if !layout.state_root().is_absolute() {
        return Err(WorkerError::new(WorkerErrorKind::InvalidStateRoot));
    }
    if invocation.state_root() != layout.state_root() {
        return Err(WorkerError::new(WorkerErrorKind::StateRootMismatch));
    }
    if invocation.project_id() != layout.project_id() {
        return Err(WorkerError::new(WorkerErrorKind::ProjectMismatch));
    }

    let storage = layout
        .open()
        .map_err(|error| WorkerError::with_source(WorkerErrorKind::StateOwnership, error))?;
    drop(storage);

    create_private_dir(&layout.project_dir())
        .map_err(|error| WorkerError::with_source(WorkerErrorKind::StateDirectory, error))?;

    let log_path = layout.log(RuntimeLog::Worker);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| WorkerError::with_source(WorkerErrorKind::LogFile, error))?;
    let stdout = clone_log(&log)?;
    let stderr = clone_log(&log)?;
    drop(log);

    let mut command = invocation.command();
    command
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    let mut wrapped = CommandWrap::from(command);
    #[cfg(unix)]
    wrapped.wrap(ProcessSession);
    let child = wrapped
        .spawn()
        .map_err(|error| WorkerError::with_source(WorkerErrorKind::Spawn, error))?;
    let pid = child.id();
    Ok(SpawnedWorker { pid, child })
}

/// Clones the log handle for one child stream.
fn clone_log(log: &File) -> Result<File, WorkerError> {
    log.try_clone()
        .map_err(|error| WorkerError::with_source(WorkerErrorKind::LogFile, error))
}

/// Creates `path` if needed and restricts it to the owner on Unix.
#[cfg(unix)]
fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Creates `path` if needed on platforms without Unix directory modes.
#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}

#[cfg(test)]
mod tests {
    use super::{WorkerErrorKind, WorkerInvocation, spawn_worker};
    use bridge_domain::{ProjectId, TaskId};
    use bridge_storage::RustStateLayout;
    use std::path::PathBuf;
    use std::str::FromStr;

    const TASK_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn project() -> ProjectId {
        ProjectId::from_str("proj-1").expect("valid project id")
    }

    fn task() -> TaskId {
        TaskId::from_str(TASK_ID).expect("valid task id")
    }

    #[test]
    fn argv_matches_the_reference_order_exactly() {
        let invocation = WorkerInvocation::new(
            "/usr/local/bin/agent-bridge",
            project(),
            "/etc/agent-bridge/projects.toml",
            "/var/lib/agent-bridge-rs",
            task(),
            7,
        )
        .expect("valid invocation");

        let argv: Vec<String> = invocation
            .argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/agent-bridge",
                "worker",
                "--project",
                "proj-1",
                "--config",
                "/etc/agent-bridge/projects.toml",
                "--state-root",
                "/var/lib/agent-bridge-rs",
                "--task",
                TASK_ID,
                "--round",
                "7",
            ]
        );
    }

    #[test]
    fn argv_preserves_spaces_and_unicode_without_a_shell() {
        let invocation = WorkerInvocation::new(
            PathBuf::from("/opt/agent bridge/agent-bridge"),
            project(),
            "/etc/agent bridge/projects.toml",
            "/var/lib/agent bridge-rs",
            task(),
            2,
        )
        .expect("valid invocation");

        let argv = invocation.argv();
        assert_eq!(
            argv[0],
            std::ffi::OsString::from("/opt/agent bridge/agent-bridge")
        );
        assert_eq!(
            argv[5],
            std::ffi::OsString::from("/etc/agent bridge/projects.toml")
        );
        assert_eq!(
            argv[7],
            std::ffi::OsString::from("/var/lib/agent bridge-rs")
        );
        // A multi-byte project id survives byte-for-byte.
        let unicode_project = ProjectId::from_str("проект-1").expect("valid project id");
        let invocation = WorkerInvocation::new(
            "/opt/agent-bridge",
            unicode_project,
            "/etc/projects.toml",
            "/var/state",
            task(),
            1,
        )
        .expect("valid invocation");
        assert_eq!(invocation.argv()[3], std::ffi::OsString::from("проект-1"));
    }

    #[test]
    fn relative_executable_is_rejected() {
        let error = WorkerInvocation::new(
            "agent-bridge",
            project(),
            "/etc/projects.toml",
            "/var/state",
            task(),
            1,
        )
        .expect_err("a relative executable must be rejected");
        assert_eq!(error.kind(), WorkerErrorKind::InvalidExecutable);
        assert!(!format!("{error} {error:?}").contains("agent-bridge"));
    }

    #[test]
    fn zero_round_is_rejected() {
        let error = WorkerInvocation::new(
            "/opt/agent-bridge",
            project(),
            "/etc/projects.toml",
            "/var/state",
            task(),
            0,
        )
        .expect_err("round zero must be rejected");
        assert_eq!(error.kind(), WorkerErrorKind::InvalidRound);
    }

    #[test]
    fn empty_paths_are_rejected() {
        let config_error =
            WorkerInvocation::new("/opt/agent-bridge", project(), "", "/var/state", task(), 1)
                .expect_err("empty config path must be rejected");
        assert_eq!(config_error.kind(), WorkerErrorKind::InvalidConfigPath);

        let state_error = WorkerInvocation::new(
            "/opt/agent-bridge",
            project(),
            "/etc/projects.toml",
            "",
            task(),
            1,
        )
        .expect_err("empty state root must be rejected");
        assert_eq!(state_error.kind(), WorkerErrorKind::InvalidStateRoot);
    }

    #[test]
    fn relative_config_and_state_paths_are_rejected() {
        let config_error = WorkerInvocation::new(
            "/opt/agent-bridge",
            project(),
            "relative/projects.toml",
            "/var/state",
            task(),
            1,
        )
        .expect_err("a relative config path must be rejected");
        assert_eq!(config_error.kind(), WorkerErrorKind::InvalidConfigPath);
        assert!(!format!("{config_error} {config_error:?}").contains("relative"));

        let state_error = WorkerInvocation::new(
            "/opt/agent-bridge",
            project(),
            "/etc/projects.toml",
            "relative/state",
            task(),
            1,
        )
        .expect_err("a relative state root must be rejected");
        assert_eq!(state_error.kind(), WorkerErrorKind::InvalidStateRoot);
        assert!(!format!("{state_error} {state_error:?}").contains("relative"));
    }

    #[test]
    fn project_mismatch_is_rejected_before_spawning() {
        let root = std::env::temp_dir().join("bridge-worker-unit-project-mismatch");
        let layout = RustStateLayout::new(root.join("state"), project()).expect("valid layout");
        let other_project = ProjectId::from_str("proj-2").expect("valid project id");
        let invocation = WorkerInvocation::new(
            "/opt/agent-bridge",
            other_project,
            "/etc/projects.toml",
            root.join("state"),
            task(),
            1,
        )
        .expect("valid invocation");
        let error = spawn_worker(&invocation, &layout, PathBuf::from("/tmp").as_path())
            .expect_err("a mismatched project must be rejected");
        assert_eq!(error.kind(), WorkerErrorKind::ProjectMismatch);
        assert!(!format!("{error} {error:?}").contains("proj-2"));
        assert!(!root.join("state").join("proj-2").exists());
    }

    #[test]
    fn current_exe_resolves_to_an_absolute_program() {
        let invocation = WorkerInvocation::from_current_exe(
            project(),
            "/etc/projects.toml",
            "/var/state",
            task(),
            1,
        )
        .expect("current executable must resolve");
        let argv = invocation.argv();
        assert!(PathBuf::from(&argv[0]).is_absolute());
        assert_eq!(argv[1], std::ffi::OsString::from("worker"));
    }

    #[test]
    fn state_root_mismatch_is_rejected_before_spawning() {
        let root = std::env::temp_dir().join("bridge-worker-unit-mismatch");
        let layout = RustStateLayout::new(root.join("state-a"), project()).expect("valid layout");
        let invocation = WorkerInvocation::new(
            "/opt/agent-bridge",
            project(),
            "/etc/projects.toml",
            root.join("state-b"),
            task(),
            1,
        )
        .expect("valid invocation");
        let error = spawn_worker(&invocation, &layout, PathBuf::from("/tmp").as_path())
            .expect_err("a mismatched state root must be rejected");
        assert_eq!(error.kind(), WorkerErrorKind::StateRootMismatch);
    }

    #[test]
    fn relative_workspace_is_rejected_before_spawning() {
        let root = std::env::temp_dir().join("bridge-worker-unit-workspace");
        let layout = RustStateLayout::new(root.join("state"), project()).expect("valid layout");
        let invocation = WorkerInvocation::new(
            "/opt/agent-bridge",
            project(),
            "/etc/projects.toml",
            root.join("state"),
            task(),
            1,
        )
        .expect("valid invocation");
        let error = spawn_worker(&invocation, &layout, PathBuf::from("relative").as_path())
            .expect_err("a relative workspace must be rejected");
        assert_eq!(error.kind(), WorkerErrorKind::InvalidWorkspace);
    }
}

pub mod workflow;

pub mod manual_status;
