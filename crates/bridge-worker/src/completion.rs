//! Main-repository review result. External aggregation is a separate consumer.
use crate::execution::ExecutionError;
use bridge_git::RepositoryComparison;
use serde_json::{Value, json};
use std::ffi::OsString;

fn paths(paths: &[OsString]) -> Result<Vec<&str>, ExecutionError> {
    paths
        .iter()
        .map(|p| p.to_str().ok_or(ExecutionError::Git))
        .collect()
}
pub(crate) fn collection_json(changes: &RepositoryComparison) -> Result<Value, ExecutionError> {
    // Refuse non-UTF8 instead of silently replacing bytes in persisted paths.
    let repository = json!({
        "root": changes.root().to_str().ok_or(ExecutionError::Git)?,
        "baseline_dirty_paths": paths(changes.baseline_dirty_paths())?,
        "changed_paths": paths(changes.changed_paths())?,
        "committed_paths": paths(changes.committed_paths())?,
        "scope_violations": paths(changes.scope_violations())?,
        "git_policy_violations": changes.git_policy_violations().iter().map(|v| v.as_str()).collect::<Vec<_>>(),
        "head_before": changes.head_before().map(|v| v.as_str()),
        "head_after": changes.head_after().map(|v| v.as_str()),
    });
    let mut result = repository.clone();
    result
        .as_object_mut()
        .ok_or(ExecutionError::Git)?
        .remove("root");
    result["task_changed_paths"] = result["changed_paths"].clone();
    result["repositories"] = json!([repository]);
    Ok(result)
}
