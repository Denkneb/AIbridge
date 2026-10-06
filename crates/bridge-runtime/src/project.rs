//! Explicit main OpenCode/MCP lifecycle. Only proven new processes are rolled back.
use crate::{
    RuntimeError, ServerCommand, SpawnGuard,
    lock::ManagerLock,
    process::{self, ProcessRecord, identity, read_record, stop_record, write_record},
    readiness,
};
use bridge_config::ProjectEntry;
use bridge_storage::RustStateLayout;
use process_wrap::std::{CommandWrap, ProcessSession};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
const MAIN: &str = "00000000-0000-0000-0000-000000000000";
pub fn validate(project: &ProjectEntry, layout: &RustStateLayout) -> Result<(), RuntimeError> {
    let root = layout.state_root();
    if layout.project_id() != project.id()
        || root.starts_with(project.workspace())
        || project.workspace().starts_with(root)
    {
        return Err(RuntimeError::Binding);
    }
    check_path(&layout.project_dir(), true)?;
    check_path(&layout.database(), false)?;
    check_path(&layout.marker(), false)?;
    Ok(())
}
fn check_path(path: &Path, directory: bool) -> Result<(), RuntimeError> {
    let mut prefix = std::path::PathBuf::new();
    let count = path.components().count();
    for (i, c) in path.components().enumerate() {
        if matches!(c, Component::ParentDir) {
            return Err(RuntimeError::Binding);
        }
        prefix.push(c);
        match fs::symlink_metadata(&prefix) {
            Ok(m)
                if m.file_type().is_symlink()
                    || (i + 1 < count || directory) && !m.is_dir()
                    || i + 1 == count && !directory && !m.is_file() =>
            {
                return Err(RuntimeError::Binding);
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(RuntimeError::Io),
        }
    }
    Ok(())
}
fn credential_preflight(project: &ProjectEntry) -> Result<(), RuntimeError> {
    for path in project
        .password_file()
        .into_iter()
        .chain(project.mcp_token_file())
    {
        let path = path.as_path();
        if path.starts_with(project.workspace()) {
            return Err(RuntimeError::Binding);
        }
        check_path(path, false)?;
        if path.exists() {
            path_credential(project, path)?;
        }
    }
    project
        .read_opencode_env()
        .map_err(|_| RuntimeError::Credentials)?;
    Ok(())
}
fn path_credential(project: &ProjectEntry, path: &Path) -> Result<(), RuntimeError> {
    if project.password_file().is_some_and(|p| p.as_path() == path) {
        project
            .read_password()
            .map_err(|_| RuntimeError::Credentials)?;
    } else {
        project
            .read_mcp_token()
            .map_err(|_| RuntimeError::Credentials)?;
    }
    Ok(())
}
pub fn setup(projects: &[(ProjectEntry, RustStateLayout)]) -> Result<Value, RuntimeError> {
    for (p, l) in projects {
        validate(p, l)?;
        credential_preflight(p)?;
        if l.project_dir().exists() {
            l.open_readonly().map_err(|_| RuntimeError::Ownership)?;
        }
    }
    for (p, l) in projects {
        let password = p.password_file().ok_or(RuntimeError::Credentials)?;
        for path in std::iter::once(password).chain(p.mcp_token_file()) {
            let path = path.as_path();
            if path.exists() {
                continue;
            }
            let parent = path.parent().ok_or(RuntimeError::Binding)?;
            fs::create_dir_all(parent).map_err(|_| RuntimeError::Io)?;
            check_path(parent, true)?;
            let mut bytes = [0; 32];
            fs::File::open("/dev/urandom")
                .and_then(|mut f| f.read_exact(&mut bytes))
                .map_err(|_| RuntimeError::Io)?;
            let secret = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            let mut f = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
                .open(path)
            {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    path_credential(p, path)?;
                    continue;
                }
                Err(_) => return Err(RuntimeError::Io),
            };
            f.write_all(format!("{secret}\n").as_bytes())
                .and_then(|_| f.sync_all())
                .map_err(|_| RuntimeError::Io)?;
            fs::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|_| RuntimeError::Io)?;
        }
        l.initialize().map_err(|_| RuntimeError::Ownership)?;
        fs::set_permissions(l.project_dir(), fs::Permissions::from_mode(0o700))
            .map_err(|_| RuntimeError::Io)?;
    }
    Ok(json!({"status":"ready","projects":projects.iter().map(|(p,_)|p.id()).collect::<Vec<_>>()}))
}
fn ready(project: &ProjectEntry, kind: &str) -> bool {
    if kind == "opencode" {
        readiness::opencode(project, Duration::from_millis(300))
    } else {
        readiness::mcp(project, Duration::from_millis(300))
    }
}
fn port(project: &ProjectEntry, kind: &str) -> Result<u16, RuntimeError> {
    if kind == "opencode" {
        Ok(project.opencode_endpoint().port())
    } else {
        project
            .mcp_endpoint()
            .map(|e| e.port())
            .ok_or(RuntimeError::Binding)
    }
}
fn stop_one(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    kind: &str,
) -> Result<bool, RuntimeError> {
    let path = layout.project_dir().join(format!("{kind}.process.json"));
    let Some(record) = read_record(&path)? else {
        return Ok(false);
    };
    if identity(record.pid).as_deref() != Some(&record.start)
        || process::boot_id()? != record.boot_id
    {
        fs::remove_file(path).map_err(|_| RuntimeError::Io)?;
        return Ok(false);
    }
    readiness::require_main_record(&record, project, kind)?;
    stop_record(&record)?;
    fs::remove_file(path).map_err(|_| RuntimeError::Io)?;
    Ok(true)
}
fn spawn_one(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    kind: &str,
    opencode: &ServerCommand,
    bridge_exe: &Path,
    timeout: Duration,
) -> Result<ProcessRecord, RuntimeError> {
    let log_path = layout.project_dir().join(format!("{kind}.server.log"));
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
        .open(log_path)
        .map_err(|_| RuntimeError::Io)?;
    if !log.metadata().map_err(|_| RuntimeError::Io)?.is_file() {
        return Err(RuntimeError::Io);
    }
    log.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| RuntimeError::Io)?;
    let mut cmd = if kind == "opencode" {
        let mut cmd = Command::new(&opencode.program);
        cmd.args(&opencode.args).args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port(project, kind)?.to_string(),
        ]);
        if let Some(env) = project
            .read_opencode_env()
            .map_err(|_| RuntimeError::Credentials)?
        {
            cmd.envs(env.iter());
        }
        cmd.env("OPENCODE_SERVER_USERNAME", "opencode").env(
            "OPENCODE_SERVER_PASSWORD",
            project
                .read_password()
                .map_err(|_| RuntimeError::Credentials)?
                .expose_secret(),
        );
        cmd
    } else {
        let mut cmd = Command::new(bridge_exe);
        cmd.args(["serve-mcp", "--project", project.id().as_str(), "--config"])
            .arg(project.source_path())
            .arg("--state-root")
            .arg(layout.state_root());
        cmd.env_remove("OPENCODE_SERVER_PASSWORD")
            .env_remove("OPENCODE_SERVER_USERNAME");
        cmd
    };
    cmd.current_dir(project.workspace())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|_| RuntimeError::Io)?))
        .stderr(Stdio::from(log));
    let mut wrapped = CommandWrap::from(cmd);
    wrapped.wrap(ProcessSession);
    let child = wrapped.spawn().map_err(|_| RuntimeError::Spawn)?;
    let pid = i32::try_from(child.id()).map_err(|_| RuntimeError::Spawn)?;
    let mut guard = SpawnGuard { child: Some(child) };
    let record = ProcessRecord {
        pid,
        start: identity(pid).ok_or(RuntimeError::Spawn)?,
        boot_id: process::boot_id()?,
        project_id: project.id().clone(),
        task_id: MAIN.parse().map_err(|_| RuntimeError::Binding)?,
        checkout: project.workspace().into(),
        kind: kind.into(),
        port: port(project, kind)?,
        endpoint: format!("http://127.0.0.1:{}", port(project, kind)?),
    };
    let record_path = layout.project_dir().join(format!("{kind}.process.json"));
    write_record(&record_path, &record)?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(RuntimeError::ProjectReadiness)?;
    while !ready(project, kind) {
        if identity(pid).as_deref() != Some(record.start.as_str()) || Instant::now() >= deadline {
            let _ = fs::remove_file(&record_path);
            return Err(RuntimeError::ProjectReadiness);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if identity(pid).as_deref() != Some(record.start.as_str()) {
        let _ = fs::remove_file(record_path);
        return Err(RuntimeError::ProjectReadiness);
    }
    let mut child = guard.child.take().ok_or(RuntimeError::Spawn)?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(record)
}
pub fn start(
    projects: &[(ProjectEntry, RustStateLayout)],
    opencode: &ServerCommand,
    bridge_exe: &Path,
    timeout: Duration,
) -> Result<Value, RuntimeError> {
    if !bridge_exe.is_absolute() || !bridge_exe.is_file() || timeout.is_zero() {
        return Err(RuntimeError::Binding);
    }
    process::require_pidfd()?;
    for (p, l) in projects {
        validate(p, l)?;
        p.read_password().map_err(|_| RuntimeError::Credentials)?;
        p.read_opencode_env()
            .map_err(|_| RuntimeError::Credentials)?;
        if p.mcp_endpoint().is_none()
            || p.read_mcp_token()
                .map_err(|_| RuntimeError::Credentials)?
                .is_none()
        {
            return Err(RuntimeError::Credentials);
        }
        l.open_readonly().map_err(|_| RuntimeError::Ownership)?;
    }
    let layouts = projects.iter().map(|(_, l)| l).collect::<Vec<_>>();
    let _lock = ManagerLock::acquire(&layouts, Duration::ZERO)?;
    let mut started = vec![];
    let result = (|| {
        for (index, (p, l)) in projects.iter().enumerate() {
            for kind in ["opencode", "mcp"] {
                let state = readiness::record_state(l, p, kind)?;
                if ready(p, kind) {
                    continue;
                }
                if state == "live" || TcpListener::bind(("127.0.0.1", port(p, kind)?)).is_err() {
                    return Err(RuntimeError::ProjectReadiness);
                }
                let record = spawn_one(p, l, kind, opencode, bridge_exe, timeout)?;
                started.push((index, kind, record));
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut rollback_failed = false;
        for (index, kind, record) in started.iter().rev() {
            if stop_record(record).is_ok() {
                let _ = fs::remove_file(
                    projects[*index]
                        .1
                        .project_dir()
                        .join(format!("{kind}.process.json")),
                );
            } else {
                rollback_failed = true;
            }
        }
        return Err(if rollback_failed {
            RuntimeError::Io
        } else {
            error
        });
    }
    Ok(json!({"status":"ready","projects":projects.iter().map(|(p,_)|p.id()).collect::<Vec<_>>()}))
}
pub fn stop(projects: &[(ProjectEntry, RustStateLayout)]) -> Result<Value, RuntimeError> {
    let present = projects
        .iter()
        .filter(|(_, l)| l.project_dir().exists())
        .collect::<Vec<_>>();
    for (p, l) in &present {
        validate(p, l)?;
        l.open_readonly().map_err(|_| RuntimeError::Ownership)?;
    }
    if present.is_empty() {
        return Ok(json!({"status":"not_managed"}));
    }
    let layouts = present.iter().map(|(_, l)| l).collect::<Vec<_>>();
    let _lock = ManagerLock::acquire(&layouts, Duration::ZERO)?;
    let mut errors = false;
    let mut stopped = 0;
    for (p, l) in present.iter().rev() {
        for kind in ["mcp", "opencode"] {
            match stop_one(p, l, kind) {
                Ok(true) => stopped += 1,
                Ok(false) => {}
                Err(_) => errors = true,
            }
        }
    }
    if errors {
        return Err(RuntimeError::ForeignProcess);
    }
    Ok(json!({"status":"stopped","processes":stopped}))
}

/// Holds every manager/admission/controller/worker fence during shared config edits.
/// Existing owned namespaces must have no unfinished tasks or service records.
pub struct ConfigEditGuard {
    _manager: Option<ManagerLock>,
    _files: Vec<fs::File>,
}
/// A config-edit refusal with a project and a concrete, credential-free reason.
#[derive(Debug)]
pub struct ConfigEditError {
    project: Option<String>,
    kind: RuntimeError,
    detail: String,
}
impl ConfigEditError {
    fn runtime(project: Option<&str>, kind: RuntimeError) -> Self {
        let detail = match kind {
            RuntimeError::LockBusy | RuntimeError::LockTimeout => {
                "Выполняется операция управления сервисами. Повторите сохранение позже."
            }
            RuntimeError::Ownership => {
                "Rust state не прошёл проверку владения или схемы. Проверьте проект через Doctor."
            }
            RuntimeError::Binding => "Настройки проекта не соответствуют каталогу Rust state.",
            RuntimeError::Record => {
                "Запись сервиса повреждена или недоступна. Проверьте проект через Doctor."
            }
            RuntimeError::ForeignProcess => {
                "Запись работающего сервиса не соответствует проекту. Проверьте проект через Doctor."
            }
            RuntimeError::Io => {
                "Нет доступа к файлам Rust state или блокировок. Проверьте права доступа."
            }
            _ => {
                "Не удалось проверить возможность изменения конфигурации. Проверьте проект через Doctor."
            }
        };
        Self {
            project: project.map(str::to_owned),
            kind,
            detail: format!("{detail} ({kind})"),
        }
    }
    fn busy(project: &str, detail: impl Into<String>) -> Self {
        Self {
            project: Some(project.into()),
            kind: RuntimeError::LockBusy,
            detail: detail.into(),
        }
    }
}
impl From<RuntimeError> for ConfigEditError {
    fn from(kind: RuntimeError) -> Self {
        Self::runtime(None, kind)
    }
}
impl std::fmt::Display for ConfigEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(project) = &self.project {
            write!(f, "Проект {project}: ")?;
        }
        f.write_str(&self.detail)
    }
}
impl std::error::Error for ConfigEditError {}

/// # Errors
/// Refuses foreign state, active tasks, retained service records or busy locks.
pub fn config_edit_guard(
    config: &bridge_config::Config,
    state: &Path,
) -> Result<ConfigEditGuard, RuntimeError> {
    config_entries_edit_guard(&config.projects().values().collect::<Vec<_>>(), state)
        .map_err(|e| e.kind)
}

/// Fence the changed project and controllers that consume it in either config.
/// Newly registered workspaces can turn an existing trusted root into a link.
/// Projects sharing credential files also participate in the fence.
/// # Errors
/// Refuses activity or unsafe state in affected projects; independent projects
/// can keep running. The caller must separately serialize writes to the file.
pub fn project_config_edit_guard(
    existing: &bridge_config::Config,
    proposed: &bridge_config::Config,
    changed: &str,
    state: &Path,
) -> Result<ConfigEditGuard, ConfigEditError> {
    use std::collections::BTreeSet;
    if proposed.project(changed).is_none() {
        return Err(RuntimeError::Binding.into());
    }
    let mut affected = BTreeSet::new();
    for config in [existing, proposed] {
        for project in config.projects().values() {
            let shared_credentials = config.project(changed).is_some_and(|target| {
                let target_paths = [target.password_file(), target.mcp_token_file()];
                [project.password_file(), project.mcp_token_file()]
                    .into_iter()
                    .flatten()
                    .any(|path| {
                        target_paths
                            .into_iter()
                            .flatten()
                            .any(|other| path == other)
                    })
            });
            if project.id().as_str() == changed
                || shared_credentials
                || config
                    .linked_projects(project.id().as_str())
                    .iter()
                    .any(|linked| linked.id().as_str() == changed)
            {
                affected.insert(project.id().as_str());
            }
        }
    }
    let projects = affected
        .into_iter()
        .filter_map(|id| existing.project(id).or_else(|| proposed.project(id)))
        .collect::<Vec<_>>();
    config_entries_edit_guard(&projects, state)
}

fn config_entries_edit_guard(
    projects: &[&ProjectEntry],
    state: &Path,
) -> Result<ConfigEditGuard, ConfigEditError> {
    use std::os::fd::AsRawFd;
    let mut layouts = vec![];
    for project in projects {
        let prepare = || -> Result<Option<RustStateLayout>, RuntimeError> {
            let layout = RustStateLayout::new(state.to_owned(), project.id().clone())
                .map_err(|_| RuntimeError::Binding)?;
            validate(project, &layout)?;
            if layout.database().exists() {
                layout
                    .open_readonly()
                    .map_err(|_| RuntimeError::Ownership)?;
                Ok(Some(layout))
            } else if layout.marker().exists() {
                Err(RuntimeError::Ownership)
            } else {
                Ok(None)
            }
        };
        if let Some(layout) =
            prepare().map_err(|e| ConfigEditError::runtime(Some(project.id().as_str()), e))?
        {
            layouts.push(layout);
        }
    }
    let refs = layouts.iter().collect::<Vec<_>>();
    let manager = if refs.is_empty() {
        None
    } else {
        Some(ManagerLock::acquire(&refs, Duration::ZERO)?)
    };
    let mut files = vec![];
    for layout in &layouts {
        let id = layout.project_id().as_str();
        let runtime_error = |e| ConfigEditError::runtime(Some(id), e);
        for (name, detail) in [
            (
                "admission.lock",
                "Выполняется операция с задачами. Повторите сохранение позже.",
            ),
            (
                "worker.lock",
                "Работает worker. Остановите worker этого проекта.",
            ),
            (
                "controller.lock",
                "Открыт контроллер Codex/OpenCode. Завершите его вкладку терминала.",
            ),
        ] {
            let path = layout.project_dir().join(name);
            check_path(&path, false).map_err(runtime_error)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
                .open(path)
                .map_err(|_| runtime_error(RuntimeError::Io))?;
            nix::fcntl::flock(
                file.as_raw_fd(),
                nix::fcntl::FlockArg::LockExclusiveNonblock,
            )
            .map_err(|e| {
                if e == nix::errno::Errno::EWOULDBLOCK {
                    ConfigEditError::busy(id, detail)
                } else {
                    runtime_error(RuntimeError::Io)
                }
            })?;
            files.push(file);
        }
        let storage = layout
            .open_readonly()
            .map_err(|_| runtime_error(RuntimeError::Ownership))?;
        let count = storage
            .count_tasks(layout.project_id(), true)
            .map_err(|_| runtime_error(RuntimeError::Ownership))?;
        if count > 0 {
            return Err(ConfigEditError::busy(
                id,
                format!(
                    "Незавершённых задач: {count}. Завершите или закройте их перед сохранением."
                ),
            ));
        }
        let project = projects
            .iter()
            .find(|p| p.id() == layout.project_id())
            .ok_or_else(|| runtime_error(RuntimeError::Binding))?;
        for kind in ["opencode", "mcp"] {
            // Retained records of exited processes are not running services.
            if readiness::record_state(layout, project, kind).map_err(runtime_error)? == "live" {
                return Err(ConfigEditError::busy(
                    id,
                    format!("Работает сервис {kind}. Остановите сервисы этого проекта."),
                ));
            }
        }
    }
    Ok(ConfigEditGuard {
        _manager: manager,
        _files: files,
    })
}
