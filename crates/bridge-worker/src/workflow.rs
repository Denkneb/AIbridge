//! Read-only workflow references and bounded cycle checks. No state is created.
use bridge_config::ProjectEntry;
use bridge_domain::{TaskStatus, WorkflowMetadata};
use bridge_storage::RustStateLayout;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::HashSet;

pub fn normalize(args: &Value) -> Result<WorkflowMetadata, &'static str> {
    let workflow = args.get("workflow_id").cloned().unwrap_or(Value::Null);
    if !workflow.is_null()
        && serde_json::from_value::<bridge_domain::WorkflowId>(workflow.clone()).is_err()
    {
        return Err("invalid_workflow_id");
    }
    let edges = args
        .get("depends_on")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or(json!([]));
    let entries = edges.as_array().ok_or("invalid_depends_on")?;
    let mut seen = HashSet::new();
    for entry in entries {
        let object = entry.as_object().ok_or("invalid_depends_on")?;
        if object.keys().any(|k| k != "project_id" && k != "task_id") {
            return Err("invalid_depends_on");
        }
        let edge: bridge_domain::DependencyReference =
            serde_json::from_value(entry.clone()).map_err(|_| "invalid_dependency_identifier")?;
        if !seen.insert((edge.project_id, edge.task_id)) {
            return Err("duplicate_dependency");
        }
    }
    if !entries.is_empty() && workflow.is_null() {
        return Err("depends_on_requires_workflow");
    }
    serde_json::from_value(json!({"workflow_id":workflow,"depends_on":edges}))
        .map_err(|_| "invalid_depends_on")
}

#[derive(Clone)]
pub struct Node {
    pub status: TaskStatus,
    pub metadata: WorkflowMetadata,
}
fn resolve_project<'a>(
    project: &'a ProjectEntry,
    registry: &'a [ProjectEntry],
    id: &str,
) -> Option<&'a ProjectEntry> {
    if id == project.id().as_str() {
        return Some(project);
    }
    registry.iter().find(|p| {
        p.id().as_str() == id
            && project
                .auto_approve_external_directories()
                .contains(&p.workspace().to_path_buf())
    })
}
fn read_node(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: &str,
) -> Result<Option<Node>, ()> {
    if !layout.database().exists() {
        return Ok(None);
    }
    let storage = layout.open_readonly().map_err(|_| ())?;
    let row: Option<(String,String,Option<String>,Option<String>)> = storage.connection().query_row(
        "SELECT status,workspace,workflow_id,depends_on FROM tasks WHERE task_id=?1 AND project_id=?2",
        rusqlite::params![task,project.id().as_str()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))
    ).optional().map_err(|_| ())?;
    row.map(|(status, workspace, workflow, edges)| {
        if std::path::Path::new(&workspace) != project.workspace() { return Err(()); }
        let metadata = normalize(&json!({"workflow_id":workflow,"depends_on":edges.map(|s|serde_json::from_str::<Value>(&s)).transpose().map_err(|_| ())?.unwrap_or(json!([]))})).map_err(|_| ())?;
        Ok(Node { status:serde_json::from_value(json!(status)).map_err(|_| ())?, metadata })
    }).transpose()
}
fn node(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    registry: &[ProjectEntry],
    p: &str,
    task: &str,
) -> Result<Option<Node>, ()> {
    let target = resolve_project(project, registry, p).ok_or(())?;
    let layout = RustStateLayout::new(layout.state_root(), target.id().clone()).map_err(|_| ())?;
    read_node(&layout, target, task)
}

pub fn validate_references(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    registry: &[ProjectEntry],
    metadata: &WorkflowMetadata,
) -> Result<(), &'static str> {
    for edge in &metadata.depends_on {
        if resolve_project(project, registry, edge.project_id.as_str()).is_none() {
            return Err("unknown_dependency_project");
        }
        let n = node(
            layout,
            project,
            registry,
            edge.project_id.as_str(),
            edge.task_id.as_str(),
        )
        .map_err(|_| "dependency_graph_unreadable")?
        .ok_or("missing_dependency_task")?;
        if n.metadata.workflow_id != metadata.workflow_id {
            return Err("workflow_mismatch");
        }
    }
    struct Walk<'a> {
        layout: &'a RustStateLayout,
        project: &'a ProjectEntry,
        registry: &'a [ProjectEntry],
        visiting: HashSet<(String, String)>,
        done: HashSet<(String, String)>,
        remaining: usize,
    }
    impl Walk<'_> {
        fn visit(&mut self, p: &str, t: &str, depth: usize) -> Result<(), &'static str> {
            if depth > 64 || self.remaining == 0 {
                return Err("dependency_cycle");
            }
            self.remaining -= 1;
            let key = (p.to_owned(), t.to_owned());
            if self.visiting.contains(&key) {
                return Err("dependency_cycle");
            }
            if self.done.contains(&key) {
                return Ok(());
            }
            let n = node(self.layout, self.project, self.registry, p, t)
                .map_err(|_| "dependency_graph_unreadable")?
                .ok_or("dependency_graph_unreadable")?;
            self.visiting.insert(key.clone());
            for edge in n.metadata.depends_on {
                self.visit(edge.project_id.as_str(), edge.task_id.as_str(), depth + 1)?;
            }
            self.visiting.remove(&key);
            self.done.insert(key);
            Ok(())
        }
    }
    let mut walk = Walk {
        layout,
        project,
        registry,
        visiting: HashSet::new(),
        done: HashSet::new(),
        remaining: 1024,
    };
    for edge in &metadata.depends_on {
        walk.visit(edge.project_id.as_str(), edge.task_id.as_str(), 1)?;
    }
    Ok(())
}

pub fn saved_metadata(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    task: bridge_domain::TaskId,
) -> Result<WorkflowMetadata, &'static str> {
    read_node(layout, project, &task.to_string())
        .map_err(|_| "workflow_metadata_unreadable")?
        .map(|n| n.metadata)
        .ok_or("workflow_task_missing")
}
/// Only fixed statuses/reasons and structurally safe identifiers are exposed.
pub fn gate(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    registry: &[ProjectEntry],
    metadata: &WorkflowMetadata,
) -> Value {
    let mut ready = true;
    let dependencies = metadata
        .depends_on
        .iter()
        .map(|edge| {
            let mut entry = json!({"project_id":edge.project_id,"task_id":edge.task_id});
            let (reason, status) =
                if resolve_project(project, registry, edge.project_id.as_str()).is_none() {
                    (Some("unlinked"), None)
                } else {
                    match node(
                        layout,
                        project,
                        registry,
                        edge.project_id.as_str(),
                        edge.task_id.as_str(),
                    ) {
                        Ok(Some(n))
                            if metadata.workflow_id.is_some()
                                && n.metadata.workflow_id == metadata.workflow_id
                                && n.status == TaskStatus::Accepted =>
                        {
                            (None, Some(n.status))
                        }
                        Ok(Some(n)) => (
                            Some(
                                if metadata.workflow_id.is_none()
                                    || n.metadata.workflow_id != metadata.workflow_id
                                {
                                    "workflow_mismatch"
                                } else {
                                    "not_accepted"
                                },
                            ),
                            Some(n.status),
                        ),
                        Ok(None) => (Some("missing"), None),
                        Err(()) => (Some("unreadable"), None),
                    }
                };
            if let Some(reason) = reason {
                ready = false;
                entry["state"] = json!("blocked");
                entry["reason"] = json!(reason);
            } else {
                entry["state"] = json!("accepted");
            }
            if let Some(status) = status {
                entry["status"] = json!(status);
            }
            entry
        })
        .collect::<Vec<_>>();
    json!({"state":if ready {"ready"} else {"waiting"},"dependencies":dependencies})
}
