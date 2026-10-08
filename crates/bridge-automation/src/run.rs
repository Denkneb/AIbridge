use crate::{AutomationError as Error, plan::validate_plan};
use bridge_config::{ProjectEntry, load_config_with_state_root};
use bridge_storage::{
    RustStateLayout,
    automation::{AutomationRun, AutomationRunStore, AutomationStoreError},
};
use bridge_worker::{WorkerLock, WorkerLockOutcome};
use nix::fcntl::{FlockArg, flock};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const FINAL_TASK: &str = "Validate the combined implementation and fix integration defects inside the approved workflow scope. Preserve all accepted step criteria.";
pub(crate) const FINAL_CRITERIA: [&str; 2] = [
    "All approved step criteria remain satisfied",
    "Final integration checks pass",
];

/// Project automation fence, held before admission. Requires proven Rust state.
pub struct AutomationLock {
    _file: File,
}
impl AutomationLock {
    pub fn acquire(layout: &RustStateLayout) -> Result<Self, Error> {
        layout.open_readonly().map_err(|_| Error::State)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
            .open(layout.project_dir().join("automation.lock"))
            .map_err(|_| Error::State)?;
        if !file.metadata().map_err(|_| Error::State)?.is_file() {
            return Err(Error::State);
        }
        flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock).map_err(|_| Error::Busy)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| Error::State)?;
        Ok(Self { _file: file })
    }
}

fn binding(project: &ProjectEntry, layout: &RustStateLayout) -> Result<Value, Error> {
    if project.id() != layout.project_id() {
        return Err(Error::Binding);
    }
    let bytes = fs::read(project.source_path()).map_err(|_| Error::Binding)?;
    let config = load_config_with_state_root(project.source_path(), layout.state_root())
        .map_err(|_| Error::Binding)?;
    let current = config
        .project(project.id().as_str())
        .ok_or(Error::Binding)?;
    // A previously loaded config must still describe the current source file.
    if current.values() != project.values()
        || current.workspace() != project.workspace()
        || fs::read(project.source_path()).map_err(|_| Error::Binding)? != bytes
    {
        return Err(Error::Binding);
    }
    Ok(
        json!({"project":project.id().as_str(), "workspace":project.workspace(), "config_sha256":format!("{:x}", Sha256::digest(bytes))}),
    )
}
pub(crate) fn origin(project: &ProjectEntry) -> Result<Value, Error> {
    let snapshot = bridge_git::take_snapshot(project.workspace()).map_err(|_| Error::Repository)?;
    if !snapshot.status().is_empty() {
        return Err(Error::Repository);
    }
    let head = snapshot.head().ok_or(Error::Repository)?;
    let common = bridge_git::checkout::main_common_dir(project.workspace())
        .map_err(|_| Error::Repository)?;
    // A linked checkout has a .git file, and must never be used as the main repo.
    if !fs::symlink_metadata(project.workspace().join(".git")).is_ok_and(|m| m.is_dir())
        || fs::canonicalize(project.workspace().join(".git")).map_err(|_| Error::Repository)?
            != common
    {
        return Err(Error::Repository);
    }
    bridge_git::checkout::check_supported(project.workspace(), head.as_str())
        .map_err(|_| Error::Repository)?;
    Ok(json!({"repository":common, "snapshot":snapshot.to_json().map_err(|_| Error::Repository)?}))
}

/// Validates raw input before initializing state, then checks project/task/Git
/// prerequisites under automation → admission locks and inserts one durable run.
pub fn create_run(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    raw: &Value,
) -> Result<AutomationRun, Error> {
    create_run_with_id(
        project,
        layout,
        raw,
        uuid::Uuid::new_v4()
            .to_string()
            .parse()
            .map_err(|_| Error::State)?,
    )
}
/// Explicit identity allows an authenticated remote admission retry to find the
/// existing run instead of submitting the approved pool twice.
pub fn create_run_with_id(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    raw: &Value,
    id: bridge_storage::automation::RunId,
) -> Result<AutomationRun, Error> {
    create_bound_run(project, layout, raw, id, serde_json::Value::Null)
}
pub fn create_bound_run(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    raw: &Value,
    id: bridge_storage::automation::RunId,
    remote_executor: Value,
) -> Result<AutomationRun, Error> {
    validate_plan(project, raw)?;
    let bound = binding(project, layout)?;
    layout.initialize().map_err(|_| Error::State)?;
    let _automatic = AutomationLock::acquire(layout)?;
    let _admission = match WorkerLock::try_acquire_admission(layout).map_err(|_| Error::State)? {
        WorkerLockOutcome::Busy => return Err(Error::Busy),
        WorkerLockOutcome::Acquired(guard) => guard,
    };
    let plan = validate_plan(project, raw)?;
    if binding(project, layout)? != bound {
        return Err(Error::Binding);
    }
    if !layout
        .open_readonly()
        .map_err(|_| Error::State)?
        .list_tasks(project.id(), true, 1, 0)
        .map_err(|_| Error::State)?
        .is_empty()
    {
        return Err(Error::UnfinishedTasks);
    }
    if !remote_executor.is_null() {
        let store = AutomationRunStore::new(layout.clone());
        match store.load(None) {
            Ok(run) if !run.status().is_terminal() => return Err(Error::Busy),
            Ok(_) | Err(AutomationStoreError::NotFound) => {}
            Err(_) => return Err(Error::State),
        }
        crate::remote::synchronize_source(project, &remote_executor)
            .map_err(|_| Error::Repository)?;
    }
    let original = origin(project)?;
    let scopes = plan
        .steps()
        .iter()
        .flat_map(|s| s.allowed_paths.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    bridge_git::checkout::check_worktree_scopes(project.workspace(), &scopes)
        .map_err(|_| Error::Scope)?;
    let mut steps = plan
        .steps()
        .iter()
        .map(|s| json!({"step":s,"phase":"prepare","task_id":null,"revisions":0}))
        .collect::<Vec<_>>();
    steps.push(json!({"step":{
        "id":"__final__", "task":FINAL_TASK,
        "allowed_paths":scopes,"test_commands":plan.final_test_commands(),
        "acceptance_criteria":FINAL_CRITERIA,"profile":null
    },"phase":"prepare","task_id":null,"revisions":0}));
    let document = json!({"run_id":id.to_string(),"plan":plan,"binding":bound,"origin":original,
        "started":SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| Error::State)?.as_secs_f64(),
        "remote_executor":remote_executor,"elapsed":0.0,"steps":steps,"index":0,"phase":"steps","blocker":null});
    // Repeat external binding proofs immediately before the durable write.
    if binding(project, layout)? != document["binding"] || origin(project)? != document["origin"] {
        return Err(Error::Binding);
    }
    AutomationRunStore::new(layout.clone())
        .create(&document)
        .map_err(|e| match e {
            AutomationStoreError::UnfinishedRun => Error::Busy,
            _ => Error::State,
        })
}

/// Read-only prerequisite for future coordinator steps. Never edits the run or
/// repository. Callers hold automation/admission fences around dependent writes.
pub fn check_binding(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    run: &AutomationRun,
) -> Result<(), Error> {
    check_config_binding(project, layout, run)?;
    if origin(project).map_err(|_| Error::Binding)? != run.document()["origin"] {
        return Err(Error::Binding);
    }
    Ok(())
}

/// Delivery resumes check config binding while main files may already be mixed.
pub fn check_config_binding(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    run: &AutomationRun,
) -> Result<(), Error> {
    layout.open_readonly().map_err(|_| Error::State)?;
    if binding(project, layout)? != run.document()["binding"] {
        return Err(Error::Binding);
    }
    Ok(())
}
