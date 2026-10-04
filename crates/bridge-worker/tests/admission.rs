use bridge_config::{ProjectEntry, load_config};
use bridge_domain::{ProjectId, TaskId, TaskStatus};
use bridge_storage::{AdmissionSettings, CreateTaskInput, RustStateLayout};
use bridge_worker::{
    WorkerLock, WorkerLockOutcome,
    admission::{ActivationOutcome, acquire_worker_fences, activate_direct_task},
};
use serde_json::json;
use std::{
    path::PathBuf,
    process::Command,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture {
    root: PathBuf,
    layout: RustStateLayout,
    project: ProjectEntry,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-admission-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let layout =
            RustStateLayout::new(root.join("state"), ProjectId::from_str("proj").unwrap()).unwrap();
        layout.initialize().unwrap();
        let config = root.join("projects.toml");
        std::fs::write(&config,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4999\"\npassword_file=\"unused\"\nmax_rounds=3\nmax_active_tasks=3\n",json!(root.join("workspace")))).unwrap();
        let project = load_config(&config)
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
        let f = Self {
            root,
            layout,
            project,
        };
        f.git(&["init", "-q"]);
        f.git(&["config", "user.name", "test"]);
        f.git(&["config", "user.email", "test@example.invalid"]);
        std::fs::write(f.project.workspace().join("allowed.txt"), "initial\n").unwrap();
        f.git(&["add", "."]);
        f.git(&["commit", "-qm", "initial"]);
        f
    }
    fn git(&self, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(self.project.workspace())
                .status()
                .unwrap()
                .success()
        );
    }
    fn create(&self, status: TaskStatus) -> TaskId {
        let task = TaskId::from_str(&uuid::Uuid::new_v4().to_string()).unwrap();
        let snapshot = bridge_git::take_snapshot(self.project.workspace())
            .unwrap()
            .to_json()
            .unwrap();
        self.layout
            .open()
            .unwrap()
            .create_task_with_admission(
                CreateTaskInput {
                    task_id: task,
                    project_id: self.project.id().clone(),
                    workspace: self.project.workspace().to_str().unwrap().into(),
                    task: "task".into(),
                    request_id: task.to_string(),
                    payload_hash: "legacy-hash".into(),
                    base_head: snapshot["head"].as_str().map(str::to_owned),
                    allowed_paths: vec!["allowed.txt".into()],
                    test_commands: vec![],
                    snapshot: Some(snapshot),
                },
                &AdmissionSettings::new(3, false, bridge_domain::ExecutionMode::Direct).unwrap(),
                status,
            )
            .unwrap();
        task
    }
    fn edge(&self, task: TaskId, dep: TaskId) {
        self.layout
            .open()
            .unwrap()
            .connection()
            .execute(
                "UPDATE tasks SET workflow_id='wf',depends_on=?1 WHERE task_id=?2",
                rusqlite::params![
                    json!([{"project_id":"proj","task_id":dep.to_string()}]).to_string(),
                    task.to_string()
                ],
            )
            .unwrap();
    }
    fn status(&self, task: TaskId) -> TaskStatus {
        self.layout
            .open()
            .unwrap()
            .get_task(task)
            .unwrap()
            .unwrap()
            .status
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn chain_waits_then_explicitly_activates_once_on_current_baseline() {
    let f = Fixture::new();
    let predecessor = f.create(TaskStatus::Implementing);
    let successor = f.create(TaskStatus::WaitingDependencies);
    f.edge(successor, predecessor);
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET workflow_id='wf' WHERE task_id=?1",
            [predecessor.to_string()],
        )
        .unwrap();
    assert_eq!(
        activate_direct_task(&f.layout, &f.project, successor).unwrap(),
        ActivationOutcome::Waiting
    );
    assert!(
        acquire_worker_fences(&f.layout, &f.project, successor)
            .unwrap()
            .is_none()
    );
    f.layout.initialize().unwrap();
    assert_eq!(f.status(successor), TaskStatus::WaitingDependencies);
    let mut storage = f.layout.open().unwrap();
    storage
        .update_task_status(
            predecessor,
            f.project.id(),
            TaskStatus::AwaitingReview,
            None,
        )
        .unwrap();
    storage
        .update_task_status(predecessor, f.project.id(), TaskStatus::Accepted, None)
        .unwrap();
    std::fs::write(f.project.workspace().join("allowed.txt"), "accepted\n").unwrap();
    assert_eq!(
        activate_direct_task(&f.layout, &f.project, successor).unwrap(),
        ActivationOutcome::Dirty
    );
    f.git(&["add", "."]);
    f.git(&["commit", "-qm", "predecessor"]);
    let before = storage.get_task(successor).unwrap().unwrap().base_head;
    assert_eq!(
        activate_direct_task(&f.layout, &f.project, successor).unwrap(),
        ActivationOutcome::Activated
    );
    let after = storage.get_task(successor).unwrap().unwrap();
    assert_ne!(before, after.base_head);
    assert_eq!(after.status, TaskStatus::Implementing);
    assert_eq!(
        activate_direct_task(&f.layout, &f.project, successor).unwrap(),
        ActivationOutcome::Unchanged
    );
    assert_eq!(storage.get_active_writers(f.project.id()).unwrap().len(), 1);
    let events: i64 = storage
        .connection()
        .query_row(
            "SELECT count(*) FROM events WHERE kind='dependencies_satisfied'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(events, 1);
    let request: String = storage
        .connection()
        .query_row(
            "SELECT payload_hash FROM rounds WHERE task_id=?1 AND round_number=1",
            [successor.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(request, "legacy-hash");
}
#[test]
fn worker_takes_both_fences_and_releases_partial_acquisition() {
    let f = Fixture::new();
    let task = f.create(TaskStatus::Implementing);
    let task_guard = WorkerLock::try_acquire_task(&f.layout, task).unwrap();
    assert!(
        acquire_worker_fences(&f.layout, &f.project, task)
            .unwrap()
            .is_none()
    );
    assert!(WorkerLock::is_free(&f.layout).unwrap());
    drop(task_guard);
    let guard = acquire_worker_fences(&f.layout, &f.project, task)
        .unwrap()
        .unwrap();
    assert!(WorkerLock::is_held(&f.layout).unwrap());
    assert!(matches!(
        WorkerLock::try_acquire_task(&f.layout, task).unwrap(),
        WorkerLockOutcome::Busy
    ));
    drop(guard);
    assert!(WorkerLock::is_free(&f.layout).unwrap());
}
#[test]
fn admission_contention_and_task_lock_prevent_activation() {
    let f = Fixture::new();
    let task = f.create(TaskStatus::WaitingDependencies);
    let admission = WorkerLock::try_acquire_admission(&f.layout).unwrap();
    assert_eq!(
        activate_direct_task(&f.layout, &f.project, task).unwrap(),
        ActivationOutcome::Busy
    );
    drop(admission);
    let lock = WorkerLock::try_acquire_task(&f.layout, task).unwrap();
    assert_eq!(
        activate_direct_task(&f.layout, &f.project, task).unwrap(),
        ActivationOutcome::Busy
    );
    drop(lock);
    assert_eq!(f.status(task), TaskStatus::WaitingDependencies);
}
#[test]
fn mismatched_workflow_missing_and_external_edges_stay_waiting() {
    let f = Fixture::new();
    let task = f.create(TaskStatus::WaitingDependencies);
    f.edge(
        task,
        TaskId::from_str(&uuid::Uuid::new_v4().to_string()).unwrap(),
    );
    assert_eq!(
        activate_direct_task(&f.layout, &f.project, task).unwrap(),
        ActivationOutcome::Waiting
    );
    let raw = json!([{"project_id":"foreign","task_id":"task-1"}]).to_string();
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET depends_on=?1 WHERE task_id=?2",
            rusqlite::params![raw, task.to_string()],
        )
        .unwrap();
    assert_eq!(
        activate_direct_task(&f.layout, &f.project, task).unwrap(),
        ActivationOutcome::Waiting
    );
    assert!(!f.layout.state_root().join("foreign").exists());
}
#[test]
fn dirty_scope_and_close_requests_refuse_without_activation() {
    let f = Fixture::new();
    let task = f.create(TaskStatus::WaitingDependencies);
    f.layout.open().unwrap().connection().execute("UPDATE tasks SET snapshot=json_set(snapshot,'$.allow_dirty',json('true')) WHERE task_id=?1",[task.to_string()]).unwrap();
    std::fs::write(f.project.workspace().join("outside.txt"), "dirty").unwrap();
    assert_eq!(
        activate_direct_task(&f.layout, &f.project, task).unwrap(),
        ActivationOutcome::OutsideScope
    );
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET close_requested_at='now' WHERE task_id=?1",
            [task.to_string()],
        )
        .unwrap();
    assert_eq!(
        activate_direct_task(&f.layout, &f.project, task).unwrap(),
        ActivationOutcome::CloseRequested
    );
    assert_eq!(f.status(task), TaskStatus::WaitingDependencies);
}
#[cfg(unix)]
#[test]
fn lock_artifact_symlinks_fail_closed_and_permissions_are_private() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let f = Fixture::new();
    let task = f.create(TaskStatus::Implementing);
    let target = f.root.join("sentinel");
    std::fs::write(&target, "untouched").unwrap();
    symlink(&target, f.layout.project_dir().join("admission.lock")).unwrap();
    assert!(WorkerLock::try_acquire_admission(&f.layout).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    std::fs::remove_file(f.layout.project_dir().join("admission.lock")).unwrap();
    let guard = WorkerLock::try_acquire_task(&f.layout, task).unwrap();
    let directory = f.layout.project_dir().join("workers");
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(directory.join(format!("{task}.lock")))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(guard);
}

#[test]
fn concurrent_explicit_activation_has_one_winner_and_never_replays() {
    let f = Fixture::new();
    let task = f.create(TaskStatus::WaitingDependencies);
    let barrier = std::sync::Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let run = || {
            barrier.wait();
            activate_direct_task(&f.layout, &f.project, task).unwrap()
        };
        let a = scope.spawn(run);
        let b = scope.spawn(run);
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == ActivationOutcome::Activated)
            .count(),
        1
    );
    assert_eq!(f.status(task), TaskStatus::Implementing);
}

#[test]
fn corrupt_edge_shapes_do_not_unlock_task() {
    let f = Fixture::new();
    let task = f.create(TaskStatus::WaitingDependencies);
    for raw in [
        "null",
        "{}",
        "[[\"proj\",\"task-1\"]]",
        "[{\"project_id\":\"proj\",\"task_id\":\"x\",\"extra\":true}]",
    ] {
        f.layout
            .open()
            .unwrap()
            .connection()
            .execute(
                "UPDATE tasks SET depends_on=?1 WHERE task_id=?2",
                rusqlite::params![raw, task.to_string()],
            )
            .unwrap();
        assert!(activate_direct_task(&f.layout, &f.project, task).is_err());
        assert_eq!(f.status(task), TaskStatus::WaitingDependencies);
    }
}
