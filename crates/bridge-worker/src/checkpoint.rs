//! Non-authoritative checkpoints: exact compact state, immediate predecessor only.
use bridge_domain::{CheckpointPath, CheckpointRef, RoundCheckpoint};
use bridge_git::checkpoint::{diff_round_stat, empty_diff_stat, worktree_state};
use bridge_storage::{RoundRef, StorageConnection};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

/// Build from the task's persisted baseline and authenticated execution root.
/// Diagnostic failures return unavailable and do not prevent the round finish.
/// Worktree callers must supply their frozen collection baseline and root;
/// authentication/binding of that checkout remains the caller's responsibility.
pub fn build_round_checkpoint(
    storage: &StorageConnection,
    round: &RoundRef,
    root: &Path,
    trusted_roots: &[&Path],
    collection_baseline: Option<&Value>,
) -> Option<RoundCheckpoint> {
    if round.round_number == 0 {
        return None;
    }
    let task = storage.get_task(round.task_id).ok()??;
    if task.project_id != round.project_id {
        return None;
    }
    if collection_baseline.is_none()
        && std::fs::canonicalize(&task.workspace).ok()? != std::fs::canonicalize(root).ok()?
    {
        return None;
    }
    let empty = json!({});
    let snapshot = collection_baseline
        .or(task.snapshot.as_ref())
        .unwrap_or(&empty);
    let manifest = |value: &Value| -> Option<BTreeMap<String, String>> {
        let Some(value) = value.get("manifest") else {
            return Some(BTreeMap::new());
        };
        value
            .as_object()?
            .iter()
            .map(|(path, digest)| {
                CheckpointPath::try_from(path.clone()).ok()?;
                let digest = CheckpointRef::try_from(digest.as_str()?.to_owned()).ok()?;
                Some((path.clone(), digest.as_str().to_owned()))
            })
            .collect()
    };
    let mut specs = vec![(root.to_path_buf(), manifest(snapshot)?)];
    let mut externals = match snapshot.get("external_repositories") {
        None => Vec::new(),
        Some(value) => value.as_array()?.clone(),
    };
    externals.sort_by(|a, b| a["root"].as_str().cmp(&b["root"].as_str()));
    let mut seen = HashSet::new();
    for external in externals {
        let external_root = external["root"].as_str()?;
        let validated = bridge_path_policy::validate_allowed_paths_with_trusted_roots(
            root,
            trusted_roots,
            &[external_root],
        )
        .ok()?;
        if !validated[0].is_external() {
            return None;
        }
        let path = PathBuf::from(validated[0].path());
        if !seen.insert(path.clone()) {
            return None;
        }
        specs.push((path, manifest(&external)?));
    }
    let previous = if round.round_number == 1 {
        None
    } else {
        Some(
            storage
                .get_round_checkpoint(&RoundRef {
                    round_number: round.round_number - 1,
                    ..round.clone()
                })
                .ok()??,
        )
    };
    capture_checkpoint(&specs, previous.as_ref())
}

/// Roots are already authenticated by the caller. Only relative paths and
/// workspace/external N labels are serialized, never the repository roots.
pub fn capture_checkpoint(
    specs: &[(PathBuf, BTreeMap<String, String>)],
    previous: Option<&RoundCheckpoint>,
) -> Option<RoundCheckpoint> {
    if specs.is_empty() || specs.len() > 64 {
        return None;
    }
    if let Some(previous) = previous {
        previous.validate().ok()?;
        if previous.repositories.len() != specs.len()
            || previous.repositories.iter().any(|r| !r.available)
        {
            return None;
        }
    }
    let mut repositories = Vec::new();
    let mut top = (Value::Null, Value::Null, Value::Null);
    for (index, (root, baseline)) in specs.iter().enumerate() {
        for (path, digest) in baseline {
            CheckpointPath::try_from(path.clone()).ok()?;
            CheckpointRef::try_from(digest.clone()).ok()?;
        }
        let label = if index == 0 {
            "workspace".to_owned()
        } else {
            format!("external {index}")
        };
        let current = worktree_state(root).ok().filter(|v| v["head"].is_string());
        let Some(current) = current else {
            repositories
                .push(json!({"label":label,"available":false,"diff_stat":empty_diff_stat()}));
            continue;
        };
        let files = current["files"].as_object()?;
        let changed = files
            .iter()
            .filter(|(name, state)| {
                baseline.get(*name).map(String::as_str) != state["digest"].as_str()
            })
            .map(|(name, state)| (name.clone(), state.clone()))
            .collect::<serde_json::Map<_, _>>();
        let absent = baseline
            .keys()
            .filter(|name| !files.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>();
        if changed.len() + absent.len() > 2000 {
            return None;
        }
        let state = json!({"changed":changed,"absent":absent});
        let prev = previous
            .and_then(|p| serde_json::to_value(&p.repositories[index]).ok())
            .unwrap_or_else(|| json!({"changed":{},"absent":[]}));
        let diff = diff_round_stat(&prev, &state, baseline);
        if index == 0 {
            top = (
                current["head"].clone(),
                current["index_fingerprint"].clone(),
                current["worktree_fingerprint"].clone(),
            );
        }
        repositories.push(json!({"label":label,"available":true,"head":current["head"],"index_fingerprint":current["index_fingerprint"],"worktree_fingerprint":current["worktree_fingerprint"],"changed":state["changed"],"absent":state["absent"],"diff_stat":diff}));
    }
    serde_json::from_value(json!({"version":1,"head":top.0,"index_fingerprint":top.1,"worktree_fingerprint":top.2,"repositories":repositories})).ok()
}

/// Caller holds the worker lock and supplies the already-observed history.
/// Diagnostics, saved usage, round/task status and event commit together.
pub fn finish_round_with_diagnostics(
    storage: &mut StorageConnection,
    input: bridge_storage::FinishRoundInput,
    messages: &[bridge_opencode::Message],
    root: &Path,
    trusted_roots: &[&Path],
    collection_baseline: Option<&Value>,
) -> Result<bridge_storage::RoundUpdateOutcome, bridge_storage::RoundUpdateError> {
    let checkpoint = build_round_checkpoint(
        storage,
        &input.round,
        root,
        trusted_roots,
        collection_baseline,
    );
    let input = crate::usage::accounted_input(storage, input, messages)?;
    storage.finish_round_with_checkpoint(input, checkpoint.as_ref())
}
