//! Seeds a fresh checkout only from a pinned, independently accepted parent.
use crate::execution::ExecutionError as Error;
use bridge_artifact::{artifact, build_entries, fingerprint, load_artifact, materialize};
use bridge_domain::{TaskId, TaskStatus};
use bridge_git::{RepositorySnapshot, checkout::probe_checkout};
use bridge_storage::{RustStateLayout, StorageConnection, Task, WorktreeStatus};
use std::path::Path;

/// Caller holds task lifecycle/worker fences and the target is freshly created.
/// A partial failure never publishes a baseline; retry recreates the checkout.
pub fn inherit_checkout(
    storage: &StorageConnection,
    layout: &RustStateLayout,
    task: &Task,
    checkout: &Path,
) -> Result<(), Error> {
    let snapshot = task.snapshot.as_ref().ok_or(Error::Baseline)?;
    let parent_id: TaskId = snapshot["automation_parent"]
        .as_str()
        .ok_or(Error::Binding)?
        .parse()
        .map_err(|_| Error::Binding)?;
    let parent = storage
        .get_task(parent_id)
        .map_err(|_| Error::Storage)?
        .ok_or(Error::Binding)?;
    let expected = snapshot
        .get("automation_parent_fingerprint")
        .filter(|v| v.is_object())
        .ok_or(Error::Binding)?;
    let workflow = |id: TaskId| {
        storage
            .connection()
            .query_row(
                "SELECT workflow_id FROM tasks WHERE task_id=?1 AND project_id=?2",
                rusqlite::params![id.to_string(), task.project_id.as_str()],
                |r| r.get::<_, Option<String>>(0),
            )
            .map_err(|_| Error::Binding)
    };
    let parent_workflow = workflow(parent_id)?;
    if parent.status != TaskStatus::Accepted
        || parent.project_id != task.project_id
        || parent.workspace != task.workspace
        || parent.base_head != task.base_head
        || parent_workflow.is_none()
        || parent_workflow != workflow(task.task_id)?
        || layout.project_id() != &task.project_id
    {
        return Err(Error::Binding);
    }
    let record = storage
        .get_worktree(parent_id, &task.project_id)
        .map_err(|_| Error::Storage)?
        .ok_or(Error::MissingRecord)?;
    if record.status != WorktreeStatus::Created || record.base_head != parent.base_head {
        return Err(Error::Binding);
    }
    let base = task.base_head.as_deref().ok_or(Error::MissingBase)?;
    let source = probe_checkout(
        Path::new(&task.workspace),
        &layout.project_dir(),
        parent_id,
        Path::new(&record.path),
        Some(base),
    )
    .map_err(|_| Error::Binding)?;
    let target = probe_checkout(
        Path::new(&task.workspace),
        &layout.project_dir(),
        task.task_id,
        checkout,
        Some(base),
    )
    .map_err(|_| Error::Binding)?;
    if source.common_dir != target.common_dir
        || record.runtime_dir.as_deref() != source.paths.runtime_dir.to_str()
    {
        return Err(Error::Binding);
    }
    let before = bridge_git::take_snapshot(&source.paths.checkout).map_err(|_| Error::Git)?;
    if &fingerprint(&before) != expected {
        return Err(Error::Binding);
    }
    // Cumulative artifacts are derived relative to the original main snapshot,
    // while the worker baseline is captured after inherited files are seeded.
    let baseline = RepositorySnapshot::from_json(parent.snapshot.as_ref().ok_or(Error::Baseline)?)
        .map_err(|_| Error::Baseline)?;
    let (derived, _) = build_entries(
        &source.paths.checkout,
        parent_id,
        base,
        &baseline,
        &parent.allowed_paths,
    )
    .map_err(|_| Error::Binding)?;
    let directory = source.paths.runtime_dir.join("artifact");
    let accepted = load_artifact(&directory).map_err(|_| Error::Binding)?;
    if accepted != derived || accepted.task_id != parent_id || accepted.base_head != base {
        return Err(Error::Binding);
    }
    for entry in &accepted.entries {
        if !task.allowed_paths.iter().any(|s| {
            if s.ends_with('/') {
                entry.path.starts_with(s)
            } else {
                entry.path == *s
            }
        }) || !entry.matches_base(
            artifact::read_object(checkout, &entry.path)
                .map_err(|_| Error::Binding)?
                .as_ref(),
        ) {
            return Err(Error::Binding);
        }
    }
    for entry in &accepted.entries {
        materialize::execute(&directory, checkout, entry).map_err(|_| Error::Git)?;
    }
    let after = bridge_git::take_snapshot(&source.paths.checkout).map_err(|_| Error::Git)?;
    let inherited = bridge_git::take_snapshot(checkout).map_err(|_| Error::Git)?;
    if load_artifact(&directory).map_err(|_| Error::Binding)? != accepted
        || &fingerprint(&after) != expected
        || inherited.manifest() != after.manifest()
        || inherited.head() != after.head()
        || inherited.index_fingerprint() != after.index_fingerprint()
    {
        return Err(Error::Binding);
    }
    Ok(())
}
