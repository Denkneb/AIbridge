//! Explicit observation recovery: prove saved endpoint, claim, spawn, release.
use crate::{
    WorkerLock, WorkerLockOutcome,
    execution::{execution_root, mode},
    recovery_startup::task_worker_running,
};
use bridge_config::{Endpoint, ProjectEntry};
use bridge_domain::{ExecutionMode, RoundStatus, TaskId, TaskStatus};
use bridge_opencode::OpenCodeClient;
use bridge_storage::{RoundRef, RoundRow, RustStateLayout, StorageConnection, Task};
use std::{error::Error, fmt, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryError {
    Ownership,
    Binding,
    Round,
    Storage,
    Lock,
}
impl fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ownership => "recovery state unowned",
            Self::Binding => "recovery binding invalid",
            Self::Round => "recovery round invalid",
            Self::Storage => "recovery storage failed",
            Self::Lock => "recovery lock failed",
        })
    }
}
impl Error for RecoveryError {}
#[derive(PartialEq, Eq)]
pub enum RecoverySpawnOutcome<T> {
    Spawned(T),
    Unchanged,
    Busy,
    Blocked,
    Unavailable,
    SpawnFailed,
}
impl<T> fmt::Debug for RecoverySpawnOutcome<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Spawned(_) => "Spawned { .. }",
            Self::Unchanged => "Unchanged",
            Self::Busy => "Busy",
            Self::Blocked => "Blocked",
            Self::Unavailable => "Unavailable",
            Self::SpawnFailed => "SpawnFailed",
        })
    }
}
fn saved_task(
    storage: &StorageConnection,
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: TaskId,
) -> Result<Task, RecoveryError> {
    if layout.project_id() != project.id() {
        return Err(RecoveryError::Binding);
    }
    let task = storage
        .get_task(id)
        .map_err(|_| RecoveryError::Storage)?
        .ok_or(RecoveryError::Binding)?;
    if &task.project_id != project.id()
        || std::path::Path::new(&task.workspace) != project.workspace()
    {
        return Err(RecoveryError::Binding);
    }
    Ok(task)
}
fn saved_round(storage: &StorageConnection, task: &Task) -> Result<RoundRow, RecoveryError> {
    let row = storage
        .connection()
        .query_row(
            "SELECT * FROM rounds WHERE task_id=?1 ORDER BY round_number DESC LIMIT 1",
            [task.task_id.to_string()],
            |r| Ok(RoundRow::from_row(r)),
        )
        .map_err(|_| RecoveryError::Round)?
        .map_err(|_| RecoveryError::Round)?;
    if row.project_id != task.project_id || row.task_id != task.task_id {
        return Err(RecoveryError::Binding);
    }
    Ok(row)
}
pub(crate) fn saved_view(
    storage: &StorageConnection,
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: &Task,
) -> Result<ProjectEntry, RecoveryError> {
    if mode(storage, task).map_err(|_| RecoveryError::Binding)? == ExecutionMode::Direct {
        return Ok(project.clone());
    }
    let root = execution_root(storage, layout, task, false).map_err(|_| RecoveryError::Binding)?;
    let record = storage
        .get_worktree(task.task_id, &task.project_id)
        .map_err(|_| RecoveryError::Storage)?
        .ok_or(RecoveryError::Binding)?;
    let endpoint = Endpoint::loopback(record.server_port.ok_or(RecoveryError::Binding)?.get())
        .map_err(|_| RecoveryError::Binding)?;
    if record.server_endpoint.as_deref() != Some(endpoint.url().as_str()) {
        return Err(RecoveryError::Binding);
    }
    project
        .execution_view(&root, endpoint)
        .map_err(|_| RecoveryError::Binding)
}
/// The background caller keeps the permission/question gate. Explicit recovery
/// may observe a stale blocker after endpoint identity is proved. No prompt,
/// permission reply, question answer or new session is ever issued here.
/// # Errors
/// Ownership, corrupt binding/round, lock and atomic persistence errors fail closed.
pub fn recover_needs_user<T, E>(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: TaskId,
    explicit: bool,
    layouts: &[&RustStateLayout],
    timeout: Duration,
    spawn: impl FnOnce(&RoundRef) -> Result<T, E>,
) -> Result<RecoverySpawnOutcome<T>, RecoveryError> {
    if task_worker_running(layout, id).map_err(|_| RecoveryError::Lock)? {
        return Ok(RecoverySpawnOutcome::Busy);
    }
    let mut storage = layout.open().map_err(|_| RecoveryError::Ownership)?;
    let task = saved_task(&storage, layout, project, id)?;
    if task.status != TaskStatus::NeedsUser || task.close_requested_at.is_some() {
        return Ok(RecoverySpawnOutcome::Unchanged);
    }
    match crate::lifecycle::recover_worktree_task(layout, project, id, layouts)
        .map_err(|_| RecoveryError::Binding)?
    {
        crate::lifecycle::RecoveryOutcome::Direct | crate::lifecycle::RecoveryOutcome::Retained => {
        }
        crate::lifecycle::RecoveryOutcome::Deferred => return Ok(RecoverySpawnOutcome::Busy),
        _ => return Ok(RecoverySpawnOutcome::Unchanged),
    }
    let row = saved_round(&storage, &task)?;
    if !row.status.is_open() {
        return Ok(RecoverySpawnOutcome::Unchanged);
    }
    if row.status != RoundStatus::Pending && row.session_id.is_none() {
        return Err(RecoveryError::Round);
    }
    let view = saved_view(&storage, layout, project, &task)?;
    let client =
        OpenCodeClient::from_project(&view, timeout).map_err(|_| RecoveryError::Binding)?;
    if !client.health().is_ok_and(|h| h.healthy()) || client.verify_workspace().is_err() {
        return Ok(RecoverySpawnOutcome::Unavailable);
    }
    if !explicit {
        let session = row.session_id.as_ref().or(task.session_id.as_ref());
        let Some(session) = session else {
            return Ok(RecoverySpawnOutcome::Blocked);
        };
        let permissions = client
            .list_permissions()
            .map_err(|_| RecoveryError::Binding)?;
        let questions = client
            .list_questions()
            .map_err(|_| RecoveryError::Binding)?;
        if permissions.iter().any(|p| p.belongs_to_session(session))
            || questions.iter().any(|q| q.belongs_to_session(session))
        {
            return Ok(RecoverySpawnOutcome::Blocked);
        }
    }
    // No admission lock spans HTTP. Revalidate the round, session and endpoint
    // under the short fence before deciding whether this request may claim it.
    let admission =
        match WorkerLock::try_acquire_admission(layout).map_err(|_| RecoveryError::Lock)? {
            WorkerLockOutcome::Busy => return Ok(RecoverySpawnOutcome::Busy),
            WorkerLockOutcome::Acquired(guard) => guard,
        };
    if task_worker_running(layout, id).map_err(|_| RecoveryError::Lock)? {
        return Ok(RecoverySpawnOutcome::Busy);
    }
    let current = saved_task(&storage, layout, project, id)?;
    if current.status != TaskStatus::NeedsUser || current.close_requested_at.is_some() {
        return Ok(RecoverySpawnOutcome::Unchanged);
    }
    let current_round = saved_round(&storage, &current)?;
    if current_round.round_number != row.round_number
        || current_round.session_id != row.session_id
        || current_round.status != row.status
    {
        return Ok(RecoverySpawnOutcome::Unchanged);
    }
    let fresh_view = saved_view(&storage, layout, project, &current)?;
    if fresh_view.workspace() != view.workspace()
        || fresh_view.opencode_endpoint() != view.opencode_endpoint()
    {
        return Err(RecoveryError::Binding);
    }
    let claim = storage
        .claim_needs_user_recovery(id, project.id())
        .map_err(|_| RecoveryError::Storage)?;
    let Some(claim) = claim else {
        return Ok(RecoverySpawnOutcome::Unchanged);
    };
    drop(admission);
    match spawn(claim.reference()) {
        Ok(worker) => Ok(RecoverySpawnOutcome::Spawned(worker)),
        Err(_) => {
            storage
                .release_needs_user_recovery(&claim)
                .map_err(|_| RecoveryError::Storage)?;
            Ok(RecoverySpawnOutcome::SpawnFailed)
        }
    }
}
