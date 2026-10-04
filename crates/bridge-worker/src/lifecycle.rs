//! Worktree close/recovery gate and logical orphan registry. No orphan deletion.
use crate::{
    WorkerLock, WorkerLockOutcome,
    execution::{ExecutionError, mode},
};
use bridge_config::ProjectEntry;
use bridge_domain::{ExecutionMode, TaskId, TaskStatus, WorkflowId};
use bridge_git::checkout::{CheckoutPaths, registrations, remove_checkout};
use bridge_storage::{
    RustStateLayout, StorageConnection, Task, WorktreeQuarantineRegistration, WorktreeStatus,
};
use std::{fs, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    Direct,
    Ready,
    Retained,
    Blocked,
    Deferred,
    Closed,
    MissingCheckout,
}
impl RecoveryOutcome {
    pub fn may_spawn(self) -> bool {
        matches!(self, Self::Direct | Self::Ready)
    }
}
fn owned_task(
    storage: &StorageConnection,
    project: &ProjectEntry,
    id: TaskId,
) -> Result<Task, ExecutionError> {
    let task = storage
        .get_task(id)
        .map_err(|_| ExecutionError::Storage)?
        .ok_or(ExecutionError::Binding)?;
    if &task.project_id != project.id() || Path::new(&task.workspace) != project.workspace() {
        return Err(ExecutionError::Binding);
    }
    Ok(task)
}
fn absent(path: &Path) -> Result<bool, ExecutionError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(_) => Err(ExecutionError::Binding),
    }
}
// Caller holds the lifecycle lock. Error rows are retained for diagnosis.
fn cleanup(
    storage: &mut StorageConnection,
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: &Task,
    layouts: &[&RustStateLayout],
) -> Result<bool, ExecutionError> {
    let record = storage
        .get_worktree(task.task_id, &task.project_id)
        .map_err(|_| ExecutionError::Storage)?
        .ok_or(ExecutionError::MissingRecord)?;
    match record.status {
        WorktreeStatus::Removed => return Ok(true),
        WorktreeStatus::Error => return Ok(false),
        WorktreeStatus::Pending => {
            // Submit never created a directory: terminalize only the logical row.
            storage
                .update_worktree_status(
                    task.task_id,
                    &task.project_id,
                    WorktreeStatus::Removed,
                    Some("closed"),
                )
                .map_err(|_| ExecutionError::Storage)?;
            return Ok(true);
        }
        WorktreeStatus::Creating | WorktreeStatus::Created => {
            storage
                .update_worktree_status(
                    task.task_id,
                    &task.project_id,
                    WorktreeStatus::Removing,
                    Some("closed"),
                )
                .map_err(|_| ExecutionError::Storage)?;
        }
        WorktreeStatus::Removing => {}
    }
    let result = (|| {
        let paths = CheckoutPaths::new(&layout.project_dir(), task.task_id)
            .map_err(|_| ExecutionError::Binding)?;
        paths
            .require_checkout(Path::new(&record.path))
            .map_err(|_| ExecutionError::Binding)?;
        if record.runtime_dir.as_deref() != paths.runtime_dir.to_str() {
            return Err(ExecutionError::Binding);
        }
        if absent(&paths.checkout)? {
            // Crash after checkout removal: no process record may survive. Never
            // signal a process when the Git binding can no longer be proven.
            if !absent(&paths.runtime_dir.join("opencode.process.json"))? {
                return Err(ExecutionError::Runtime);
            }
        } else {
            bridge_runtime::stop_worktree_server(layout, project, task.task_id, layouts)
                .map_err(|_| ExecutionError::Runtime)?;
        }
        remove_checkout(
            project.workspace(),
            &layout.project_dir(),
            task.task_id,
            &paths.checkout,
        )
        .map_err(|_| ExecutionError::Git)?;
        // Only the deterministic owned task directory, after registered checkout
        // removal. remove_dir_all does not traverse descendant symlinks.
        if !absent(&paths.task_dir)? {
            fs::remove_dir_all(&paths.task_dir).map_err(|_| ExecutionError::Binding)?;
        }
        storage
            .update_worktree_status(
                task.task_id,
                &task.project_id,
                WorktreeStatus::Removed,
                Some("closed"),
            )
            .map_err(|_| ExecutionError::Storage)?;
        Ok::<_, ExecutionError>(())
    })();
    if result.is_err() {
        storage
            .record_worktree_cleanup_deferred(task.task_id, &task.project_id)
            .map_err(|_| ExecutionError::Storage)?;
        return Ok(false);
    }
    Ok(true)
}

/// Persist a close marker, then recover under the nonblocking project worker lock.
/// # Errors
/// Refuses foreign state/task; cleanup failure retains its marker and row.
pub fn close_worktree_task(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: TaskId,
    reason: &str,
    layouts: &[&RustStateLayout],
) -> Result<RecoveryOutcome, ExecutionError> {
    let mut storage = layout.open().map_err(|_| ExecutionError::Ownership)?;
    if layout.project_id() != project.id() {
        return Err(ExecutionError::Binding);
    }
    let saved = owned_task(&storage, project, task)?;
    if mode(&storage, &saved)? != ExecutionMode::Worktree {
        return Err(ExecutionError::Binding);
    }
    storage
        .request_task_close(task, reason)
        .map_err(|_| ExecutionError::Storage)?;
    drop(storage);
    recover_worktree_task(layout, project, task, layouts)
}

/// Gate before worker admission. Never creates a checkout or starts a server.
/// Review, failed, needs-user and accepted results retain their checkout.
/// # Errors
/// Refuses malformed metadata and records without an owned task/Git binding.
pub fn recover_worktree_task(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: TaskId,
    layouts: &[&RustStateLayout],
) -> Result<RecoveryOutcome, ExecutionError> {
    let mut storage = layout.open().map_err(|_| ExecutionError::Ownership)?;
    if layout.project_id() != project.id() {
        return Err(ExecutionError::Binding);
    }
    let task = owned_task(&storage, project, id)?;
    if mode(&storage, &task)? == ExecutionMode::Direct {
        return Ok(RecoveryOutcome::Direct);
    }
    let admission =
        match WorkerLock::try_acquire_admission(layout).map_err(|_| ExecutionError::Ownership)? {
            WorkerLockOutcome::Busy => return Ok(RecoveryOutcome::Deferred),
            WorkerLockOutcome::Acquired(guard) => guard,
        };
    let parallel = storage
        .get_active_writers(project.id())
        .map_err(|_| ExecutionError::Storage)?
        .iter()
        .find(|r| r.task_id == id)
        .is_some_and(|r| r.parallel);
    let _guard = match crate::admission::acquire_lifecycle_fences(layout, project, id, parallel)
        .map_err(|_| ExecutionError::Ownership)?
    {
        Some(guard) => guard,
        None => return Ok(RecoveryOutcome::Deferred),
    };
    drop(admission);
    // Reread after acquisition: close/revision may have been persisted meanwhile.
    let task = owned_task(&storage, project, id)?;
    let record = storage
        .get_worktree(id, &task.project_id)
        .map_err(|_| ExecutionError::Storage)?
        .ok_or(ExecutionError::MissingRecord)?;
    if task.status == TaskStatus::Closed && record.status == WorktreeStatus::Removed {
        return Ok(RecoveryOutcome::Blocked);
    }
    if record.status == WorktreeStatus::Removing || task.close_requested_at.is_some() {
        if !cleanup(&mut storage, layout, project, &task, layouts)? {
            return Ok(RecoveryOutcome::Deferred);
        }
        if task.close_requested_at.is_some() {
            storage
                .complete_requested_close(id)
                .map_err(|_| ExecutionError::Storage)?;
            return Ok(RecoveryOutcome::Closed);
        }
        return Ok(RecoveryOutcome::Blocked);
    }
    if matches!(
        record.status,
        WorktreeStatus::Removed | WorktreeStatus::Error
    ) {
        return Ok(RecoveryOutcome::Blocked);
    }
    if record.status == WorktreeStatus::Created && !task.status.is_terminal() {
        let paths =
            CheckoutPaths::new(&layout.project_dir(), id).map_err(|_| ExecutionError::Binding)?;
        paths
            .require_checkout(Path::new(&record.path))
            .map_err(|_| ExecutionError::Binding)?;
        if absent(&paths.checkout)? {
            // Ordinary transition guards remain authoritative. Only an active
            // implement/revise round can fail; retained states stay retained.
            if matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising) {
                let number = storage
                    .connection()
                    .query_row(
                        "SELECT MAX(round_number) FROM rounds WHERE task_id=?1",
                        [id.to_string()],
                        |r| r.get(0),
                    )
                    .map_err(|_| ExecutionError::Storage)?;
                storage
                    .fail_worktree_missing(bridge_storage::RoundRef {
                        task_id: id,
                        project_id: task.project_id.clone(),
                        round_number: number,
                    })
                    .map_err(|_| ExecutionError::Storage)?;
            }
            return Ok(RecoveryOutcome::MissingCheckout);
        }
        // An existing directory is insufficient: require the same saved
        // task slot and two-way Git administrative binding before admission.
        crate::execution::execution_root(&storage, layout, &task, false)?;
    }
    if !matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising) {
        return Ok(RecoveryOutcome::Retained);
    }
    Ok(RecoveryOutcome::Ready)
}

/// Read-only filesystem/Git scan, writing only logical quarantine records.
/// Returns the number of newly registered entries; repeated scans are idempotent.
/// # Errors
/// Symlinked scan root, foreign state or conflicting registry metadata refuses.
pub fn quarantine_orphans(
    layout: &RustStateLayout,
    project: &ProjectEntry,
) -> Result<usize, ExecutionError> {
    let mut storage = layout.open().map_err(|_| ExecutionError::Ownership)?;
    if layout.project_id() != project.id() {
        return Err(ExecutionError::Binding);
    }
    let _guard = match WorkerLock::try_acquire(layout).map_err(|_| ExecutionError::Ownership)? {
        WorkerLockOutcome::Busy => return Err(ExecutionError::Round),
        WorkerLockOutcome::Acquired(guard) => guard,
    };
    let root = fs::canonicalize(layout.project_dir())
        .map_err(|_| ExecutionError::Binding)?
        .join("worktrees");
    if absent(&root)? {
        return Ok(0);
    }
    let meta = fs::symlink_metadata(&root).map_err(|_| ExecutionError::Binding)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(ExecutionError::Binding);
    }
    let registered = registrations(project.workspace()).ok();
    let mut entries = fs::read_dir(&root)
        .map_err(|_| ExecutionError::Binding)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ExecutionError::Binding)?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut count = 0;
    for entry in entries {
        let meta = entry.file_type().map_err(|_| ExecutionError::Binding)?;
        if meta.is_symlink() || !meta.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(token) = WorkflowId::try_from(name.clone()) else {
            continue;
        };
        let path = entry.path();
        let Some(original) = path.to_str() else {
            continue;
        };
        let matches: bool = storage.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM worktrees JOIN tasks USING(task_id) WHERE worktrees.task_id=?1 AND tasks.project_id=?2 AND worktrees.path=?3)",
            rusqlite::params![name, project.id().as_str(), path.join("checkout").to_str()], |r| r.get(0),
        ).map_err(|_| ExecutionError::Storage)?;
        if matches {
            continue;
        }
        if let Some(old) = storage
            .get_worktree_quarantine(&token)
            .map_err(|_| ExecutionError::Storage)?
        {
            if old.original_path != original {
                return Err(ExecutionError::Binding);
            }
            continue;
        }
        let checkout = path.join("checkout");
        let checkout_ok = fs::symlink_metadata(&checkout)
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink());
        let reason = if name.parse::<TaskId>().is_err() {
            "invalid_task_id"
        } else if !checkout_ok {
            "missing_checkout"
        } else if let Some(registered) = &registered {
            if registered.contains(&checkout) {
                "orphan_registered"
            } else {
                "unregistered_checkout"
            }
        } else {
            "registration_unknown"
        };
        storage
            .register_worktree_quarantine(
                &token,
                original,
                &WorktreeQuarantineRegistration {
                    reason: Some(reason.into()),
                    ..Default::default()
                },
            )
            .map_err(|_| ExecutionError::Storage)?;
        count += 1;
    }
    Ok(count)
}

/// Visits every task during startup recovery, including waiting/retained/terminal
/// checkout records. It never activates dependencies or spawns workers.
/// # Errors
/// Ownership, invalid identifiers and ordinary recovery failures refuse.
pub fn recover_project_worktrees(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    layouts: &[&RustStateLayout],
) -> Result<Vec<(TaskId, RecoveryOutcome)>, ExecutionError> {
    let storage = layout.open().map_err(|_| ExecutionError::Ownership)?;
    if layout.project_id() != project.id() {
        return Err(ExecutionError::Binding);
    }
    let mut statement = storage
        .connection()
        .prepare("SELECT task_id FROM tasks WHERE project_id=?1 ORDER BY created_at,task_id")
        .map_err(|_| ExecutionError::Storage)?;
    let ids = statement
        .query_map([project.id().as_str()], |r| r.get::<_, String>(0))
        .map_err(|_| ExecutionError::Storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ExecutionError::Storage)?;
    let mut outcomes = Vec::new();
    for raw in ids {
        let id = raw.parse().map_err(|_| ExecutionError::Binding)?;
        outcomes.push((id, recover_worktree_task(layout, project, id, layouts)?));
    }
    Ok(outcomes)
}
