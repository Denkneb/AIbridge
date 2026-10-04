//! Frozen external baselines, trusted-root proof and qualified review results.
use crate::execution::ExecutionError;
use bridge_git::{RepositoryComparison, RepositorySnapshot};
use bridge_storage::Task;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::{Component, Path, PathBuf},
};

pub(crate) struct ExternalBaseline {
    pub root: PathBuf,
    pub snapshot: RepositorySnapshot,
}

/// Existing components may not be rebound through symlinks. Missing tails
/// remain provable so disappearance can be reported as external_repo_missing.
pub(crate) fn prove_path(path: &Path) -> Result<(), ExecutionError> {
    if !path.is_absolute()
        || path
            .to_str()
            .is_none_or(|p| p.split('/').any(|s| s == "." || s == ".."))
    {
        return Err(ExecutionError::Binding);
    }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        if !matches!(component, Component::RootDir | Component::Normal(_)) {
            return Err(ExecutionError::Binding);
        }
        prefix.push(component.as_os_str());
        match std::fs::symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(ExecutionError::Binding),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ExecutionError::Binding),
        }
    }
    Ok(())
}

pub(crate) fn external_baselines(
    task: &Task,
    root: &Path,
    trusted: &[PathBuf],
) -> Result<Vec<ExternalBaseline>, ExecutionError> {
    let entries = task
        .snapshot
        .as_ref()
        .and_then(|s| s.get("external_repositories"));
    let entries = match entries {
        None => &[][..],
        Some(v) => v.as_array().ok_or(ExecutionError::Baseline)?.as_slice(),
    };
    if entries.len() > 63 {
        return Err(ExecutionError::Baseline);
    }
    let mut seen = BTreeSet::new();
    let mut external = Vec::new();
    for value in entries {
        let path = PathBuf::from(value["root"].as_str().ok_or(ExecutionError::Baseline)?);
        if path.components().collect::<PathBuf>().as_os_str() != path.as_os_str() {
            return Err(ExecutionError::Binding);
        }
        prove_path(&path)?;
        if path.starts_with(root)
            || root.starts_with(&path)
            || !trusted.iter().any(|t| path.starts_with(t))
            || !seen.insert(path.clone())
        {
            return Err(ExecutionError::Binding);
        }
        // A present Git repository must still be exactly the saved root.
        // Missing/non-Git repositories are diagnosed by verifier/collection.
        if path.is_dir() && bridge_git::is_repository(&path).map_err(|_| ExecutionError::Git)? {
            let groups = bridge_path_policy::group_allowed_paths_by_repo(
                root,
                &[path.to_str().ok_or(ExecutionError::Binding)?],
            )
            .map_err(|_| ExecutionError::Binding)?;
            if groups.last().is_none_or(|g| g.root() != path) {
                return Err(ExecutionError::Binding);
            }
        }
        if !task
            .allowed_paths
            .iter()
            .any(|p| Path::new(p).is_absolute() && Path::new(p).starts_with(&path))
        {
            return Err(ExecutionError::Binding);
        }
        external.push(ExternalBaseline {
            root: path,
            snapshot: RepositorySnapshot::from_json(value).map_err(|_| ExecutionError::Baseline)?,
        });
    }
    let mut affected = BTreeSet::new();
    for scope in task
        .allowed_paths
        .iter()
        .filter(|p| Path::new(p).is_absolute())
    {
        let path = Path::new(scope);
        prove_path(path)?;
        let expected = external
            .iter()
            .filter(|e| path.starts_with(&e.root))
            .max_by_key(|e| e.root.components().count())
            .ok_or(ExecutionError::Binding)?;
        if expected.root.is_dir()
            && bridge_git::is_repository(&expected.root).map_err(|_| ExecutionError::Git)?
        {
            let groups = bridge_path_policy::group_allowed_paths_by_repo(root, &[scope])
                .map_err(|_| ExecutionError::Binding)?;
            if groups.last().is_none_or(|g| g.root() != expected.root) {
                return Err(ExecutionError::Binding);
            }
        }
        affected.insert(expected.root.clone());
    }
    if external.iter().any(|e| !affected.contains(&e.root)) {
        return Err(ExecutionError::Binding);
    }
    external.sort_by(|a, b| a.root.cmp(&b.root));
    Ok(external)
}
fn paths(paths: &[OsString]) -> Result<Vec<&str>, ExecutionError> {
    paths
        .iter()
        .map(|p| p.to_str().ok_or(ExecutionError::Git))
        .collect()
}
fn repository_json(changes: &RepositoryComparison) -> Result<Value, ExecutionError> {
    Ok(json!({
        "root": changes.root().to_str().ok_or(ExecutionError::Git)?,
        "baseline_dirty_paths": paths(changes.baseline_dirty_paths())?,
        "changed_paths": paths(changes.changed_paths())?,
        "committed_paths": paths(changes.committed_paths())?,
        "scope_violations": paths(changes.scope_violations())?,
        "git_policy_violations": changes.git_policy_violations().iter().map(|v| v.as_str()).collect::<Vec<_>>(),
        "head_before": changes.head_before().map(|v| v.as_str()),
        "head_after": changes.head_after().map(|v| v.as_str()),
    }))
}
pub(crate) fn collection_json(changes: &[RepositoryComparison]) -> Result<Value, ExecutionError> {
    let repositories = changes
        .iter()
        .map(repository_json)
        .collect::<Result<Vec<_>, _>>()?;
    aggregate(repositories)
}
fn aggregate(mut repositories: Vec<Value>) -> Result<Value, ExecutionError> {
    if repositories.is_empty() {
        return Err(ExecutionError::Baseline);
    }
    repositories[1..].sort_by(|a, b| a["root"].as_str().cmp(&b["root"].as_str()));
    let mut result = repositories[0].clone();
    result
        .as_object_mut()
        .ok_or(ExecutionError::Git)?
        .remove("root");
    result["task_changed_paths"] = result["changed_paths"].clone();
    if repositories.len() > 1 {
        for key in [
            "changed_paths",
            "committed_paths",
            "scope_violations",
            "git_policy_violations",
        ] {
            let mut values = result[key]
                .as_array()
                .ok_or(ExecutionError::Git)?
                .iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or(ExecutionError::Git))
                .collect::<Result<BTreeSet<_>, _>>()?;
            for repo in &repositories[1..] {
                let root = repo["root"].as_str().ok_or(ExecutionError::Git)?;
                let separator = if key == "git_policy_violations" {
                    ':'
                } else {
                    '/'
                };
                for path in repo[key].as_array().ok_or(ExecutionError::Git)? {
                    values.insert(format!(
                        "{root}{separator}{}",
                        path.as_str().ok_or(ExecutionError::Git)?
                    ));
                }
            }
            result[key] = json!(values);
        }
    }
    result["repositories"] = json!(repositories);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_multi_repository_aggregation_corpus() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../../docs/fixtures/multi-repository-cases.json"
        ))
        .unwrap();
        let mut checked = 0;
        for case in corpus["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["operation"] == "aggregate_repositories")
        {
            let repositories = case["repositories"]
                .as_array()
                .unwrap()
                .iter()
                .map(|input| {
                    let mut repo = input["result"].clone();
                    repo["root"] = input["root"].clone();
                    repo["baseline_dirty_paths"] = input["baseline"]["dirty_paths"].clone();
                    repo
                })
                .collect();
            assert_eq!(
                aggregate(repositories).unwrap(),
                case["expect"],
                "{}",
                case["id"]
            );
            checked += 1;
        }
        assert_eq!(checked, 10);
    }
}
