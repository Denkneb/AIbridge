//! Content-free per-round Git state and predecessor-only change statistics.
use crate::{
    GitError, head, index_fingerprint, os_from_bytes, run_checked,
    sha256::Sha256,
    worktree::{listed_file_state, surrogateescape_key},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub fn empty_diff_stat() -> Value {
    json!({"counts":{"added":0,"modified":0,"deleted":0,"renamed":0},"entries":[],"truncated":0})
}

/// Same non-ignored file set and digest rules as the baseline manifest. No
/// content or absolute paths leave this function. Non-UTF8 paths fail closed.
pub fn worktree_state(workspace: &Path) -> Result<Value, GitError> {
    let mut names = BTreeMap::new();
    for args in [
        &["ls-files", "-z"][..],
        &["ls-files", "--others", "--exclude-standard", "-z"][..],
    ] {
        let bytes = run_checked(workspace, args)?;
        if bytes.is_empty() {
            continue;
        }
        let body = bytes.strip_suffix(&[0]).ok_or(GitError::MalformedOutput)?;
        for entry in body.split(|b| *b == 0) {
            if entry.is_empty() {
                return Err(GitError::MalformedOutput);
            }
            let name = os_from_bytes(entry);
            names.insert(surrogateescape_key(&name), name);
        }
    }
    let mut files = serde_json::Map::new();
    let mut hash = Sha256::new();
    for name in names.into_values() {
        let text = name.to_str().ok_or(GitError::MalformedOutput)?;
        if let Some((digest, kind)) = listed_file_state(&workspace.join(&name))? {
            let digest = crate::sha256::to_hex(&digest);
            for part in [text, &digest, kind] {
                hash.update(part.as_bytes());
                hash.update(&[0]);
            }
            files.insert(text.to_owned(), json!({"digest":digest,"kind":kind}));
        }
    }
    Ok(
        json!({"head":head(workspace)?.map(|h| h.as_str().to_owned()),"index_fingerprint":index_fingerprint(workspace)?.to_hex(),"worktree_fingerprint":crate::sha256::to_hex(&hash.finalize()),"files":files}),
    )
}

fn state(
    path: &str,
    changed: &Value,
    absent: &BTreeSet<String>,
    baseline: &BTreeMap<String, String>,
) -> (Option<String>, String) {
    if let Some(value) = changed.get(path) {
        return (
            value["digest"].as_str().map(str::to_owned),
            value["kind"].as_str().unwrap_or("unknown").to_owned(),
        );
    }
    if absent.contains(path) {
        return (None, "unknown".to_owned());
    }
    (baseline.get(path).cloned(), "unknown".to_owned())
}
/// Callers pass validated exact states; statistics never fall back to an older
/// round. Rename pairs use equal digests and sorted source/destination order.
pub fn diff_round_stat(
    previous: &Value,
    current: &Value,
    baseline: &BTreeMap<String, String>,
) -> Value {
    let prev = &previous["changed"];
    let cur = &current["changed"];
    let absent = |v: &Value| {
        v["absent"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    };
    let pa = absent(previous);
    let ca = absent(current);
    let mut relevant = pa.union(&ca).cloned().collect::<BTreeSet<_>>();
    for changed in [prev, cur] {
        if let Some(map) = changed.as_object() {
            relevant.extend(map.keys().cloned());
        }
    }
    let mut added: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut deleted = Vec::new();
    let mut modified = Vec::new();
    for path in relevant {
        let (pd, pk) = state(&path, prev, &pa, baseline);
        let (cd, ck) = state(&path, cur, &ca, baseline);
        match (pd, cd) {
            (Some(p), Some(c)) if p != c => {
                modified.push(json!({"path":path,"change":"modified","kind":ck}))
            }
            (None, Some(digest)) => added.entry(digest).or_default().push((path, ck)),
            (Some(digest), None) => deleted.push((path, digest, pk)),
            _ => (),
        }
    }
    let mut renamed = Vec::new();
    let mut remaining_deleted = Vec::new();
    for (path, digest, kind) in deleted {
        if let Some(bucket) = added.get_mut(&digest).filter(|v| !v.is_empty()) {
            let (destination, new_kind) = bucket.remove(0);
            renamed
                .push(json!({"path":destination,"from":path,"change":"renamed","kind":new_kind}));
        } else {
            remaining_deleted.push(json!({"path":path,"change":"deleted","kind":kind}));
        }
    }
    let remaining_added = added
        .into_values()
        .flatten()
        .map(|(path, kind)| json!({"path":path,"change":"added","kind":kind}))
        .collect::<Vec<_>>();
    let counts = json!({"added":remaining_added.len(),"modified":modified.len(),"deleted":remaining_deleted.len(),"renamed":renamed.len()});
    let mut entries = [remaining_added, modified, renamed, remaining_deleted].concat();
    entries.sort_by(|a, b| {
        (a["path"].as_str(), a["change"].as_str()).cmp(&(b["path"].as_str(), b["change"].as_str()))
    });
    let truncated = entries.len().saturating_sub(200);
    entries.truncate(200);
    json!({"counts":counts,"entries":entries,"truncated":truncated})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_python_diff_goldens() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../../docs/fixtures/checkpoint-goldens.json"
        ))
        .unwrap();
        for case in corpus["cases"].as_array().unwrap() {
            let baseline = serde_json::from_value(case["baseline"].clone()).unwrap();
            assert_eq!(
                diff_round_stat(&case["previous"], &case["current"], &baseline),
                case["expect"]
            );
        }
    }
}
