//! Detached supervisor ownership and spawn lease; model output never supplies argv.
use crate::{
    AutomationError as Error,
    codex::{private_dir, read_bounded},
    run::{AutomationLock, check_binding, check_config_binding},
};
use bridge_config::ProjectEntry;
use bridge_storage::{
    RustStateLayout,
    automation::{AutomationRunStore, RunControl, RunId, RunStatus},
};
use process_wrap::std::{ChildWrapper, CommandWrap, ProcessSession};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    pid: i32,
    start: String,
    boot_id: String,
    project_id: String,
    workspace: PathBuf,
    run_id: String,
    kind: String,
}
fn boot() -> Result<String, Error> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().into())
        .map_err(|_| Error::State)
}
fn identity(pid: i32) -> Option<String> {
    if pid <= 1 {
        return None;
    }
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = text.rsplit_once(") ")?;
    let fields = rest.split_whitespace().collect::<Vec<_>>();
    if matches!(*fields.first()?, "Z" | "X") {
        return None;
    }
    let start = *fields.get(19)?;
    start
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| start.into())
}
pub fn directory(layout: &RustStateLayout, id: RunId) -> PathBuf {
    layout.project_dir().join("automation").join(id.to_string())
}
/// Read-only liveness, never deletes stale or foreign records.
pub fn supervisor_running(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: RunId,
) -> Result<bool, Error> {
    layout.open_readonly().map_err(|_| Error::State)?;
    if layout.project_id() != project.id() {
        return Err(Error::Binding);
    }
    let path = directory(layout, id).join("process.json");
    for ancestor in [
        layout.project_dir().join("automation"),
        directory(layout, id),
    ] {
        if let Ok(meta) = fs::symlink_metadata(ancestor)
            && !meta.is_dir()
        {
            return Err(Error::State);
        }
    }
    if !fs::symlink_metadata(&path).is_ok() {
        return Ok(false);
    }
    let data = read_bounded(&path, 16384).map_err(|_| Error::State)?;
    let record: Record = serde_json::from_slice(&data).map_err(|_| Error::State)?;
    if record.pid <= 1
        || record.start.is_empty()
        || !record.start.bytes().all(|c| c.is_ascii_digit())
        || record.boot_id.is_empty()
    {
        return Err(Error::State);
    }
    if identity(record.pid).as_deref() != Some(&record.start) || boot()? != record.boot_id {
        return Ok(false);
    }
    if record.project_id != project.id().as_str()
        || record.workspace != project.workspace()
        || record.run_id != id.to_string()
        || record.kind != "automation"
    {
        return Err(Error::Binding);
    }
    Ok(true)
}
/// Called by the private automation-worker before coordinator work.
pub fn acquire_supervisor(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: RunId,
) -> Result<AutomationLock, Error> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let guard = loop {
        match AutomationLock::acquire(layout) {
            Ok(g) => break g,
            Err(Error::Busy) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(e),
        }
    };
    let path = directory(layout, id).join("process.json");
    let record: Record =
        serde_json::from_slice(&read_bounded(&path, 16384).map_err(|_| Error::State)?)
            .map_err(|_| Error::State)?;
    if record.pid != std::process::id() as i32 || !supervisor_running(layout, project, id)? {
        return Err(Error::Binding);
    }
    Ok(guard)
}
struct SpawnGuard(Option<Box<dyn ChildWrapper>>);
impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Creates a detached process with an exact argv. The parent holds automation
/// lock until a durable identity record is published; the child then takes it.
pub fn launch(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: RunId,
    executable: &Path,
    resume: bool,
) -> Result<Value, Error> {
    launch_command(layout, project, id, executable, Vec::new(), resume)
}
/// Fixture command selection is trusted embedding configuration, never plan data.
pub fn launch_command(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: RunId,
    executable: &Path,
    prefix: Vec<OsString>,
    resume: bool,
) -> Result<Value, Error> {
    launch_command_mode(
        layout,
        project,
        id,
        executable,
        prefix,
        if resume {
            LaunchMode::Resume
        } else {
            LaunchMode::Start
        },
    )
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    Start,
    Resume,
    Stop,
}
/// Stop starts cleanup without changing the durable stop control back to run.
pub fn launch_command_mode(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: RunId,
    executable: &Path,
    prefix: Vec<OsString>,
    mode: LaunchMode,
) -> Result<Value, Error> {
    if !executable.is_absolute() || !executable.is_file() {
        return Err(Error::State);
    }
    let _guard = AutomationLock::acquire(layout)?;
    let store = AutomationRunStore::new(layout.clone());
    let run = store.load(Some(id)).map_err(|_| Error::State)?;
    if run.status().is_terminal()
        || (mode == LaunchMode::Start
            && (run.status() != RunStatus::Running || run.control() != RunControl::Run))
        || (mode == LaunchMode::Stop && run.control() != RunControl::Stop)
    {
        return Err(Error::State);
    }
    if supervisor_running(layout, project, id)? {
        return Err(Error::Busy);
    }
    // Partial main delivery is checked by the journal, rather than clean-main
    // origin checks. All other phases require the original main binding.
    if mode == LaunchMode::Stop {
        // Cleanup uses bound task records even after an origin/config drift blocker.
        if layout.project_id() != project.id()
            || run.document()["binding"]["workspace"] != json!(project.workspace())
        {
            return Err(Error::Binding);
        }
    } else if run.document()["phase"] == "deliver" {
        check_config_binding(project, layout, &run)?;
    } else {
        check_binding(project, layout, &run)?;
    }
    let dir = directory(layout, id);
    private_dir(&dir).map_err(|_| Error::State)?;
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
        .open(dir.join("supervisor.log"))
        .map_err(|_| Error::State)?;
    if !log.metadata().map_err(|_| Error::State)?.is_file() {
        return Err(Error::State);
    }
    log.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| Error::State)?;
    let mut command = Command::new(executable);
    command
        .args(prefix)
        .arg("automation-worker")
        .arg("--project")
        .arg(project.id().as_str())
        .arg("--config")
        .arg(project.source_path())
        .arg("--state-root")
        .arg(layout.state_root())
        .arg("--run")
        .arg(id.to_string())
        .current_dir(project.workspace())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().map_err(|_| Error::State)?))
        .stderr(Stdio::from(log));
    let mut wrapped = CommandWrap::from(command);
    wrapped.wrap(ProcessSession);
    let mut child = SpawnGuard(Some(wrapped.spawn().map_err(|_| Error::State)?));
    let pid =
        i32::try_from(child.0.as_ref().ok_or(Error::State)?.id()).map_err(|_| Error::State)?;
    let record = Record {
        pid,
        start: identity(pid).ok_or(Error::State)?,
        boot_id: boot()?,
        project_id: project.id().as_str().into(),
        workspace: project.workspace().into(),
        run_id: id.to_string(),
        kind: "automation".into(),
    };
    bridge_artifact::artifact::atomic_write(
        &dir.join("process.json"),
        &serde_json::to_vec(&record).map_err(|_| Error::State)?,
        0o600,
    )
    .map_err(|_| Error::State)?;
    if mode == LaunchMode::Resume {
        // This happens only after successful spawn and record publication.
        store
            .set_control(id, RunControl::Run)
            .map_err(|_| Error::State)?;
        let mut document = run.document().clone();
        document["blocker"] = Value::Null;
        store
            .save(&document, RunStatus::Running)
            .map_err(|_| Error::State)?;
    }
    let mut process = child.0.take().ok_or(Error::State)?;
    std::thread::spawn(move || {
        let _ = process.wait();
    });
    Ok(json!({"run_id":id.to_string(),"pid":pid,"status":"starting"}))
}

/// Pause without deleting task worktrees, then terminate the owned supervisor tree.
pub fn shutdown(layout: &RustStateLayout, project: &ProjectEntry) -> Result<(), Error> {
    let store = AutomationRunStore::new(layout.clone());
    let run = match store.load(None) {
        Ok(run) => run,
        Err(bridge_storage::automation::AutomationStoreError::NotFound) => return Ok(()),
        Err(_) => return Err(Error::State),
    };
    if !run.status().is_terminal() && run.control() != RunControl::Stop {
        store
            .set_control(run.id(), RunControl::Pause)
            .map_err(|_| Error::State)?;
    }
    if supervisor_running(layout, project, run.id())? {
        let record: Record = serde_json::from_slice(
            &read_bounded(&directory(layout, run.id()).join("process.json"), 16384)
                .map_err(|_| Error::State)?,
        )
        .map_err(|_| Error::State)?;
        bridge_runtime::process_tree::ProcessTree::from_identity(
            record.pid,
            &record.start,
            &record.boot_id,
        )
        .and_then(|tree| tree.stop())
        .map_err(|_| Error::State)?;
    }
    let _guard = AutomationLock::acquire(layout)?;
    let run = store.load(Some(run.id())).map_err(|_| Error::State)?;
    if !run.status().is_terminal() && run.control() == RunControl::Pause {
        store
            .save(run.document(), RunStatus::Paused)
            .map_err(|_| Error::State)?;
    }
    Ok(())
}
