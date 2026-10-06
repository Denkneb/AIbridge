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
/// # Errors
/// Refuses foreign state, active tasks, retained service records or busy locks.
pub fn config_edit_guard(
    config: &bridge_config::Config,
    state: &Path,
) -> Result<ConfigEditGuard, RuntimeError> {
    use std::os::fd::AsRawFd;
    let mut layouts = vec![];
    for project in config.projects().values() {
        let layout = RustStateLayout::new(state.to_owned(), project.id().clone())
            .map_err(|_| RuntimeError::Binding)?;
        validate(project, &layout)?;
        if layout.database().exists() {
            layout
                .open_readonly()
                .map_err(|_| RuntimeError::Ownership)?;
            layouts.push(layout);
        } else if layout.marker().exists() {
            return Err(RuntimeError::Ownership);
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
        for name in ["admission.lock", "worker.lock", "controller.lock"] {
            let path = layout.project_dir().join(name);
            check_path(&path, false)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
                .open(path)
                .map_err(|_| RuntimeError::Io)?;
            nix::fcntl::flock(
                file.as_raw_fd(),
                nix::fcntl::FlockArg::LockExclusiveNonblock,
            )
            .map_err(|_| RuntimeError::LockBusy)?;
            files.push(file);
        }
        let storage = layout
            .open_readonly()
            .map_err(|_| RuntimeError::Ownership)?;
        if storage
            .count_tasks(layout.project_id(), true)
            .map_err(|_| RuntimeError::Ownership)?
            > 0
        {
            return Err(RuntimeError::LockBusy);
        }
        for kind in ["opencode", "mcp"] {
            if read_record(&layout.project_dir().join(format!("{kind}.process.json")))?.is_some() {
                return Err(RuntimeError::LockBusy);
            }
        }
    }
    Ok(ConfigEditGuard {
        _manager: manager,
        _files: files,
    })
}
