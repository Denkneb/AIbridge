//! Ordered durable journal and per-file materializer. No rollback or Git writes.
use crate::{
    Artifact, Context, DeliveryError, Entry, Result, artifact, derive, initial_preflight,
    validate_artifact,
};
use bridge_config::ProjectEntry;
use bridge_storage::{RustStateLayout, WorktreeDeliveryState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    #[serde(flatten)]
    entry: Entry,
    applied: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    task_id: bridge_domain::TaskId,
    base_head: String,
    main_head: String,
    index_fingerprint: String,
    operations: Vec<Operation>,
}
fn save(path: &Path, journal: &Journal) -> Result<()> {
    artifact::atomic_write(
        path,
        &serde_json::to_vec(journal).map_err(|_| DeliveryError::new("journal_corrupt"))?,
        0o600,
    )
}
fn fault(hook: &mut impl FnMut(&str) -> bool, phase: &str) -> Result<()> {
    if hook(phase) {
        Err(DeliveryError::new("simulated_crash"))
    } else {
        Ok(())
    }
}
fn main_identity(project: &ProjectEntry) -> Result<(String, String)> {
    let head = bridge_git::head(project.workspace())
        .map_err(|_| DeliveryError::new("snapshot_failed"))?
        .ok_or(DeliveryError::new("base_head_missing"))?
        .as_str()
        .to_owned();
    let index = bridge_git::index_fingerprint(project.workspace())
        .map_err(|_| DeliveryError::new("snapshot_failed"))?
        .to_hex();
    Ok((head, index))
}
fn journal(ctx: &Context, project: &ProjectEntry, artifact: &Artifact) -> Result<Journal> {
    let expected = ctx.runtime.join("delivery-journal.json");
    if ctx.record.delivery_journal_path.as_deref() != expected.to_str() {
        return Err(DeliveryError::new("journal_corrupt"));
    }
    let j: Journal = serde_json::from_slice(&artifact::read_file(&expected)?)
        .map_err(|_| DeliveryError::new("journal_corrupt"))?;
    if j.version != 1
        || j.task_id != artifact.task_id
        || j.base_head != artifact.base_head
        || j.operations.len() != artifact.entries.len()
    {
        return Err(DeliveryError::new("journal_corrupt"));
    }
    if !j
        .operations
        .iter()
        .zip(&artifact.entries)
        .all(|(o, e)| o.entry == *e)
    {
        return Err(DeliveryError::new("journal_artifact_mismatch"));
    }
    let (head, index) = main_identity(project)?;
    if head != j.main_head {
        return Err(DeliveryError::new("head_changed"));
    }
    if index != j.index_fingerprint {
        return Err(DeliveryError::new("index_changed"));
    }
    for entry in &artifact.entries {
        let obj = artifact::read_object(project.workspace(), &entry.path)?;
        if !entry.matches_base(obj.as_ref()) && !entry.matches_artifact(obj.as_ref()) {
            return Err(DeliveryError::paths(
                "needs_manual_recovery",
                vec![entry.path.clone()],
            ));
        }
    }
    // External additions, even outside the operation set, are never silently
    // adopted into a resumed delivery.
    let paths = bridge_git::dirty_paths(project.workspace())
        .map_err(|_| DeliveryError::new("snapshot_failed"))?;
    if paths.iter().any(|p| {
        !artifact
            .entries
            .iter()
            .any(|e| p.to_str() == Some(e.path.as_str()))
    }) {
        return Err(DeliveryError::new("needs_manual_recovery"));
    }
    Ok(j)
}
fn post_verify(project: &ProjectEntry, artifact: &Artifact, journal: &Journal) -> Result<()> {
    for e in &artifact.entries {
        if !e.matches_artifact(artifact::read_object(project.workspace(), &e.path)?.as_ref()) {
            return Err(DeliveryError::paths(
                "post_verify_failed",
                vec![e.path.clone()],
            ));
        }
    }
    if main_identity(project)? != (journal.main_head.clone(), journal.index_fingerprint.clone()) {
        return Err(DeliveryError::new("post_verify_failed"));
    }
    let paths = bridge_git::dirty_paths(project.workspace())
        .map_err(|_| DeliveryError::new("post_verify_failed"))?
        .iter()
        .map(|p| {
            p.to_str()
                .map(str::to_owned)
                .ok_or(DeliveryError::new("post_verify_failed"))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let entries = artifact
        .entries
        .iter()
        .map(|e| e.path.clone())
        .collect::<BTreeSet<_>>();
    if !paths.is_subset(&entries) || (journal.main_head == artifact.base_head && paths != entries) {
        return Err(DeliveryError::new("post_verify_failed"));
    }
    Ok(())
}
pub(super) fn run(
    ctx: &Context,
    layout: &RustStateLayout,
    project: &ProjectEntry,
    apply: bool,
    hook: &mut impl FnMut(&str) -> bool,
) -> Result<Value> {
    if ctx.record.delivery_state == Some(WorktreeDeliveryState::Delivered)
        || ctx.record.delivered_at.is_some()
    {
        return Err(DeliveryError::new("already_delivered"));
    }
    let (artifact, _) = derive(ctx)?;
    validate_artifact(ctx, &artifact, apply)?;
    let jpath = ctx.runtime.join("delivery-journal.json");
    let mut j = if ctx.record.delivery_state == Some(WorktreeDeliveryState::Applying) {
        journal(ctx, project, &artifact)?
    } else {
        initial_preflight(project, &artifact)?;
        let (main_head, index_fingerprint) = main_identity(project)?;
        Journal {
            version: 1,
            task_id: artifact.task_id,
            base_head: artifact.base_head.clone(),
            main_head,
            index_fingerprint,
            operations: artifact
                .entries
                .iter()
                .map(|e| Operation {
                    entry: e.clone(),
                    applied: false,
                })
                .collect(),
        }
    };
    if !apply {
        return Ok(
            json!({"mode":"dry-run","status":"validated","entries":artifact.entries,"base_head":artifact.base_head,"delivery_state":ctx.record.delivery_state.unwrap_or(WorktreeDeliveryState::None).as_str()}),
        );
    }
    if ctx.record.delivery_state != Some(WorktreeDeliveryState::Applying) {
        save(&jpath, &j)?;
        layout
            .open()
            .map_err(|_| DeliveryError::new("state_unavailable"))?
            .set_worktree_delivery(
                ctx.task.task_id,
                project.id(),
                WorktreeDeliveryState::Applying,
                jpath.to_str(),
                None,
            )
            .map_err(|_| DeliveryError::new("state_unavailable"))?;
        fault(hook, "after_journal")?;
    }
    for i in 0..j.operations.len() {
        let entry = &j.operations[i].entry;
        let obj = artifact::read_object(project.workspace(), &entry.path)?;
        if !entry.matches_artifact(obj.as_ref()) {
            if !entry.matches_base(obj.as_ref()) {
                return Err(DeliveryError::paths(
                    "needs_manual_recovery",
                    vec![entry.path.clone()],
                ));
            }
            bridge_artifact::materialize::execute(
                &ctx.runtime.join("artifact"),
                project.workspace(),
                entry,
            )?;
            fault(hook, &format!("after_op:{i}"))?;
        }
        // Always sync the directory again: an earlier rename may have reached
        // disk before its journal marker during a real process interruption.
        let parent = project
            .workspace()
            .join(&entry.path)
            .parent()
            .unwrap()
            .to_path_buf();
        if parent.exists() {
            artifact::sync_dir(&parent)?;
        }
        j.operations[i].applied = true;
        save(&jpath, &j)?;
        fault(hook, &format!("after_applied:{i}"))?;
    }
    fault(hook, "before_post_verify")?;
    post_verify(project, &artifact, &j)?;
    fault(hook, "after_post_verify")?;
    layout
        .open()
        .map_err(|_| DeliveryError::new("state_unavailable"))?
        .set_worktree_delivery(
            ctx.task.task_id,
            project.id(),
            WorktreeDeliveryState::Delivered,
            None,
            None,
        )
        .map_err(|_| DeliveryError::new("state_unavailable"))?;
    Ok(
        json!({"mode":"apply","status":"delivered","entries":artifact.entries,"base_head":artifact.base_head,"delivery_state":"delivered"}),
    )
}
