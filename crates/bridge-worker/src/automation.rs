//! Internal automation provenance. Public MCP arguments cannot mint this identity.
use bridge_config::ProjectEntry;
use bridge_domain::{TaskId, TaskStatus};
use bridge_storage::{
    RustStateLayout, Task,
    automation::{AutomationRun, AutomationRunStore, RunControl, RunId, RunStatus},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};
type Result<T> = std::result::Result<T, &'static str>;

pub fn active_run(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    id: RunId,
) -> Result<AutomationRun> {
    let run = AutomationRunStore::new(layout.clone())
        .load(Some(id))
        .map_err(|_| "automation_state_unavailable")?;
    let b = &run.document()["binding"];
    if layout.project_id() != project.id()
        || b["project"] != project.id().as_str()
        || b["workspace"].as_str().map(Path::new) != Some(project.workspace())
        || b["config_sha256"]
            != format!(
                "{:x}",
                Sha256::digest(
                    std::fs::read(project.source_path())
                        .map_err(|_| "automation_binding_changed")?
                )
            )
    {
        return Err("automation_binding_changed");
    }
    if run.status() != RunStatus::Running || run.control() != RunControl::Run {
        return Err("automation_not_running");
    }
    Ok(run)
}
pub fn current_item(run: &AutomationRun) -> Result<&Value> {
    let index = run.document()["index"]
        .as_u64()
        .ok_or("automation_state_corrupt")? as usize;
    run.document()["steps"]
        .as_array()
        .and_then(|v| v.get(index))
        .filter(|v| v.is_object())
        .ok_or("automation_state_corrupt")
}
/// Returns authoritative snapshot/hash fields after checking the persisted
/// submit intent. Scope/criteria/commands cannot come from a model response.
pub fn submit_provenance(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    run_id: RunId,
    args: &Value,
) -> Result<Value> {
    let run = active_run(layout, project, run_id)?;
    let doc = run.document();
    let item = current_item(&run)?;
    if doc["phase"] != "steps" || item["phase"] != "submit" {
        return Err("automation_phase_mismatch");
    }
    let index = doc["index"].as_u64().ok_or("automation_state_corrupt")? as usize;
    let step = &item["step"];
    let step_id = step["id"].as_str().ok_or("automation_state_corrupt")?;
    let scopes = doc["steps"].as_array().ok_or("automation_state_corrupt")?[..=index]
        .iter()
        .flat_map(|i| i["step"]["allowed_paths"].as_array().into_iter().flatten())
        .map(|p| p.as_str().ok_or("automation_state_corrupt"))
        .collect::<Result<BTreeSet<_>>>()?;
    if args["request_id"] != format!("auto:{run_id}:{step_id}:submit")
        || args["task"] != item["prepared_task"]
        || !item["prepared_task"].is_string()
        || args["allowed_paths"] != json!(scopes)
        || args["test_commands"] != step["test_commands"]
        || args["profile"] != step["profile"]
        || args["workflow_id"] != run_id.to_string()
        || args.get("allow_dirty").is_some_and(|v| *v != false)
        || args.get("allow_commit").is_some_and(|v| *v != false)
        || args.get("budget").is_some_and(|v| !v.is_null())
    {
        return Err("automation_approved_step_mismatch");
    }
    let main =
        bridge_git::take_snapshot(project.workspace()).map_err(|_| "automation_binding_changed")?;
    if main.to_json().map_err(|_| "automation_binding_changed")? != doc["origin"]["snapshot"] {
        return Err("automation_binding_changed");
    }
    let mut provenance = json!({"automation_run_id":run_id.to_string()});
    if index > 0 {
        let parent = &doc["steps"][index - 1];
        let id: TaskId = parent["task_id"]
            .as_str()
            .ok_or("automation_parent_invalid")?
            .parse()
            .map_err(|_| "automation_parent_invalid")?;
        let storage = layout
            .open_readonly()
            .map_err(|_| "automation_state_unavailable")?;
        let task = storage
            .get_task(id)
            .map_err(|_| "automation_state_unavailable")?
            .ok_or("automation_parent_invalid")?;
        let workflow: Option<String> = storage
            .connection()
            .query_row(
                "SELECT workflow_id FROM tasks WHERE task_id=?1 AND project_id=?2",
                rusqlite::params![id.to_string(), project.id().as_str()],
                |r| r.get(0),
            )
            .map_err(|_| "automation_parent_invalid")?;
        if parent["phase"] != "accepted"
            || task.status != TaskStatus::Accepted
            || task.project_id != *project.id()
            || Path::new(&task.workspace) != project.workspace()
            || workflow.as_deref() != Some(&run_id.to_string())
            || args["depends_on"] != json!([{"project_id":project.id(),"task_id":id}])
            || args["inherit_task_id"] != json!(id)
            || args["inherit_fingerprint"] != parent["review"]["fingerprint"]
            || parent["review"]["decision"] != "accept"
        {
            return Err("automation_parent_invalid");
        }
        let record = storage
            .get_worktree(id, project.id())
            .map_err(|_| "automation_parent_invalid")?
            .ok_or("automation_parent_invalid")?;
        let root = bridge_git::checkout::probe_checkout(
            project.workspace(),
            &layout.project_dir(),
            id,
            Path::new(&record.path),
            task.base_head.as_deref(),
        )
        .map_err(|_| "automation_parent_invalid")?;
        if task.base_head.as_deref() != main.head().map(|h| h.as_str())
            || bridge_artifact::fingerprint(
                &bridge_git::take_snapshot(&root.paths.checkout)
                    .map_err(|_| "automation_parent_invalid")?,
            ) != parent["review"]["fingerprint"]
        {
            return Err("automation_parent_invalid");
        }
        provenance["automation_parent"] = json!(id);
        provenance["automation_parent_fingerprint"] = parent["review"]["fingerprint"].clone();
    } else if args.get("inherit_task_id").is_some_and(|v| !v.is_null())
        || args
            .get("inherit_fingerprint")
            .is_some_and(|v| !v.is_null())
        || args
            .get("depends_on")
            .is_some_and(|v| !v.is_null() && *v != json!([]))
    {
        return Err("automation_parent_invalid");
    }
    Ok(provenance)
}
pub fn revision_authorized(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: &Task,
    caller: Option<RunId>,
    args: &Value,
) -> Result<()> {
    let managed = task
        .snapshot
        .as_ref()
        .and_then(|s| s.get("automation_run_id"));
    if managed.is_none() {
        return if caller.is_none() {
            Ok(())
        } else {
            Err("automation_task_mismatch")
        };
    }
    let caller = caller.ok_or("automation_managed")?;
    if managed != Some(&json!(caller.to_string())) {
        return Err("automation_managed");
    }
    let run = active_run(layout, project, caller)?;
    let item = current_item(&run)?;
    if item["task_id"] != json!(task.task_id)
        || item["phase"] != "revise"
        || args["request_id"] != item["pending_revision"]["request_id"]
        || args["findings"] != item["pending_revision"]["findings"]
        || args
            .get("allow_budget_override")
            .is_some_and(|v| *v != false)
    {
        return Err("automation_revision_mismatch");
    }
    Ok(())
}
