//! Task-scoped OpenCode runtime with pidfd ownership and bounded manager locking.
//! Production uses `opencode serve`; fixtures may inject a trusted executable.
//! All service artifacts live outside the proven checkout. No MCP/CLI startup
//! is performed by this library; callers hold the task lifecycle/worker fence.
pub mod lock;
mod process;
use bridge_config::{Endpoint, ProjectEntry};
use bridge_domain::{TaskId, TaskStatus};
use bridge_git::checkout::{CheckoutPaths, probe_checkout};
use bridge_opencode::OpenCodeClient;
use bridge_storage::{RustStateLayout, WorktreeStatus};
use process::{ProcessRecord, identity, read_record, stop_record, write_record};
use process_wrap::std::{ChildWrapper, CommandWrap, ProcessSession};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fmt,
    fs::{self, OpenOptions},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeError {
    Ownership,
    Binding,
    Record,
    ForeignProcess,
    LockBusy,
    LockTimeout,
    NoPort,
    Credentials,
    Spawn,
    Readiness,
    Io,
    Unsupported,
}
impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ownership => "runtime_state_unowned",
            Self::Binding => "runtime_binding_mismatch",
            Self::Record => "runtime_record_corrupt",
            Self::ForeignProcess => "runtime_foreign_process",
            Self::LockBusy => "runtime_manager_busy",
            Self::LockTimeout => "runtime_manager_timeout",
            Self::NoPort => "worktree_no_port",
            Self::Credentials => "runtime_credentials_unavailable",
            Self::Spawn => "runtime_spawn_failed",
            Self::Readiness => "worktree_server_unavailable",
            Self::Io => "runtime_io_error",
            Self::Unsupported => "pidfd_unsupported",
        })
    }
}
impl std::error::Error for RuntimeError {}
/// Public timing policy. Ordinary stop keeps zero lock wait; startup defaults 60s.
#[derive(Clone, Copy)]
pub struct RuntimeOptions {
    pub lock_wait: Duration,
    pub ready_timeout: Duration,
    pub request_timeout: Duration,
}
impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            lock_wait: Duration::from_secs(60),
            ready_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(2),
        }
    }
}
/// Trusted executable selection; request/task text is never interpreted as argv.
pub struct ServerCommand {
    program: OsString,
    args: Vec<OsString>,
}
impl ServerCommand {
    pub fn opencode() -> Self {
        Self {
            program: "opencode".into(),
            args: Vec::new(),
        }
    }
    /// # Errors
    /// Fixture/integration executables must be absolute files.
    pub fn executable(path: &Path, args: Vec<OsString>) -> Result<Self, RuntimeError> {
        if !path.is_absolute() || !path.is_file() {
            return Err(RuntimeError::Spawn);
        }
        Ok(Self {
            program: path.as_os_str().into(),
            args,
        })
    }
}
/// Safe diagnostic states. Inspect never rewrites or removes records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Missing,
    Stale,
    Live,
}
pub struct WorktreeServer {
    pub client: OpenCodeClient,
    pub port: u16,
    pub reused: bool,
}
impl fmt::Debug for WorktreeServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorktreeServer { .. }")
    }
}

fn binding(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: TaskId,
) -> Result<CheckoutPaths, RuntimeError> {
    if layout.project_id() != project.id() {
        return Err(RuntimeError::Binding);
    }
    let storage = layout.open().map_err(|_| RuntimeError::Ownership)?;
    let saved = storage
        .get_task(task)
        .map_err(|_| RuntimeError::Binding)?
        .ok_or(RuntimeError::Binding)?;
    if saved.project_id != *project.id() || Path::new(&saved.workspace) != project.workspace() {
        return Err(RuntimeError::Binding);
    }
    let record = storage
        .get_worktree(task, project.id())
        .map_err(|_| RuntimeError::Binding)?
        .ok_or(RuntimeError::Binding)?;
    if !matches!(
        record.status,
        WorktreeStatus::Created | WorktreeStatus::Removing
    ) {
        return Err(RuntimeError::Binding);
    }
    let paths =
        CheckoutPaths::new(&layout.project_dir(), task).map_err(|_| RuntimeError::Binding)?;
    paths
        .require_checkout(Path::new(&record.path))
        .map_err(|_| RuntimeError::Binding)?;
    if record.runtime_dir.as_deref() != paths.runtime_dir.to_str() {
        return Err(RuntimeError::Binding);
    }
    // Cleanup may follow executor commits; repository/root/admin must still agree.
    probe_checkout(
        project.workspace(),
        &layout.project_dir(),
        task,
        &paths.checkout,
        None,
    )
    .map_err(|_| RuntimeError::Binding)?;
    Ok(paths)
}
fn path_record(paths: &CheckoutPaths) -> PathBuf {
    paths.runtime_dir.join("opencode.process.json")
}
fn require_record(
    record: &ProcessRecord,
    project: &ProjectEntry,
    task: TaskId,
    paths: &CheckoutPaths,
) -> Result<(), RuntimeError> {
    if record.project_id != *project.id()
        || record.task_id != task
        || record.checkout != paths.checkout
        || record.kind != "worktree"
        || !(43000..44000).contains(&record.port)
        || record.endpoint != format!("http://127.0.0.1:{}", record.port)
    {
        return Err(RuntimeError::ForeignProcess);
    }
    Ok(())
}
fn active_record(
    paths: &CheckoutPaths,
    project: &ProjectEntry,
    task: TaskId,
) -> Result<Option<ProcessRecord>, RuntimeError> {
    let file = path_record(paths);
    let Some(record) = read_record(&file)? else {
        return Ok(None);
    };
    if identity(record.pid).as_deref() != Some(record.start.as_str())
        || process::boot_id()?.as_str() != record.boot_id
    {
        fs::remove_file(&file).map_err(|_| RuntimeError::Io)?;
        return Ok(None);
    }
    require_record(&record, project, task, paths)?;
    Ok(Some(record))
}
fn client(
    project: &ProjectEntry,
    paths: &CheckoutPaths,
    port: u16,
    timeout: Duration,
    model_required: bool,
) -> Result<OpenCodeClient, RuntimeError> {
    let view = project
        .execution_view(
            &paths.checkout,
            Endpoint::loopback(port).map_err(|_| RuntimeError::Binding)?,
        )
        .map_err(|_| RuntimeError::Binding)?;
    OpenCodeClient::from_project(&view, timeout)
        .map(|c| c.with_require_prompt_model(model_required))
        .map_err(|_| RuntimeError::Credentials)
}
fn ready(client: &OpenCodeClient) -> bool {
    client.health().is_ok_and(|h| h.healthy())
        && client.verify_workspace().is_ok()
        && client
            .check_compatibility()
            .is_ok_and(|d| d.is_compatible())
}
fn reserved_ports(
    layouts: &[&RustStateLayout],
    projects: &[&ProjectEntry],
) -> Result<BTreeSet<u16>, RuntimeError> {
    let mut ports = BTreeSet::new();
    for project in projects {
        ports.insert(project.opencode_endpoint().port());
        if let Some(m) = project.mcp_endpoint() {
            ports.insert(m.port());
        }
    }
    let roots = layouts
        .iter()
        .map(|l| fs::canonicalize(l.state_root()).map_err(|_| RuntimeError::Io))
        .collect::<Result<BTreeSet<_>, _>>()?;
    for root in roots {
        for project in fs::read_dir(root).map_err(|_| RuntimeError::Io)? {
            let project = project.map_err(|_| RuntimeError::Io)?;
            if !project.file_type().map_err(|_| RuntimeError::Io)?.is_dir() {
                continue;
            }
            let tree = project.path().join("worktrees");
            if fs::symlink_metadata(&tree).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(RuntimeError::Binding);
            }
            let entries = match fs::read_dir(tree) {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(RuntimeError::Io),
            };
            for task in entries {
                let task = task.map_err(|_| RuntimeError::Io)?;
                if !task.file_type().map_err(|_| RuntimeError::Io)?.is_dir() {
                    continue;
                }
                let runtime = task.path().join("runtime");
                if fs::symlink_metadata(&runtime).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Err(RuntimeError::Binding);
                }
                if let Some(r) = read_record(&runtime.join("opencode.process.json"))
                    .ok()
                    .flatten()
                    && identity(r.pid).as_deref() == Some(r.start.as_str())
                    && process::boot_id()? == r.boot_id
                {
                    ports.insert(r.port);
                }
            }
        }
    }
    Ok(ports)
}
struct SpawnGuard {
    child: Option<Box<dyn ChildWrapper>>,
}
impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
/// Starts/reuses a server only after storage and Git binding proofs. Startup
/// serializes allocation, record reservation and readiness under manager lock.
/// # Errors
/// Rejects foreign ownership, unsupported pidfd, occupied ports, or incompatible
/// HTTP root/doc/health. A failed fresh spawn is killed/reaped and unreserved.
#[allow(clippy::too_many_arguments)]
pub fn start_worktree_server(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: TaskId,
    layouts: &[&RustStateLayout],
    projects: &[&ProjectEntry],
    command: &ServerCommand,
    options: RuntimeOptions,
) -> Result<WorktreeServer, RuntimeError> {
    process::require_pidfd()?;
    binding(layout, project, task)?;
    let mut lock_layouts = layouts.to_vec();
    lock_layouts.push(layout);
    let _lock = lock::ManagerLock::acquire(&lock_layouts, options.lock_wait)?;
    let paths = binding(layout, project, task)?;
    let storage = layout.open().map_err(|_| RuntimeError::Ownership)?;
    let saved = storage
        .get_task(task)
        .map_err(|_| RuntimeError::Binding)?
        .ok_or(RuntimeError::Binding)?;
    if !matches!(
        saved.status,
        TaskStatus::Implementing | TaskStatus::Revising
    ) || saved.close_requested_at.is_some()
    {
        return Err(RuntimeError::Binding);
    }
    let wt = storage
        .get_worktree(task, project.id())
        .map_err(|_| RuntimeError::Binding)?
        .ok_or(RuntimeError::Binding)?;
    if wt.status != WorktreeStatus::Created {
        return Err(RuntimeError::Binding);
    }
    let profile = storage
        .get_task_profile(task, project.id())
        .map_err(|_| RuntimeError::Binding)?;
    let model_required = profile
        .as_ref()
        .map_or(project.opencode_model().is_some(), |p| p.model.is_some());
    if let Some(record) = active_record(&paths, project, task)? {
        let client = client(
            project,
            &paths,
            record.port,
            options.request_timeout,
            model_required,
        )?;
        if !ready(&client) {
            return Err(RuntimeError::Readiness);
        }
        return Ok(WorktreeServer {
            client,
            port: record.port,
            reused: true,
        });
    }
    let mut all_projects = projects.to_vec();
    all_projects.push(project);
    let reserved = reserved_ports(&lock_layouts, &all_projects)?;
    let port = (43000..44000)
        .find(|p| !reserved.contains(p) && TcpListener::bind(("127.0.0.1", *p)).is_ok())
        .ok_or(RuntimeError::NoPort)?;
    let client = client(
        project,
        &paths,
        port,
        options.request_timeout,
        model_required,
    )?;
    let env = project
        .read_opencode_env()
        .map_err(|_| RuntimeError::Credentials)?;
    let password = project
        .read_password()
        .map_err(|_| RuntimeError::Credentials)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&paths.runtime_dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| RuntimeError::Io)?;
    }
    let mut log = OpenOptions::new();
    log.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log.mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK);
    }
    let log = log
        .open(paths.runtime_dir.join("server.log"))
        .map_err(|_| RuntimeError::Io)?;
    if !log.metadata().map_err(|_| RuntimeError::Io)?.is_file() {
        return Err(RuntimeError::Io);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        log.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| RuntimeError::Io)?;
    }
    let mut spawn = Command::new(&command.program);
    spawn
        .args(&command.args)
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .current_dir(&paths.checkout)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|_| RuntimeError::Io)?))
        .stderr(Stdio::from(log));
    if let Some(env) = env {
        spawn.envs(env.iter());
    }
    spawn
        .env("OPENCODE_SERVER_USERNAME", "opencode")
        .env("OPENCODE_SERVER_PASSWORD", password.expose_secret());
    let mut wrap = CommandWrap::from(spawn);
    wrap.wrap(ProcessSession);
    let child = wrap.spawn().map_err(|_| RuntimeError::Spawn)?;
    let pid = i32::try_from(child.id()).map_err(|_| RuntimeError::Spawn)?;
    let mut guard = SpawnGuard { child: Some(child) };
    let record = ProcessRecord {
        pid,
        start: identity(pid).ok_or(RuntimeError::Spawn)?,
        boot_id: process::boot_id()?,
        project_id: project.id().clone(),
        task_id: task,
        checkout: paths.checkout.clone(),
        kind: "worktree".into(),
        port,
        endpoint: format!("http://127.0.0.1:{port}"),
    };
    let record_path = path_record(&paths);
    let result = (|| {
        write_record(&record_path, &record)?;
        let deadline = Instant::now()
            .checked_add(options.ready_timeout)
            .ok_or(RuntimeError::Readiness)?;
        loop {
            if guard
                .child
                .as_mut()
                .ok_or(RuntimeError::Spawn)?
                .try_wait()
                .map_err(|_| RuntimeError::Spawn)?
                .is_some()
            {
                return Err(RuntimeError::Readiness);
            }
            if ready(&client) {
                break;
            }
            if Instant::now() >= deadline {
                return Err(RuntimeError::Readiness);
            }
            std::thread::sleep(
                Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        Ok(())
    })();
    if let Err(error) = result {
        drop(guard);
        if read_record(&record_path).ok().flatten().as_ref() == Some(&record) {
            let _ = fs::remove_file(&record_path);
        }
        return Err(error);
    }
    let mut child = guard.child.take().ok_or(RuntimeError::Spawn)?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(WorktreeServer {
        client,
        port,
        reused: false,
    })
}
/// Read-only diagnosis with the same storage/Git/process proof.
/// # Errors
/// Corrupt/live foreign bindings are refused without signalling or rewriting.
pub fn worktree_server_state(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: TaskId,
) -> Result<ServerState, RuntimeError> {
    let paths = binding(layout, project, task)?;
    let Some(record) = read_record(&path_record(&paths))? else {
        return Ok(ServerState::Missing);
    };
    if identity(record.pid).as_deref() != Some(record.start.as_str())
        || process::boot_id()? != record.boot_id
    {
        return Ok(ServerState::Stale);
    }
    require_record(&record, project, task, &paths)?;
    Ok(ServerState::Live)
}
/// Idempotent stop. Ordinary operations never wait for the manager lock.
/// # Errors
/// Foreign/reused PID and malformed record never receive a signal. pidfd ensures
/// no PID race after opening the exact process; record stays if shutdown fails.
pub fn stop_worktree_server(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: TaskId,
    layouts: &[&RustStateLayout],
) -> Result<bool, RuntimeError> {
    binding(layout, project, task)?;
    let mut roots = layouts.to_vec();
    roots.push(layout);
    let _lock = lock::ManagerLock::acquire(&roots, Duration::ZERO)?;
    let paths = binding(layout, project, task)?;
    let Some(record) = active_record(&paths, project, task)? else {
        return Ok(false);
    };
    stop_record(&record)?;
    fs::remove_file(path_record(&paths)).map_err(|_| RuntimeError::Io)?;
    Ok(true)
}
