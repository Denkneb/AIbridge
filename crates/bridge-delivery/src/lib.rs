//! Byte-complete accepted checkout delivery. All diagnostics use fixed codes.
mod artifact;
mod materialize;
pub use artifact::{Artifact, Entry, build_entries, load_artifact};
use bridge_config::ProjectEntry;
use bridge_domain::{TaskId, TaskStatus};
use bridge_storage::{RustStateLayout, Task, WorktreeRecord};
use bridge_storage::{WorktreeDeliveryState, WorktreeStatus};
use bridge_worker::{WorkerLock, WorkerLockOutcome};
use serde_json::{Value, json};
use std::{
    fmt,
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, DeliveryError>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryError {
    pub code: &'static str,
    pub paths: Vec<String>,
}
impl DeliveryError {
    pub(crate) fn new(code: &'static str) -> Self {
        Self {
            code,
            paths: vec![],
        }
    }
    pub(crate) fn paths(code: &'static str, paths: Vec<String>) -> Self {
        Self { code, paths }
    }
}
impl fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for DeliveryError {}
struct Context {
    task: Task,
    record: WorktreeRecord,
    checkout: PathBuf,
    runtime: PathBuf,
}
fn context(layout: &RustStateLayout, project: &ProjectEntry, id: TaskId) -> Result<Context> {
    let storage = layout
        .open()
        .map_err(|_| DeliveryError::new("state_unavailable"))?;
    let task = storage
        .get_task(id)
        .map_err(|_| DeliveryError::new("state_unavailable"))?
        .ok_or(DeliveryError::new("unknown_task"))?;
    if &task.project_id != project.id() || Path::new(&task.workspace) != project.workspace() {
        return Err(DeliveryError::new("task_binding_mismatch"));
    }
    if task.status != TaskStatus::Accepted {
        return Err(DeliveryError::new("not_accepted"));
    }
    if storage
        .writer_activity_present(project.id())
        .map_err(|_| DeliveryError::new("state_unavailable"))?
    {
        return Err(DeliveryError::new("active_writers"));
    }
    let record = storage
        .get_worktree(id, project.id())
        .map_err(|_| DeliveryError::new("state_unavailable"))?
        .ok_or(DeliveryError::new("no_worktree"))?;
    if record.status != WorktreeStatus::Created {
        return Err(DeliveryError::new("worktree_not_created"));
    }
    let binding = bridge_git::checkout::probe_checkout(
        project.workspace(),
        &layout.project_dir(),
        id,
        Path::new(&record.path),
        None,
    )
    .map_err(|_| DeliveryError::new("repo_identity_mismatch"))?;
    if record.runtime_dir.as_deref().map(Path::new) != Some(binding.paths.runtime_dir.as_path())
        || record.base_head.is_none()
        || task.base_head != record.base_head
    {
        return Err(DeliveryError::new("worktree_path_mismatch"));
    }
    Ok(Context {
        task,
        record,
        checkout: binding.paths.checkout,
        runtime: binding.paths.runtime_dir,
    })
}
fn derive(ctx: &Context) -> Result<(Artifact, std::collections::BTreeMap<String, Vec<u8>>)> {
    let baseline: Value = serde_json::from_str(
        ctx.record
            .baseline_json
            .as_deref()
            .ok_or(DeliveryError::new("baseline_missing"))?,
    )
    .map_err(|_| DeliveryError::new("baseline_corrupt"))?;
    let baseline = bridge_git::RepositorySnapshot::from_json(&baseline)
        .map_err(|_| DeliveryError::new("baseline_corrupt"))?;
    build_entries(
        &ctx.checkout,
        ctx.task.task_id,
        ctx.record
            .base_head
            .as_deref()
            .ok_or(DeliveryError::new("base_head_missing"))?,
        &baseline,
        &ctx.task.allowed_paths,
    )
}
/// Builds under admission and review fences, rejecting any reserved or real writer.
pub fn build(layout: &RustStateLayout, project: &ProjectEntry, id: TaskId) -> Result<Value> {
    let _admission = match WorkerLock::try_acquire_admission(layout)
        .map_err(|_| DeliveryError::new("state_unavailable"))?
    {
        WorkerLockOutcome::Busy => return Err(DeliveryError::new("project_busy")),
        WorkerLockOutcome::Acquired(g) => g,
    };
    // Accepted tasks have no reservation, so review fencing also probes every
    // task lock if a prior parallel executor lost its reservation.
    let _fences = bridge_worker::admission::try_review_fences_while_admitted(layout, project, id)
        .map_err(|_| DeliveryError::new("state_unavailable"))?
        .ok_or(DeliveryError::new("project_busy"))?;
    build_admitted(layout, project, id)
}
fn build_admitted(layout: &RustStateLayout, project: &ProjectEntry, id: TaskId) -> Result<Value> {
    let ctx = context(layout, project, id)?;
    if ctx
        .record
        .delivery_state
        .is_some_and(|s| s != WorktreeDeliveryState::None)
    {
        return Err(DeliveryError::new("delivery_in_progress"));
    }
    let (artifact, blobs) = derive(&ctx)?;
    artifact::write_artifact(&ctx.runtime.join("artifact"), &artifact, &blobs)?;
    Ok(
        json!({"mode":"build","status":"built","entries":artifact.entries,"base_head":artifact.base_head,"delivery_state":"none"}),
    )
}
/// Read-only artifact/checkout and clean-main preflight. It shares all checks
/// with apply; no artifact, journal, database or workspace content is written.
pub fn dry_run(layout: &RustStateLayout, project: &ProjectEntry, id: TaskId) -> Result<Value> {
    let _admission = match WorkerLock::try_acquire_admission(layout)
        .map_err(|_| DeliveryError::new("state_unavailable"))?
    {
        WorkerLockOutcome::Busy => return Err(DeliveryError::new("project_busy")),
        WorkerLockOutcome::Acquired(g) => g,
    };
    let _fences = bridge_worker::admission::try_review_fences_while_admitted(layout, project, id)
        .map_err(|_| DeliveryError::new("state_unavailable"))?
        .ok_or(DeliveryError::new("project_busy"))?;
    let ctx = context(layout, project, id)?;
    materialize::run(&ctx, layout, project, false, &mut |_| false)
}
/// Applies or resumes a proven artifact while holding admission and task fences.
pub fn apply(layout: &RustStateLayout, project: &ProjectEntry, id: TaskId) -> Result<Value> {
    apply_with_fault(layout, project, id, |_| false)
}
/// Deterministic interruption hook for integration tests. Returning true stops
/// immediately at a durable boundary; state remains available for resume.
pub fn apply_with_fault(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: TaskId,
    mut hook: impl FnMut(&str) -> bool,
) -> Result<Value> {
    let _admission = match WorkerLock::try_acquire_admission(layout)
        .map_err(|_| DeliveryError::new("state_unavailable"))?
    {
        WorkerLockOutcome::Busy => return Err(DeliveryError::new("project_busy")),
        WorkerLockOutcome::Acquired(g) => g,
    };
    let _fences = bridge_worker::admission::try_review_fences_while_admitted(layout, project, id)
        .map_err(|_| DeliveryError::new("state_unavailable"))?
        .ok_or(DeliveryError::new("project_busy"))?;
    let ctx = context(layout, project, id)?;
    materialize::run(&ctx, layout, project, true, &mut hook)
}

fn validate_artifact(ctx: &Context, derived: &Artifact, required: bool) -> Result<()> {
    let dest = ctx.runtime.join("artifact");
    if dest.join("manifest.json").exists() {
        if load_artifact(&dest)? != *derived {
            return Err(DeliveryError::new("artifact_drift"));
        }
    } else if required {
        return Err(DeliveryError::new("artifact_missing"));
    }
    for entry in &derived.entries {
        if !entry.matches_artifact(artifact::read_object(&ctx.checkout, &entry.path)?.as_ref()) {
            return Err(DeliveryError::paths(
                "artifact_drift",
                vec![entry.path.clone()],
            ));
        }
    }
    Ok(())
}
fn initial_preflight(project: &ProjectEntry, artifact: &Artifact) -> Result<()> {
    let snapshot = bridge_git::take_snapshot(project.workspace())
        .map_err(|_| DeliveryError::new("snapshot_failed"))?;
    if !snapshot.dirty_paths().is_empty() {
        return Err(DeliveryError::new("dirty_main_workspace"));
    }
    let head = snapshot
        .head()
        .ok_or(DeliveryError::new("base_head_missing"))?
        .as_str();
    if !bridge_git::objects::commit_descends_from(project.workspace(), &artifact.base_head, head)
        .map_err(|_| DeliveryError::new("history_unreadable"))?
    {
        return Err(DeliveryError::new("history_rewritten"));
    }
    for entry in &artifact.entries {
        if !entry.matches_base(artifact::read_object(project.workspace(), &entry.path)?.as_ref()) {
            return Err(DeliveryError::paths(
                "target_conflict",
                vec![entry.path.clone()],
            ));
        }
    }
    Ok(())
}
