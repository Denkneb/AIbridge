//! Independent managed acceptance evidence. Callers hold review fences.
use bridge_config::ProjectEntry;
use bridge_domain::{RoundStatus, TaskStatus, VerificationStatus};
use bridge_git::{RepositorySnapshot, checkout::probe_checkout};
use bridge_storage::{RoundRow, RustStateLayout, Task, WorktreeStatus};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
type Result<T> = std::result::Result<T, &'static str>;
pub struct Evidence {
    pub root: PathBuf,
    pub fingerprint: Value,
    pub round: u32,
    pub verification: Value,
    pub baseline: Value,
    pub changed_paths: Vec<String>,
}
pub fn acceptance_checks(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: &Task,
    step: &Value,
) -> Result<Evidence> {
    let storage = layout
        .open_readonly()
        .map_err(|_| "automation_state_unavailable")?;
    let current = storage
        .get_task(task.task_id)
        .map_err(|_| "automation_state_unavailable")?
        .ok_or("automation_task_missing")?;
    if current.project_id != *project.id()
        || layout.project_id() != project.id()
        || Path::new(&current.workspace) != project.workspace()
        || !matches!(
            current.status,
            TaskStatus::AwaitingReview | TaskStatus::Accepted
        )
        || current.close_requested_at.is_some()
    {
        return Err("automation_task_not_reviewable");
    }
    let mode: String = storage
        .connection()
        .query_row(
            "SELECT execution_mode FROM tasks WHERE task_id=?1",
            [task.task_id.to_string()],
            |r| r.get(0),
        )
        .map_err(|_| "automation_state_unavailable")?;
    if mode != "worktree" || current.delivery_mode != bridge_domain::DeliveryMode::Manual {
        return Err("automation_execution_policy_mismatch");
    }
    let record = storage
        .get_worktree(task.task_id, project.id())
        .map_err(|_| "automation_state_unavailable")?
        .ok_or("automation_checkout_missing")?;
    if record.status != WorktreeStatus::Created || record.base_head != current.base_head {
        return Err("automation_checkout_invalid");
    }
    let base = current
        .base_head
        .as_deref()
        .ok_or("automation_checkout_invalid")?;
    let binding = probe_checkout(
        project.workspace(),
        &layout.project_dir(),
        task.task_id,
        Path::new(&record.path),
        Some(base),
    )
    .map_err(|_| "automation_checkout_invalid")?;
    if record.runtime_dir.as_deref() != binding.paths.runtime_dir.to_str() {
        return Err("automation_checkout_invalid");
    }
    let baseline: Value = serde_json::from_str(
        record
            .baseline_json
            .as_deref()
            .ok_or("automation_baseline_missing")?,
    )
    .map_err(|_| "automation_baseline_invalid")?;
    let baseline_snapshot =
        RepositorySnapshot::from_json(&baseline).map_err(|_| "automation_baseline_invalid")?;
    let paths = step["allowed_paths"]
        .as_array()
        .ok_or("automation_step_invalid")?
        .iter()
        .map(|p| {
            p.as_str()
                .map(str::to_owned)
                .ok_or("automation_step_invalid")
        })
        .collect::<Result<Vec<_>>>()?;
    let commands = step["test_commands"]
        .as_array()
        .filter(|c| !c.is_empty())
        .ok_or("automation_step_invalid")?;
    if current.test_commands
        != commands
            .iter()
            .map(|c| {
                c.as_str()
                    .map(str::to_owned)
                    .ok_or("automation_step_invalid")
            })
            .collect::<Result<Vec<_>>>()?
    {
        return Err("automation_commands_changed");
    }
    let snapshot = bridge_git::take_snapshot(&binding.paths.checkout)
        .map_err(|_| "automation_checkout_invalid")?;
    let fingerprint = bridge_artifact::fingerprint(&snapshot);
    let comparison = bridge_git::compare_repository_snapshot(
        &binding.paths.checkout,
        &baseline_snapshot,
        &paths,
        false,
        false,
    )
    .map_err(|_| "automation_checkout_invalid")?;
    if !comparison.scope_violations().is_empty() {
        return Err("automation_step_scope_violation");
    }
    if !comparison.git_policy_violations().is_empty()
        || snapshot.head() != baseline_snapshot.head()
        || snapshot.index_fingerprint() != baseline_snapshot.index_fingerprint()
    {
        return Err("automation_head_or_index_changed");
    }
    let row = storage
        .connection()
        .query_row(
            "SELECT * FROM rounds WHERE task_id=?1 ORDER BY round_number DESC LIMIT 1",
            [task.task_id.to_string()],
            |r| Ok(RoundRow::from_row(r)),
        )
        .map_err(|_| "automation_round_missing")?
        .map_err(|_| "automation_round_invalid")?;
    if row.project_id != current.project_id
        || row.task_id != current.task_id
        || row.status != RoundStatus::Complete
        || row.verifier_state != Some(bridge_domain::VerifierState::Done)
    {
        return Err("automation_verification_missing");
    }
    let verification = row.verifier_json.ok_or("automation_verification_missing")?;
    if verification.status != VerificationStatus::Passed
        || verification
            .side_effects
            .as_ref()
            .is_some_and(|v| !v.is_empty())
        || verification
            .repositories
            .as_ref()
            .is_some_and(|v| !v.is_empty())
    {
        return Err("automation_verification_failed");
    }
    if verification.before.as_ref().map(|v| json!(v)) != Some(fingerprint.clone())
        || verification.after.as_ref().map(|v| json!(v)) != Some(fingerprint.clone())
    {
        return Err("automation_verification_stale");
    }
    let entries = verification
        .commands
        .as_ref()
        .ok_or("automation_verification_missing")?;
    if json!(entries.iter().map(|c| &c.command).collect::<Vec<_>>()) != json!(commands)
        || entries
            .iter()
            .any(|c| c.exit_code != Some(0) || c.timed_out == Some(true) || c.reason.is_some())
    {
        return Err("automation_commands_not_passed");
    }
    if bridge_artifact::fingerprint(
        &bridge_git::take_snapshot(&binding.paths.checkout)
            .map_err(|_| "automation_checkout_invalid")?,
    ) != fingerprint
    {
        return Err("automation_checkout_changed");
    }
    Ok(Evidence {
        root: binding.paths.checkout,
        fingerprint,
        round: row.round_number,
        verification: json!(verification),
        baseline,
        changed_paths: comparison
            .changed_paths()
            .iter()
            .map(|p| {
                p.to_str()
                    .map(str::to_owned)
                    .ok_or("automation_path_invalid")
            })
            .collect::<Result<_>>()?,
    })
}
pub fn positive_review_gate(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: &Task,
) -> Result<Evidence> {
    let run_id = task
        .snapshot
        .as_ref()
        .and_then(|s| s["automation_run_id"].as_str())
        .ok_or("automation_task_unmanaged")?
        .parse()
        .map_err(|_| "automation_run_invalid")?;
    let run = crate::automation::active_run(layout, project, run_id)?;
    let storage = layout
        .open_readonly()
        .map_err(|_| "automation_state_unavailable")?;
    let workflow: Option<String> = storage
        .connection()
        .query_row(
            "SELECT workflow_id FROM tasks WHERE task_id=?1 AND project_id=?2",
            rusqlite::params![task.task_id.to_string(), project.id().as_str()],
            |r| r.get(0),
        )
        .map_err(|_| "automation_state_unavailable")?;
    if workflow.as_deref() != Some(run_id.to_string().as_str()) {
        return Err("automation_workflow_changed");
    }
    let item = crate::automation::current_item(&run)?;
    if run.document()["phase"] != "steps"
        || item["task_id"] != json!(task.task_id)
        || item["phase"] != "accept"
        || item["review"]["decision"] != "accept"
        || item["review"]["findings"] != json!([])
        || !item["review"]["summary"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty())
    {
        return Err("automation_positive_review_required");
    }
    let evidence = acceptance_checks(layout, project, task, &item["step"])?;
    if item["review"]["fingerprint"] != evidence.fingerprint
        || item["review"]["round"] != evidence.round
    {
        return Err("automation_review_stale");
    }
    Ok(evidence)
}
