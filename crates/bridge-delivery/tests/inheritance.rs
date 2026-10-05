mod common;
use bridge_domain::TaskId;
use bridge_storage::{CreateTaskInput, Task, WorktreeRegistration, WorktreeStatus};
use common::Fixture;
use serde_json::json;
use std::{fs, os::unix::fs::symlink, path::PathBuf};

fn child(f: &Fixture) -> (Task, PathBuf) {
    let mut storage = f.layout.open().unwrap();
    storage
        .connection()
        .execute(
            "UPDATE tasks SET workflow_id='workflow' WHERE task_id=?1",
            [f.id.to_string()],
        )
        .unwrap();
    let parent = storage.get_task(f.id).unwrap().unwrap();
    let id: TaskId = uuid::Uuid::new_v4().to_string().parse().unwrap();
    let source = bridge_git::take_snapshot(&f.checkout).unwrap();
    let mut snapshot = parent.snapshot.clone().unwrap();
    snapshot["automation_parent"] = json!(f.id);
    snapshot["automation_parent_fingerprint"] = bridge_artifact::fingerprint(&source);
    snapshot["automation_run_id"] = json!("workflow");
    storage
        .create_task(CreateTaskInput {
            task_id: id,
            project_id: f.project.id().clone(),
            workspace: parent.workspace.clone(),
            task: "child".into(),
            request_id: id.to_string(),
            payload_hash: "hash".into(),
            base_head: parent.base_head.clone(),
            allowed_paths: vec!["src/".into()],
            test_commands: vec![],
            snapshot: Some(snapshot),
        })
        .unwrap();
    storage
        .connection()
        .execute(
            "UPDATE tasks SET workflow_id='workflow',execution_mode='worktree' WHERE task_id=?1",
            [id.to_string()],
        )
        .unwrap();
    let binding = bridge_git::checkout::create_checkout(
        f.project.workspace(),
        &f.layout.project_dir(),
        id,
        parent.base_head.as_deref().unwrap(),
    )
    .unwrap();
    storage
        .register_worktree(
            id,
            f.project.id(),
            binding.paths.checkout.to_str().unwrap(),
            &WorktreeRegistration {
                runtime_dir: Some(binding.paths.runtime_dir.to_str().unwrap().into()),
                base_head: parent.base_head,
                status: Some(WorktreeStatus::Creating),
                ..Default::default()
            },
        )
        .unwrap();
    (
        storage.get_task(id).unwrap().unwrap(),
        binding.paths.checkout,
    )
}
#[test]
fn inherits_complete_accepted_bytes_and_builds_cumulative_artifact() {
    let f = Fixture::new();
    f.changes();
    bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
    let (task, target) = child(&f);
    let mut storage = f.layout.open().unwrap();
    bridge_worker::inheritance::inherit_checkout(&storage, &f.layout, &task, &target).unwrap();
    let baseline = bridge_git::take_snapshot(&target).unwrap();
    assert_eq!(
        baseline.manifest(),
        bridge_git::take_snapshot(&f.checkout).unwrap().manifest()
    );
    storage
        .update_worktree_baseline(
            task.task_id,
            f.project.id(),
            &baseline.to_json().unwrap().to_string(),
            task.base_head.as_deref(),
        )
        .unwrap();
    storage
        .update_worktree_status(task.task_id, f.project.id(), WorktreeStatus::Created, None)
        .unwrap();
    fs::write(target.join("src/later"), "second step").unwrap();
    storage
        .connection()
        .execute(
            "UPDATE tasks SET status='accepted' WHERE task_id=?1",
            [task.task_id.to_string()],
        )
        .unwrap();
    storage
        .connection()
        .execute("DELETE FROM active_writers", [])
        .unwrap();
    bridge_delivery::build(&f.layout, &f.project, task.task_id).unwrap();
    let artifact =
        bridge_delivery::load_artifact(&target.parent().unwrap().join("runtime/artifact")).unwrap();
    assert_eq!(artifact.entries.len(), 6);
    assert!(artifact.entries.iter().any(|e| e.path == "src/later"));
    let compared = bridge_git::compare_repository_snapshot(
        &target,
        &baseline,
        &["src/later".into()],
        false,
        false,
    )
    .unwrap();
    assert!(compared.scope_violations().is_empty());
    assert_eq!(compared.changed_paths().len(), 1);
    assert!(bridge_worker::prompt::initial_prompt(&task, &target).contains("унаследованные"));
}
#[test]
fn rejects_parent_scope_artifact_and_target_drift_before_inherited_writes() {
    for mode in [
        "parent",
        "workflow",
        "scope",
        "artifact",
        "blob",
        "fingerprint",
        "target",
        "symlink",
    ] {
        let f = Fixture::new();
        f.changes();
        bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
        let (mut task, target) = child(&f);
        let storage = f.layout.open().unwrap();
        match mode {
            "parent" => {
                storage
                    .connection()
                    .execute(
                        "UPDATE tasks SET status='awaiting_review' WHERE task_id=?1",
                        [f.id.to_string()],
                    )
                    .unwrap();
            }
            "workflow" => {
                storage
                    .connection()
                    .execute(
                        "UPDATE tasks SET workflow_id='other' WHERE task_id=?1",
                        [task.task_id.to_string()],
                    )
                    .unwrap();
            }
            "scope" => task.allowed_paths = vec!["src/a".into()],
            "artifact" => {
                let mut artifact = bridge_delivery::load_artifact(&f.dest()).unwrap();
                artifact.task_id = task.task_id;
                fs::write(
                    f.dest().join("manifest.json"),
                    serde_json::to_vec(&artifact).unwrap(),
                )
                .unwrap();
            }
            "blob" => {
                let artifact = bridge_delivery::load_artifact(&f.dest()).unwrap();
                let hash = artifact
                    .entries
                    .iter()
                    .find_map(|e| e.blob_sha256.as_ref())
                    .unwrap();
                fs::write(f.dest().join("blobs").join(hash), "tampered").unwrap();
            }
            "fingerprint" => fs::write(f.checkout.join("src/a"), "drift").unwrap(),
            "target" => fs::write(target.join("src/a"), "conflict").unwrap(),
            "symlink" => {
                fs::remove_dir_all(target.join("src")).unwrap();
                symlink(f.project.workspace().join("src"), target.join("src")).unwrap();
            }
            _ => unreachable!(),
        }
        let before = bridge_git::take_snapshot(&target)
            .unwrap()
            .to_json()
            .unwrap();
        assert!(
            bridge_worker::inheritance::inherit_checkout(&storage, &f.layout, &task, &target)
                .is_err(),
            "{mode}"
        );
        assert_eq!(
            bridge_git::take_snapshot(&target)
                .unwrap()
                .to_json()
                .unwrap(),
            before,
            "{mode}"
        );
    }
}
