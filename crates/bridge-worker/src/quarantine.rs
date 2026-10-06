//! Explicit snapshot-confirmed orphan relocation and later purge.
use crate::{WorkerLock, WorkerLockOutcome};
use bridge_config::ProjectEntry;
use bridge_domain::TaskId;
use bridge_git::checkout::{move_orphan, probe_orphan, registrations, remove_orphan};
use bridge_storage::{
    RustStateLayout, WorktreeQuarantineEntry, WorktreeQuarantineStatus as Status,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
fn safe(path: &Path) -> Result<(), &'static str> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err("path_unsafe");
        }
        prefix.push(component);
        match fs::symlink_metadata(&prefix) {
            Ok(m) if m.file_type().is_symlink() => return Err("path_symlink"),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("path_unavailable"),
        }
    }
    Ok(())
}
fn identity(path: &Path) -> Value {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            json!({"device":m.dev(),"inode":m.ino(),"directory":m.is_dir(),"symlink":m.file_type().is_symlink()})
        }
        Err(_) => Value::Null,
    }
}
fn record_safe(path: &Path) -> Result<(), &'static str> {
    safe(path)?;
    let bytes = match fs::read(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("process_unverifiable"),
    };
    if bytes.len() > 16384 {
        return Err("process_unverifiable");
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "process_unverifiable")?;
    let pid = value["pid"]
        .as_i64()
        .filter(|v| *v > 1)
        .ok_or("process_unverifiable")?;
    let expected = value["start"].as_str().ok_or("process_unverifiable")?;
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(raw) => {
            let fields = raw
                .rsplit_once(") ")
                .ok_or("process_unverifiable")?
                .1
                .split_whitespace()
                .collect::<Vec<_>>();
            if fields.get(19) == Some(&expected) && !matches!(fields.first(), Some(&"Z" | &"X")) {
                return Err("process_live");
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("process_unverifiable"),
    }
    Ok(())
}
struct Candidate {
    entry: WorktreeQuarantineEntry,
    slot: PathBuf,
    dest: PathBuf,
    document: Value,
}
fn collect(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    purge: bool,
) -> Result<Vec<Candidate>, &'static str> {
    bridge_runtime::project::validate(project, layout).map_err(|_| "state_binding")?;
    let storage = layout.open_readonly().map_err(|_| "state_unowned")?;
    let rows = storage
        .list_worktree_quarantine()
        .map_err(|_| "registry_unavailable")?;
    let registered = registrations(project.workspace()).map_err(|_| "registration_unverifiable")?;
    let mut result = vec![];
    for entry in rows {
        if entry.status == Status::Removed || purge != (entry.status == Status::Moved) {
            continue;
        }
        let slot = layout
            .project_dir()
            .join("worktrees")
            .join(entry.entry_id.as_str());
        let dest = layout
            .project_dir()
            .join("quarantine")
            .join(entry.entry_id.as_str());
        let mut blockers = vec![];
        if Path::new(&entry.original_path) != slot
            || entry
                .quarantined_path
                .as_ref()
                .is_some_and(|p| Path::new(p) != dest)
        {
            blockers.push("registry_path_mismatch");
        }
        for path in [&slot, &dest] {
            if let Err(e) = safe(path) {
                blockers.push(e);
            }
            if path.exists() && !path.is_dir() {
                blockers.push("path_not_directory");
            }
            if let Err(e) = record_safe(&path.join("runtime/opencode.process.json")) {
                blockers.push(e);
            }
        }
        let owned:bool=storage.connection().query_row("SELECT EXISTS(SELECT 1 FROM worktrees WHERE task_id=?1) OR EXISTS(SELECT 1 FROM tasks WHERE task_id=?1 AND status NOT IN ('accepted','closed','failed')) OR EXISTS(SELECT 1 FROM active_writers WHERE task_id=?1)",[entry.entry_id.as_str()],|r|r.get(0)).map_err(|_|"ownership_unverifiable")?;
        if owned {
            blockers.push("active_ownership");
        }
        if purge && slot.exists() {
            blockers.push("original_still_present");
        }
        if !purge && !slot.exists() && !dest.exists() {
            blockers.push("data_missing");
        }
        if let Ok(m) = fs::metadata(&slot)
            && m.dev()
                != fs::metadata(layout.project_dir())
                    .map_err(|_| "filesystem_unverifiable")?
                    .dev()
        {
            blockers.push("cross_filesystem");
        }
        for parent in [&slot, &dest] {
            let checkout = parent.join("checkout");
            if registered.contains(&checkout) {
                if probe_orphan(project.workspace(), &checkout).is_err() {
                    blockers.push("repo_identity_mismatch");
                }
            } else if checkout.join(".git").exists() {
                blockers.push("registration_inconsistent");
            }
        }
        if slot.exists() && dest.exists() {
            for e in fs::read_dir(&slot).map_err(|_| "path_unavailable")? {
                let e = e.map_err(|_| "path_unavailable")?;
                if dest.join(e.file_name()).exists() {
                    blockers.push("ambiguous_copies");
                }
            }
        }
        blockers.sort();
        blockers.dedup();
        let document = json!({"entry_id":entry.entry_id.as_str(),"status":entry.status.as_str(),"original_path":entry.original_path,"quarantined_path":entry.quarantined_path,"reason":entry.reason,"found_at":entry.found_at,"action":if purge{"remove"}else{"move"},"original_identity":identity(&slot),"destination_identity":identity(&dest),"registered":registered.contains(&slot.join("checkout"))||registered.contains(&dest.join("checkout")),"blockers":blockers,"eligible":blockers.is_empty()});
        result.push(Candidate {
            entry,
            slot,
            dest,
            document,
        });
    }
    Ok(result)
}
fn snapshot(rows: &[Candidate]) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&rows.iter().map(|c| &c.document).collect::<Vec<_>>()).unwrap()
        )
    )
}
pub fn preview(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    purge: bool,
) -> Result<Value, &'static str> {
    if !layout.database().exists() {
        return Ok(json!({"snapshot":format!("{:x}",Sha256::digest(b"[]")),"candidates":[]}));
    }
    let rows = collect(project, layout, purge)?;
    Ok(
        json!({"snapshot":snapshot(&rows),"candidates":rows.iter().map(|c|&c.document).collect::<Vec<_>>()}),
    )
}
fn lock(outcome: WorkerLockOutcome) -> Result<WorkerLock, &'static str> {
    match outcome {
        WorkerLockOutcome::Acquired(g) => Ok(g),
        WorkerLockOutcome::Busy => Err("worker_lock_busy"),
    }
}
/// Caller supplies the exact reviewed IDs or all:<snapshot>; bare all is refused.
/// Filesystem effects precede durable transitions and resume from exact slots.
pub fn apply(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    purge: bool,
    confirm: &str,
) -> Result<Value, &'static str> {
    if confirm.is_empty() || confirm == "all" {
        return Err("exact_confirmation_required");
    }
    let _worker = lock(WorkerLock::try_acquire(layout).map_err(|_| "worker_lock_unavailable")?)?;
    let rows = collect(project, layout, purge)?;
    let selected = if let Some(digest) = confirm.strip_prefix("all:") {
        if digest != snapshot(&rows) {
            return Err("snapshot_changed");
        }
        rows.iter()
            .map(|r| r.entry.entry_id.as_str())
            .collect::<Vec<_>>()
    } else {
        confirm.split(',').collect()
    };
    if selected.is_empty()
        || selected
            .iter()
            .any(|id| !rows.iter().any(|r| r.entry.entry_id.as_str() == *id))
    {
        return Err("unknown_confirmation_id");
    }
    let mut completed = vec![];
    for c in rows
        .iter()
        .filter(|r| selected.contains(&r.entry.entry_id.as_str()))
    {
        let _task = if let Ok(task) = c.entry.entry_id.as_str().parse::<TaskId>() {
            Some(lock(
                WorkerLock::try_acquire_task(layout, task).map_err(|_| "task_lock_unavailable")?,
            )?)
        } else {
            None
        };
        let _admission =
            lock(WorkerLock::try_acquire_admission(layout).map_err(|_| "admission_unavailable")?)?;
        let fresh = collect(project, layout, purge)?;
        let current = fresh
            .iter()
            .find(|r| r.entry.entry_id == c.entry.entry_id)
            .ok_or("registry_changed")?;
        if current.document != c.document {
            return Err("candidate_changed");
        }
        if c.document["eligible"] != true {
            return Err("candidate_blocked");
        }
        safe(&c.slot)?;
        safe(&c.dest)?;
        if purge {
            if c.dest.exists() {
                let checkout = c.dest.join("checkout");
                if registrations(project.workspace())
                    .map_err(|_| "registration_unverifiable")?
                    .contains(&checkout)
                {
                    remove_orphan(project.workspace(), &checkout)
                        .map_err(|_| "checkout_remove_failed")?;
                }
                fs::remove_dir_all(&c.dest).map_err(|_| "quarantine_remove_failed")?;
            }
        } else {
            let parent = c.dest.parent().ok_or("path_unsafe")?;
            fs::create_dir_all(parent).map_err(|_| "quarantine_create_failed")?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .map_err(|_| "quarantine_mode_failed")?;
            if c.slot.exists() {
                if registrations(project.workspace())
                    .map_err(|_| "registration_unverifiable")?
                    .contains(&c.slot.join("checkout"))
                {
                    fs::create_dir_all(&c.dest).map_err(|_| "quarantine_create_failed")?;
                    move_orphan(
                        project.workspace(),
                        &c.slot.join("checkout"),
                        &c.dest.join("checkout"),
                    )
                    .map_err(|_| "checkout_move_failed")?;
                }
                if !c.dest.exists() {
                    fs::rename(&c.slot, &c.dest).map_err(|_| "quarantine_move_failed")?;
                } else {
                    for e in fs::read_dir(&c.slot).map_err(|_| "quarantine_move_failed")? {
                        let e = e.map_err(|_| "quarantine_move_failed")?;
                        let target = c.dest.join(e.file_name());
                        if target.exists() {
                            return Err("ambiguous_copies");
                        }
                        fs::rename(e.path(), target).map_err(|_| "quarantine_move_failed")?;
                    }
                    fs::remove_dir(&c.slot).map_err(|_| "quarantine_move_failed")?;
                }
            }
            fs::set_permissions(&c.dest, fs::Permissions::from_mode(0o700))
                .map_err(|_| "quarantine_mode_failed")?;
        }
        for parent in [c.slot.parent(), c.dest.parent()].into_iter().flatten() {
            fs::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|_| "quarantine_sync_failed")?;
        }
        let mut storage = layout.open().map_err(|_| "state_unowned")?;
        storage
            .transition_worktree_quarantine(
                &c.entry.entry_id,
                if purge {
                    Status::Removed
                } else {
                    Status::Moved
                },
                c.dest.to_str(),
                Some(c.entry.status),
                Some(&c.entry.original_path),
            )
            .map_err(|_| "registry_transition_failed")?;
        completed.push(c.entry.entry_id.as_str());
    }
    Ok(json!({"completed":completed,"action":if purge{"remove"}else{"move"}}))
}
