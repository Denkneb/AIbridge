//! Explicit B1 activation and worker fencing. Opening state never activates tasks.
use crate::{WorkerLock, WorkerLockOutcome};
use bridge_config::ProjectEntry;
use bridge_domain::{ExecutionMode, TaskId, TaskStatus, WorkflowMetadata};
use bridge_storage::{AdmissionSettings, RustStateLayout, StorageConnection, Task};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::{error::Error, fmt, path::Path};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdmissionError {
    Ownership,
    Binding,
    Storage,
    Lock,
    Metadata,
    Baseline,
}
impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ownership => "admission state ownership failed",
            Self::Binding => "admission task binding failed",
            Self::Storage => "admission storage failed",
            Self::Lock => "admission lock failed",
            Self::Metadata => "admission metadata is invalid",
            Self::Baseline => "admission baseline unavailable",
        })
    }
}
impl Error for AdmissionError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationOutcome {
    Activated,
    Unchanged,
    Busy,
    Waiting,
    Dirty,
    OutsideScope,
    CloseRequested,
}

/// Lifetime fence for a single writer, including the per-task guard.
#[derive(Debug)]
pub struct WorkerFences {
    _project: Option<WorkerLock>,
    _task: WorkerLock,
}
/// Locks a saved writer without activating waiting tasks. None means busy/no work.
/// # Errors
/// Ownership, binding, persisted mode and lock failures refuse before execution.
pub fn acquire_worker_fences(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task_id: TaskId,
) -> Result<Option<WorkerFences>, AdmissionError> {
    let _admission =
        match WorkerLock::try_acquire_admission(layout).map_err(|_| AdmissionError::Lock)? {
            WorkerLockOutcome::Busy => return Ok(None),
            WorkerLockOutcome::Acquired(guard) => guard,
        };
    let mut storage = layout.open().map_err(|_| AdmissionError::Ownership)?;
    let task = bound_task(&storage, layout, project, task_id)?;
    if task.status.is_terminal()
        || task.status == TaskStatus::WaitingDependencies
        || task.close_requested_at.is_some()
    {
        return Ok(None);
    }
    // Read the saved flag before taking the lifetime fences. Missing rows use
    // conservative single-writer fencing, then admission repairs the ledger.
    let saved = storage
        .get_active_writers(project.id())
        .map_err(|_| AdmissionError::Metadata)?;
    let parallel = saved
        .iter()
        .find(|r| r.task_id == task_id)
        .is_some_and(|r| r.parallel);
    let guards = acquire_lifecycle_fences(layout, project, task_id, parallel)?;
    let Some(guards) = guards else {
        return Ok(None);
    };
    let mode = task_mode(&storage, task_id)?;
    let settings = AdmissionSettings::new(
        project.max_active_tasks(),
        project.allow_parallel_writers() && mode == ExecutionMode::Worktree,
        mode,
    )
    .map_err(|_| AdmissionError::Metadata)?;
    match storage.admit_saved_writer(task_id, project.id(), &settings) {
        Ok(_) => {}
        Err(
            bridge_storage::WriterError::ProjectBusy | bridge_storage::WriterError::ScopeOverlap,
        ) => return Ok(None),
        Err(_) => return Err(AdmissionError::Metadata),
    }
    Ok(Some(guards))
}

/// Used for close/recovery too, including terminal tasks; never activates tasks.
/// Public review fence. It neither activates nor repairs writer reservations.
pub fn try_review_fences(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task_id: TaskId,
) -> Result<Option<WorkerFences>, AdmissionError> {
    let _admission =
        match WorkerLock::try_acquire_admission(layout).map_err(|_| AdmissionError::Lock)? {
            WorkerLockOutcome::Busy => return Ok(None),
            WorkerLockOutcome::Acquired(guard) => guard,
        };
    let storage = layout.open().map_err(|_| AdmissionError::Ownership)?;
    bound_task(&storage, layout, project, task_id)?;
    let parallel = storage
        .get_active_writers(project.id())
        .map_err(|_| AdmissionError::Metadata)?
        .iter()
        .find(|r| r.task_id == task_id)
        .is_some_and(|r| r.parallel);
    acquire_lifecycle_fences(layout, project, task_id, parallel)
}

/// Used for close/recovery too, including terminal tasks; never activates tasks.
/// Caller must hold admission lock until this returns.
pub(crate) fn acquire_lifecycle_fences(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task_id: TaskId,
    parallel: bool,
) -> Result<Option<WorkerFences>, AdmissionError> {
    let storage = layout.open().map_err(|_| AdmissionError::Ownership)?;
    bound_task(&storage, layout, project, task_id)?;
    let project_guard = match WorkerLock::try_acquire(layout).map_err(|_| AdmissionError::Lock)? {
        WorkerLockOutcome::Busy => return Ok(None),
        WorkerLockOutcome::Acquired(guard) => guard,
    };
    if !parallel {
        // A parallel executor does not hold worker.lock. Probe every persisted
        // task fence, including writers whose ledger row was lost, before
        // letting a single writer take over after a config downgrade.
        let mut statement=storage.connection().prepare("SELECT task_id FROM tasks WHERE project_id=?1 AND task_id!=?2 UNION SELECT task_id FROM active_writers WHERE project_id=?1 AND task_id!=?2")
            .map_err(|_|AdmissionError::Storage)?;
        let rows = statement
            .query_map(
                rusqlite::params![project.id().as_str(), task_id.to_string()],
                |r| r.get::<_, String>(0),
            )
            .map_err(|_| AdmissionError::Storage)?;
        for raw in rows {
            let id = raw
                .map_err(|_| AdmissionError::Storage)?
                .parse()
                .map_err(|_| AdmissionError::Metadata)?;
            match WorkerLock::try_acquire_task(layout, id).map_err(|_| AdmissionError::Lock)? {
                WorkerLockOutcome::Busy => return Ok(None),
                WorkerLockOutcome::Acquired(guard) => drop(guard),
            }
        }
    }
    let task_guard =
        match WorkerLock::try_acquire_task(layout, task_id).map_err(|_| AdmissionError::Lock)? {
            WorkerLockOutcome::Busy => return Ok(None),
            WorkerLockOutcome::Acquired(guard) => guard,
        };
    let project_guard = if parallel {
        drop(project_guard);
        None
    } else {
        Some(project_guard)
    };
    Ok(Some(WorkerFences {
        _project: project_guard,
        _task: task_guard,
    }))
}
fn bound_task(
    storage: &StorageConnection,
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: TaskId,
) -> Result<Task, AdmissionError> {
    if layout.project_id() != project.id() {
        return Err(AdmissionError::Binding);
    }
    let task = storage
        .get_task(task)
        .map_err(|_| AdmissionError::Storage)?
        .ok_or(AdmissionError::Binding)?;
    if &task.project_id != project.id() || Path::new(&task.workspace) != project.workspace() {
        return Err(AdmissionError::Binding);
    }
    Ok(task)
}
fn task_mode(storage: &StorageConnection, task: TaskId) -> Result<ExecutionMode, AdmissionError> {
    let raw: String = storage
        .connection()
        .query_row(
            "SELECT execution_mode FROM tasks WHERE task_id=?1",
            [task.to_string()],
            |r| r.get(0),
        )
        .map_err(|_| AdmissionError::Storage)?;
    serde_json::from_value(json!(raw)).map_err(|_| AdmissionError::Metadata)
}
fn dependencies_ready(storage: &StorageConnection, task: &Task) -> Result<bool, AdmissionError> {
    let (workflow, raw): (Option<String>, Option<String>) = storage
        .connection()
        .query_row(
            "SELECT workflow_id,depends_on FROM tasks WHERE task_id=?1",
            [task.task_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| AdmissionError::Storage)?;
    let edges: Value = raw
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|_| AdmissionError::Metadata)?
        .unwrap_or_else(|| json!([]));
    if !edges
        .as_array()
        .is_some_and(|edges| edges.iter().all(Value::is_object))
    {
        return Err(AdmissionError::Metadata);
    }
    let metadata: WorkflowMetadata =
        serde_json::from_value(json!({"workflow_id":workflow,"depends_on":edges}))
            .map_err(|_| AdmissionError::Metadata)?;
    if !metadata.depends_on.is_empty() && metadata.workflow_id.is_none() {
        return Ok(false);
    }
    for edge in metadata.depends_on {
        let project = edge.project_id.as_str();
        let dependency = edge.task_id.as_str();
        // Linked-project resolution is wired by workflow service 8.14. Refuse
        // unresolved external edges; never initialize another project's state.
        if project != task.project_id.as_str() || dependency == task.task_id.to_string() {
            return Ok(false);
        }
        let resolved: Option<(String, Option<String>)> = storage
            .connection()
            .query_row(
                "SELECT status,workflow_id FROM tasks WHERE project_id=?1 AND task_id=?2",
                rusqlite::params![project, dependency],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|_| AdmissionError::Storage)?;
        if !resolved.is_some_and(|(status, wf)| status == "accepted" && wf == workflow) {
            return Ok(false);
        }
    }
    Ok(true)
}
fn baseline(
    project: &ProjectEntry,
    task: &Task,
) -> Result<Result<Option<Value>, ActivationOutcome>, AdmissionError> {
    if !task
        .snapshot
        .as_ref()
        .is_some_and(|s| s.get("manifest").is_some())
    {
        return Ok(Ok(None));
    }
    let paths = task
        .allowed_paths
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let roots = project
        .auto_approve_external_directories()
        .iter()
        .map(|p| p.as_path())
        .collect::<Vec<_>>();
    bridge_path_policy::validate_allowed_paths_with_trusted_roots(
        project.workspace(),
        &roots,
        &paths,
    )
    .map_err(|_| AdmissionError::Baseline)?;
    let groups = bridge_path_policy::group_allowed_paths_by_repo(project.workspace(), &paths)
        .map_err(|_| AdmissionError::Baseline)?;
    let snapshots = bridge_git::take_multi_repository_snapshot(project.workspace(), &groups)
        .map_err(|_| AdmissionError::Baseline)?;
    let allow_dirty = task
        .snapshot
        .as_ref()
        .is_some_and(|s| s["allow_dirty"] == true);
    let allow_commit = task
        .snapshot
        .as_ref()
        .is_some_and(|s| s["allow_commit"] == true);
    if !allow_dirty
        && snapshots
            .repositories()
            .iter()
            .any(|r| !r.snapshot().dirty_paths().is_empty())
    {
        return Ok(Err(ActivationOutcome::Dirty));
    }
    for repo in snapshots.repositories() {
        for dirty in repo.snapshot().dirty_paths() {
            let candidate = if repo.root() == project.workspace() {
                Path::new(dirty).to_owned()
            } else {
                repo.root().join(dirty)
            };
            let candidate = candidate.to_str().ok_or(AdmissionError::Baseline)?;
            if !repo.allowed_paths().iter().any(|scope| {
                scope == "**"
                    || candidate == scope
                    || (scope.ends_with('/') && candidate.starts_with(scope))
            }) {
                return Ok(Err(ActivationOutcome::OutsideScope));
            }
        }
    }
    let mut snapshot = snapshots
        .main()
        .snapshot()
        .to_json()
        .map_err(|_| AdmissionError::Baseline)?;
    let mut external = Vec::new();
    for repo in snapshots.repositories().iter().skip(1) {
        let mut value = repo
            .snapshot()
            .to_json()
            .map_err(|_| AdmissionError::Baseline)?;
        value["root"] = json!(repo.root().to_str().ok_or(AdmissionError::Baseline)?);
        value["allowed_paths"] = json!(repo.allowed_paths());
        external.push(value);
    }
    snapshot["external_repositories"] = json!(external);
    snapshot["allow_dirty"] = json!(allow_dirty);
    snapshot["allow_commit"] = json!(allow_commit);
    Ok(Ok(Some(snapshot)))
}
/// Explicit direct-mode dependency activation. Never starts a worker or sends HTTP.
/// # Errors
/// Corrupt metadata, invalid binding, snapshot or database errors fail closed.
pub fn activate_direct_task(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task_id: TaskId,
) -> Result<ActivationOutcome, AdmissionError> {
    activate_task(layout, project, task_id, true, &[])
}
/// Explicit B1/B2 activation using saved execution mode; worktree base is frozen.
/// # Errors
/// Uses the same strict dependency, ownership and admission guards as B1.
pub fn activate_waiting_task(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task_id: TaskId,
) -> Result<ActivationOutcome, AdmissionError> {
    activate_task(layout, project, task_id, false, &[])
}
/// Explicit activation resolving the configured linked projects read-only under admission.
pub fn activate_waiting_task_with_registry(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task_id: TaskId,
    registry: &[ProjectEntry],
) -> Result<ActivationOutcome, AdmissionError> {
    activate_task(layout, project, task_id, false, registry)
}
fn activate_task(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task_id: TaskId,
    direct_only: bool,
    registry: &[ProjectEntry],
) -> Result<ActivationOutcome, AdmissionError> {
    let _admission =
        match WorkerLock::try_acquire_admission(layout).map_err(|_| AdmissionError::Lock)? {
            WorkerLockOutcome::Busy => return Ok(ActivationOutcome::Busy),
            WorkerLockOutcome::Acquired(guard) => guard,
        };
    let mut storage = layout.open().map_err(|_| AdmissionError::Ownership)?;
    let task = bound_task(&storage, layout, project, task_id)?;
    if task.status != TaskStatus::WaitingDependencies {
        return Ok(ActivationOutcome::Unchanged);
    }
    if task.close_requested_at.is_some() {
        return Ok(ActivationOutcome::CloseRequested);
    }
    let mode = task_mode(&storage, task_id)?;
    if direct_only && mode != ExecutionMode::Direct {
        return Err(AdmissionError::Binding);
    }
    let parallel = mode == ExecutionMode::Worktree && project.allow_parallel_writers();
    let _fences = match acquire_lifecycle_fences(layout, project, task_id, parallel)? {
        Some(guards) => guards,
        None => return Ok(ActivationOutcome::Busy),
    };
    if !(if registry.is_empty() {
        dependencies_ready(&storage, &task)?
    } else {
        let metadata = crate::workflow::saved_metadata(layout, project, task_id)
            .map_err(|_| AdmissionError::Metadata)?;
        crate::workflow::gate(layout, project, registry, &metadata)["state"] == "ready"
    }) {
        return Ok(ActivationOutcome::Waiting);
    }
    let refreshed = match if mode == ExecutionMode::Direct {
        baseline(project, &task)?
    } else {
        Ok(None)
    } {
        Ok(value) => value,
        Err(outcome) => return Ok(outcome),
    };
    if let Some(snapshot) = refreshed {
        let head = snapshot["head"].as_str();
        if !storage
            .refresh_task_baseline(task_id, project.id(), &snapshot, head)
            .map_err(|_| AdmissionError::Storage)?
        {
            return Ok(ActivationOutcome::Unchanged);
        }
    }
    let settings = AdmissionSettings::new(project.max_active_tasks(), parallel, mode)
        .map_err(|_| AdmissionError::Metadata)?;
    if storage
        .activate_waiting_dependencies_with_admission(task_id, project.id(), &settings)
        .map_err(|_| AdmissionError::Storage)?
    {
        Ok(ActivationOutcome::Activated)
    } else {
        Ok(ActivationOutcome::Busy)
    }
}
