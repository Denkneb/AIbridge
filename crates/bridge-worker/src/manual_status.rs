use crate::{WorkerLock, WorkerLockOutcome};
use bridge_config::ProjectEntry;
use bridge_domain::{TaskId, TaskStatus};
use bridge_storage::RustStateLayout;

/// Explicit correction of a nonterminal task status. Does not send prompts,
/// create results, alter rounds, run tests, accept or close a task.
pub fn set_status(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: TaskId,
    expected: TaskStatus,
    target: TaskStatus,
    reason: &str,
) -> Result<(), &'static str> {
    bridge_runtime::project::validate(project, layout).map_err(|_| "task_binding_mismatch")?;
    if target.is_terminal() {
        return Err("terminal_status_forbidden");
    }
    if !bridge_config::suspected_secret_categories(reason).is_empty() {
        return Err("suspected_secret");
    }
    let _admission =
        match WorkerLock::try_acquire_admission(layout).map_err(|_| "state_unavailable")? {
            WorkerLockOutcome::Busy => return Err("task_busy"),
            WorkerLockOutcome::Acquired(guard) => guard,
        };
    let storage = layout.open_readonly().map_err(|_| "state_unavailable")?;
    let task = storage
        .get_task(id)
        .map_err(|_| "state_unavailable")?
        .ok_or("unknown_task")?;
    if task.project_id != *project.id()
        || std::path::Path::new(&task.workspace) != project.workspace()
    {
        return Err("task_binding_mismatch");
    }
    let _fences = crate::admission::try_review_fences_while_admitted(layout, project, id)
        .map_err(|_| "state_unavailable")?
        .ok_or("task_busy")?;
    layout
        .open()
        .map_err(|_| "state_unavailable")?
        .set_task_status_manual(id, project.id(), expected, target, reason)
}
