use bridge_config::{ProjectEntry, load_config_with_state_root};
use bridge_domain::TaskId;
use bridge_storage::{CreateTaskInput, RustStateLayout, WorktreeRegistration, WorktreeStatus};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    process::Command,
};
struct Fixture {
    root: PathBuf,
    project: ProjectEntry,
    layout: RustStateLayout,
    id: TaskId,
    checkout: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bridge-delivery-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let main = root.join("main");
        fs::create_dir(&main).unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&main)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "fixture"]);
        git(&["config", "user.email", "fixture@example.test"]);
        fs::create_dir(main.join("src")).unwrap();
        fs::write(main.join("src/a"), b"base\0binary").unwrap();
        fs::write(main.join("src/remove"), "remove").unwrap();
        fs::write(main.join("src/executable"), "exec").unwrap();
        symlink("a", main.join("src/link")).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        fs::write(root.join("password"), "fixture-secret").unwrap();
        fs::set_permissions(root.join("password"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(root.join("projects.toml"),format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:9000\"\npassword_file={}\nmax_rounds=3\nexecution_mode=\"worktree\"\n",json!(main),json!(root.join("password")))).unwrap();
        let project = load_config_with_state_root(&root.join("projects.toml"), &root.join("state"))
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
        let layout = RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
        layout.initialize().unwrap();
        let id = uuid::Uuid::new_v4().to_string().parse().unwrap();
        let baseline = bridge_git::take_snapshot(&main).unwrap();
        let base = baseline.head().unwrap().as_str().to_owned();
        let mut storage = layout.open().unwrap();
        storage
            .create_task(CreateTaskInput {
                task_id: id,
                project_id: project.id().clone(),
                workspace: main.to_str().unwrap().into(),
                task: "change".into(),
                request_id: "request".into(),
                payload_hash: "hash".into(),
                base_head: Some(base.clone()),
                allowed_paths: vec!["src/".into()],
                test_commands: vec![],
                snapshot: Some(baseline.to_json().unwrap()),
            })
            .unwrap();
        storage.connection().execute_batch("UPDATE tasks SET status='accepted',execution_mode='worktree';DELETE FROM active_writers;").unwrap();
        let binding =
            bridge_git::checkout::create_checkout(&main, &layout.project_dir(), id, &base).unwrap();
        storage
            .register_worktree(
                id,
                project.id(),
                binding.paths.checkout.to_str().unwrap(),
                &WorktreeRegistration {
                    runtime_dir: Some(binding.paths.runtime_dir.to_str().unwrap().into()),
                    base_head: Some(base),
                    baseline_json: Some(baseline.to_json().unwrap().to_string()),
                    status: Some(WorktreeStatus::Created),
                    ..Default::default()
                },
            )
            .unwrap();
        Self {
            root,
            project,
            layout,
            id,
            checkout: binding.paths.checkout,
        }
    }
    fn changes(&self) {
        fs::write(self.checkout.join("src/a"), b"result\xff\0").unwrap();
        fs::remove_file(self.checkout.join("src/remove")).unwrap();
        fs::write(self.checkout.join("src/new"), [0, 255, 10, 20]).unwrap();
        fs::set_permissions(
            self.checkout.join("src/executable"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::remove_file(self.checkout.join("src/link")).unwrap();
        symlink("new", self.checkout.join("src/link")).unwrap();
    }
    fn dest(&self) -> PathBuf {
        self.checkout.parent().unwrap().join("runtime/artifact")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn byte_complete_artifact_supports_binary_delete_modes_and_symlink() {
    let f = Fixture::new();
    f.changes();
    let before = bridge_git::take_snapshot(f.project.workspace())
        .unwrap()
        .to_json()
        .unwrap();
    let report = bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
    assert_eq!(report["status"], "built");
    let artifact = bridge_delivery::load_artifact(&f.dest()).unwrap();
    assert_eq!(artifact.entries.len(), 5);
    for e in &artifact.entries {
        if let Some(hash) = &e.blob_sha256 {
            assert_eq!(
                Some(fs::read(f.dest().join("blobs").join(hash)).unwrap().len() as u64),
                e.size
            );
        }
    }
    assert!(artifact.entries.iter().any(|e| e.op == "mode_change"));
    assert!(artifact.entries.iter().any(|e| e.op == "delete"));
    assert!(artifact.entries.iter().any(|e| e.kind == "symlink"));
    assert_eq!(
        bridge_delivery::dry_run(&f.layout, &f.project, f.id).unwrap()["status"],
        "validated"
    );
    assert_eq!(
        bridge_git::take_snapshot(f.project.workspace())
            .unwrap()
            .to_json()
            .unwrap(),
        before
    );
}
#[test]
fn preflight_refuses_dirty_drift_tamper_and_outside_scope_without_target_writes() {
    let f = Fixture::new();
    f.changes();
    bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
    fs::write(f.project.workspace().join("outside"), "dirty").unwrap();
    assert_eq!(
        bridge_delivery::dry_run(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "dirty_main_workspace"
    );
    fs::remove_file(f.project.workspace().join("outside")).unwrap();
    fs::write(f.checkout.join("src/a"), "drift").unwrap();
    assert_eq!(
        bridge_delivery::dry_run(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "artifact_drift"
    );
    f.changes_without_link();
    bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
    let artifact = bridge_delivery::load_artifact(&f.dest()).unwrap();
    let hash = artifact
        .entries
        .iter()
        .find_map(|e| e.blob_sha256.as_ref())
        .unwrap();
    fs::write(f.dest().join("blobs").join(hash), "corrupt").unwrap();
    assert_eq!(
        bridge_delivery::dry_run(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "artifact_corrupt"
    );
    fs::write(f.checkout.join("outside"), "outside scope").unwrap();
    assert_eq!(
        bridge_delivery::build(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "out_of_scope_changes"
    );
    assert_eq!(
        fs::read(f.project.workspace().join("src/a")).unwrap(),
        b"base\0binary"
    );
}
impl Fixture {
    fn changes_without_link(&self) {
        fs::write(self.checkout.join("src/a"), b"result\xff\0").unwrap();
    }
}
#[test]
fn accepted_guard_and_writer_reservation_gate_precede_artifact_writes() {
    let f = Fixture::new();
    f.changes();
    let s = f.layout.open().unwrap();
    s.connection()
        .execute("UPDATE tasks SET status='awaiting_review'", [])
        .unwrap();
    assert_eq!(
        bridge_delivery::build(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "not_accepted"
    );
    assert!(!f.dest().exists());
    s.connection()
        .execute("UPDATE tasks SET status='accepted'", [])
        .unwrap();
    s.connection().execute("INSERT INTO active_writers(task_id,project_id,scopes_json,created_at,parallel) VALUES (?1,'proj','[\"src/\"]','stamp',1)",[f.id.to_string()]).unwrap();
    assert_eq!(
        bridge_delivery::build(&f.layout, &f.project, f.id)
            .unwrap_err()
            .code,
        "active_writers"
    );
    assert!(!f.dest().exists());
}
