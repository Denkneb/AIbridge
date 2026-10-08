use bridge_config::{ProjectEntry, load_config};
use bridge_domain::{ExecutionMode, ProjectId, TaskId, TaskStatus};
use bridge_runtime::{
    RuntimeError, RuntimeOptions, ServerCommand, ServerState, lock::ManagerLock,
    start_worktree_server, stop_worktree_server, worktree_server_state,
};
use bridge_storage::{
    AdmissionSettings, CreateTaskInput, RustStateLayout, WorktreeRegistration, WorktreeStatus,
};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
fn network_fence() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
struct Fixture {
    root: PathBuf,
    layout: RustStateLayout,
    project: ProjectEntry,
    task: TaskId,
    checkout: PathBuf,
    runtime: PathBuf,
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        std::fs::write(main.join("file"), "base\n").unwrap();
        git(&main, &["add", "file"]);
        git(&main, &["commit", "-qm", "fixture"]);
        let base = git(&main, &["rev-parse", "HEAD"]);
        let password = root.join("password");
        std::fs::write(&password, "fixture-password\n").unwrap();
        private(&password);
        let path = root.join("projects.toml");
        std::fs::write(&path,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4999\"\npassword_file={}\nmax_rounds=3\nexecution_mode=\"worktree\"\n",serde_json::json!(main.to_str().unwrap()),serde_json::json!(password.to_str().unwrap()))).unwrap();
        let project = load_config(&path).unwrap().project("proj").unwrap().clone();
        let layout = RustStateLayout::new(
            root.join("state"),
            ProjectId::try_from("proj".to_owned()).unwrap(),
        )
        .unwrap();
        layout.initialize().unwrap();
        let task: TaskId = "11111111-1111-4111-8111-111111111111".parse().unwrap();
        let mut storage = layout.open().unwrap();
        storage
            .create_task_with_admission(
                CreateTaskInput {
                    task_id: task,
                    project_id: project.id().clone(),
                    workspace: main.to_str().unwrap().into(),
                    task: "fixture".into(),
                    request_id: "request".into(),
                    payload_hash: "hash".into(),
                    base_head: Some(base.clone()),
                    allowed_paths: vec!["file".into()],
                    test_commands: vec![],
                    snapshot: None,
                },
                &AdmissionSettings::new(1, false, ExecutionMode::Worktree).unwrap(),
                TaskStatus::Implementing,
            )
            .unwrap();
        let binding =
            bridge_git::checkout::create_checkout(&main, &layout.project_dir(), task, &base)
                .unwrap();
        let checkout = binding.paths.checkout;
        let runtime = binding.paths.runtime_dir;
        storage
            .register_worktree(
                task,
                project.id(),
                checkout.to_str().unwrap(),
                &WorktreeRegistration {
                    runtime_dir: Some(runtime.to_str().unwrap().into()),
                    base_head: Some(base),
                    status: Some(WorktreeStatus::Created),
                    ..Default::default()
                },
            )
            .unwrap();
        Self {
            root,
            layout,
            project,
            task,
            checkout,
            runtime,
        }
    }
    fn start(&self, mode: &str) -> Result<bridge_runtime::WorktreeServer, RuntimeError> {
        let cmd = ServerCommand::executable(
            Path::new(env!("CARGO_BIN_EXE_worktree_server_fixture")),
            vec![mode.into()],
        )
        .unwrap();
        start_worktree_server(
            &self.layout,
            &self.project,
            self.task,
            &[],
            &[],
            &cmd,
            RuntimeOptions {
                lock_wait: Duration::from_secs(1),
                ready_timeout: Duration::from_millis(500),
                request_timeout: Duration::from_millis(50),
            },
        )
    }
    fn stop(&self) -> Result<bool, RuntimeError> {
        stop_worktree_server(&self.layout, &self.project, self.task, &[])
    }
    fn record(&self) -> PathBuf {
        self.runtime.join("opencode.process.json")
    }
    fn change_record(&self, mutate: impl FnOnce(&mut serde_json::Value)) -> String {
        let old = std::fs::read_to_string(self.record()).unwrap();
        let mut value = serde_json::from_str(&old).unwrap();
        mutate(&mut value);
        std::fs::write(self.record(), value.to_string()).unwrap();
        old
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.stop();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[test]
fn start_reuse_strict_root_and_idempotent_stop_preserve_checkout() {
    let _network = network_fence();
    let f = Fixture::new();
    let before = bridge_git::take_snapshot(&f.checkout).unwrap();
    std::fs::write(f.runtime.join("server.log"), "existing log\n").unwrap();
    let first = f.start("serve").unwrap();
    assert!(!first.reused);
    assert_eq!(first.client.workspace(), f.checkout);
    let second = f.start("serve").unwrap();
    assert!(second.reused);
    assert_eq!(first.port, second.port);
    assert_eq!(
        worktree_server_state(&f.layout, &f.project, f.task).unwrap(),
        ServerState::Live
    );
    assert_eq!(bridge_git::take_snapshot(&f.checkout).unwrap(), before);
    assert!(f.runtime.join("server.log").exists());
    assert!(!f.checkout.join("server.log").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(f.runtime.join("server.log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(f.record()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(f.stop().unwrap());
    assert!(!f.stop().unwrap());
    assert_eq!(
        worktree_server_state(&f.layout, &f.project, f.task).unwrap(),
        ServerState::Missing
    );
    assert!(
        !std::fs::read_to_string(f.runtime.join("server.log"))
            .unwrap()
            .contains("fixture-password")
    );
}
#[test]
fn readiness_failures_kill_fresh_process_and_release_record_and_port() {
    let _network = network_fence();
    for mode in ["bad-path", "bad-doc", "unhealthy", "stall", "exit"] {
        let f = Fixture::new();
        assert!(f.start(mode).is_err());
        assert!(!f.record().exists());
        if let Ok(pid) = std::fs::read_to_string(f.runtime.join("fixture.pid")) {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            assert!(stat.is_err(), "fresh child must be reaped");
        }
        assert!(f.start("serve").is_ok());
        assert!(f.stop().unwrap());
    }
}
#[test]
fn live_foreign_record_is_never_signalled_or_replaced() {
    let _network = network_fence();
    let f = Fixture::new();
    let _server = f.start("serve").unwrap();
    let old = f.change_record(|v| v["project_id"] = serde_json::json!("other"));
    assert_eq!(f.stop(), Err(RuntimeError::ForeignProcess));
    assert_eq!(f.start("serve").unwrap_err(), RuntimeError::ForeignProcess);
    assert_eq!(
        worktree_server_state(&f.layout, &f.project, f.task),
        Err(RuntimeError::ForeignProcess)
    );
    std::fs::write(f.record(), old).unwrap();
    assert!(f.stop().unwrap());
}
#[test]
fn stale_record_diagnosis_is_readonly_then_start_replaces_without_signalling_old_pid() {
    let _network = network_fence();
    let f = Fixture::new();
    f.start("serve").unwrap();
    let old = f.change_record(|v| v["start"] = serde_json::json!("0"));
    let stale = std::fs::read_to_string(f.record()).unwrap();
    assert_eq!(
        worktree_server_state(&f.layout, &f.project, f.task).unwrap(),
        ServerState::Stale
    );
    assert_eq!(std::fs::read_to_string(f.record()).unwrap(), stale);
    // Restore and stop the known fixture to avoid deliberately orphaning it.
    std::fs::write(f.record(), old).unwrap();
    f.stop().unwrap();
    std::fs::write(f.record(), stale).unwrap();
    private(&f.record());
    let server = f.start("serve").unwrap();
    assert!(!server.reused);
    f.stop().unwrap();
}
#[test]
fn corrupt_record_and_symlinked_log_fail_without_overwriting() {
    let _network = network_fence();
    let f = Fixture::new();
    std::fs::write(f.record(), "bad-json").unwrap();
    private(&f.record());
    assert_eq!(f.start("serve").unwrap_err(), RuntimeError::Record);
    assert_eq!(std::fs::read_to_string(f.record()).unwrap(), "bad-json");
    std::fs::remove_file(f.record()).unwrap();
    #[cfg(unix)]
    {
        let outside = f.root.join("outside.log");
        std::fs::write(&outside, "sentinel").unwrap();
        std::os::unix::fs::symlink(&outside, f.runtime.join("server.log")).unwrap();
        assert!(f.start("serve").is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "sentinel");
    }
}
#[test]
fn lock_wait_is_bounded_default_nonblocking_and_timeout_has_no_spawn_side_effects() {
    let _network = network_fence();
    let f = Fixture::new();
    let lock = ManagerLock::acquire(&[&f.layout], Duration::ZERO).unwrap();
    let at = Instant::now();
    assert!(matches!(
        ManagerLock::acquire(&[&f.layout], Duration::ZERO),
        Err(RuntimeError::LockBusy)
    ));
    assert!(at.elapsed() < Duration::from_millis(200));
    assert_eq!(f.start("serve").unwrap_err(), RuntimeError::LockTimeout);
    assert!(!f.record().exists());
    assert!(!f.runtime.join("server.log").exists());
    drop(lock);
    assert!(f.start("serve").is_ok());
    f.stop().unwrap();
}
#[test]
fn lock_wait_succeeds_after_release_and_partial_acquisition_releases_every_root() {
    let _network = network_fence();
    let a = Fixture::new();
    let b = Fixture::new();
    let (first, last) = if a.layout.state_root() < b.layout.state_root() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    let held = ManagerLock::acquire(&[&last.layout], Duration::ZERO).unwrap();
    assert!(matches!(
        ManagerLock::acquire(&[&first.layout, &last.layout], Duration::from_millis(40)),
        Err(RuntimeError::LockTimeout)
    ));
    let independent = ManagerLock::acquire(&[&first.layout], Duration::ZERO).unwrap();
    drop(independent);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        drop(held);
    });
    let _both = ManagerLock::acquire(
        &[&last.layout, &first.layout, &first.layout],
        Duration::from_secs(1),
    )
    .unwrap();
}
#[test]
fn concurrent_start_reuses_one_server_under_manager_lock() {
    let _network = network_fence();
    let f = Fixture::new();
    let barrier = std::sync::Barrier::new(2);
    let handles = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            f.start("serve").unwrap()
        });
        let b = scope.spawn(|| {
            barrier.wait();
            f.start("serve").unwrap()
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(handles.0.port, handles.1.port);
    assert_ne!(handles.0.reused, handles.1.reused);
    f.stop().unwrap();
}

#[test]
fn configured_ports_and_live_record_reservations_are_skipped_before_bind() {
    let _network = network_fence();
    let mut f = Fixture::new();
    f.project = f
        .project
        .execution_view(
            f.project.workspace(),
            bridge_config::Endpoint::loopback(43000).unwrap(),
        )
        .unwrap();
    let reserved = f
        .layout
        .state_root()
        .join("reserved/worktrees/reservation/runtime");
    std::fs::create_dir_all(&reserved).unwrap();
    let pid = std::process::id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap();
    let path = reserved.join("opencode.process.json");
    std::fs::write(&path,serde_json::json!({"pid":pid,"start":start,"boot_id":std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),"project_id":"reserved","task_id":f.task,"checkout":f.checkout,"kind":"worktree","port":43001,"endpoint":"http://127.0.0.1:43001"}).to_string()).unwrap();
    private(&path);
    let server = f.start("serve").unwrap();
    assert_ne!(server.port, 43000);
    assert_ne!(server.port, 43001);
    assert!(f.stop().unwrap());
}
#[test]
fn pinned_model_requires_openapi_model_even_after_config_removes_its_model() {
    let _network = network_fence();
    let f = Fixture::new();
    let mut snapshot = f.project.profile_snapshot(None).unwrap();
    snapshot.model = Some("frozen/model".into());
    let raw = snapshot.canonical_json().unwrap();
    let hash = snapshot.canonical_hash().unwrap();
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET profile=?1,profile_json=?2,profile_hash=?3,profile_source=?4",
            [
                snapshot.id.as_str(),
                raw.as_str(),
                hash.as_str(),
                snapshot.origin.as_str(),
            ],
        )
        .unwrap();
    assert_eq!(f.start("no-model").unwrap_err(), RuntimeError::Readiness);
    assert!(!f.record().exists());
    assert!(f.start("serve").is_ok());
    f.stop().unwrap();
}

fn pending_execution_fixture() -> Fixture {
    let f = Fixture::new();
    bridge_git::checkout::remove_checkout(
        f.project.workspace(),
        &f.layout.project_dir(),
        f.task,
        &f.checkout,
    )
    .unwrap();
    let snapshot = bridge_git::take_snapshot(f.project.workspace())
        .unwrap()
        .to_json()
        .unwrap();
    f.layout.open().unwrap().connection().execute_batch("UPDATE worktrees SET status='pending'; UPDATE tasks SET test_commands='[\"grep -q executor file\"]';").unwrap();
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute("UPDATE tasks SET snapshot=?1", [snapshot.to_string()])
        .unwrap();
    f
}
fn prepare_fixture(
    f: &Fixture,
    number: u32,
) -> Result<bridge_worker::execution::RoundExecution, bridge_worker::execution::ExecutionError> {
    let command = ServerCommand::executable(
        Path::new(env!("CARGO_BIN_EXE_worktree_server_fixture")),
        vec!["serve".into()],
    )
    .unwrap();
    bridge_worker::execution::prepare_round_execution(
        &f.layout,
        &f.project,
        bridge_storage::RoundRef {
            task_id: f.task,
            project_id: f.project.id().clone(),
            round_number: number,
        },
        &[],
        &[],
        &command,
        RuntimeOptions {
            lock_wait: Duration::from_secs(1),
            ready_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_millis(100),
        },
    )
}
#[test]
fn worker_revision_verifier_collection_and_checkpoint_use_the_same_checkout() {
    let _network = network_fence();
    let f = pending_execution_fixture();
    let main = bridge_git::take_snapshot(f.project.workspace()).unwrap();
    let first = prepare_fixture(&f, 1).unwrap();
    let baseline = first.baseline_json().unwrap();
    let port = first.server_port;
    let dispatched = first.dispatch(&f.layout).unwrap();
    std::fs::write(f.checkout.join("file"), "executor change\n").unwrap();
    let changes = first.collect_changes(&f.layout).unwrap();
    assert_eq!(changes.changed_paths(), [std::ffi::OsString::from("file")]);
    assert!(changes.scope_violations().is_empty());
    let verification = first
        .verify(&f.layout, Duration::from_secs(2), 1024)
        .unwrap();
    assert_eq!(
        verification.verification().status,
        bridge_domain::VerificationStatus::Passed
    );
    assert!(
        serde_json::to_string(verification.verification())
            .unwrap()
            .contains("grep -q executor file")
    );
    first
        .finish(
            &f.layout,
            bridge_storage::FinishRoundInput {
                round: bridge_storage::RoundRef {
                    task_id: f.task,
                    project_id: f.project.id().clone(),
                    round_number: 1,
                },
                round_status: bridge_domain::RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: Some("done".into()),
                error_code: None,
                result_json: Some(serde_json::json!({})),
            },
            &[],
        )
        .unwrap();
    let mut storage = f.layout.open().unwrap();
    let checkpoint = storage
        .get_round_checkpoint(&bridge_storage::RoundRef {
            task_id: f.task,
            project_id: f.project.id().clone(),
            round_number: 1,
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(checkpoint).unwrap()["repositories"][0]["diff_stat"]["counts"]["modified"],
        1
    );
    storage
        .create_revision_round(bridge_storage::CreateRevisionRoundInput {
            task_id: f.task,
            project_id: f.project.id().clone(),
            round_number: 2,
            request_id: "revision".into(),
            payload_hash: "revision-hash".into(),
            findings: Some("fix".into()),
        })
        .unwrap();
    // Revision keeps its immutable baseline even if the executor committed.
    git(&f.checkout, &["add", "file"]);
    git(&f.checkout, &["commit", "-qm", "executor round"]);
    let revised = prepare_fixture(&f, 2).unwrap();
    assert_eq!(revised.root, first.root);
    assert_eq!(revised.server_port, port);
    assert_eq!(revised.baseline_json().unwrap(), baseline);
    assert_eq!(
        std::fs::read_to_string(f.checkout.join("file")).unwrap(),
        "executor change\n"
    );
    let revision = revised.dispatch(&f.layout).unwrap();
    assert_ne!(dispatched.session().id(), revision.session().id());
    assert_eq!(
        bridge_git::take_snapshot(f.project.workspace()).unwrap(),
        main
    );
    let prompts = std::fs::read_to_string(f.runtime.join("fixture-prompts.jsonl")).unwrap();
    assert_eq!(prompts.lines().count(), 2);
    for line in prompts.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(
            value["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains(f.checkout.to_str().unwrap())
        );
    }
    assert_eq!(
        storage.get_task(f.task).unwrap().unwrap().workspace,
        f.project.workspace().to_str().unwrap()
    );
    f.stop().unwrap();
}
#[test]
fn created_checkout_is_never_recreated_and_corrupt_baseline_is_not_refreshed() {
    let _network = network_fence();
    let f = pending_execution_fixture();
    prepare_fixture(&f, 1).unwrap();
    f.stop().unwrap();
    let storage = f.layout.open().unwrap();
    let raw = storage
        .get_worktree(f.task, f.project.id())
        .unwrap()
        .unwrap()
        .baseline_json
        .unwrap();
    storage
        .connection()
        .execute_batch("UPDATE worktrees SET baseline_json='{}'")
        .unwrap();
    assert_eq!(
        prepare_fixture(&f, 1).unwrap_err(),
        bridge_worker::execution::ExecutionError::Baseline
    );
    storage
        .connection()
        .execute("UPDATE worktrees SET baseline_json=?1", [raw])
        .unwrap();
    std::fs::remove_dir_all(&f.checkout).unwrap();
    assert_eq!(
        prepare_fixture(&f, 1).unwrap_err(),
        bridge_worker::execution::ExecutionError::MissingCheckout
    );
    assert!(!f.checkout.exists());
}
#[test]
fn creating_crash_recovers_only_a_proven_partial_checkout_and_keeps_frozen_base() {
    let _network = network_fence();
    let f = Fixture::new();
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute_batch("UPDATE worktrees SET status='creating'")
        .unwrap();
    std::fs::write(f.checkout.join("partial"), "partial creation").unwrap();
    let execution = prepare_fixture(&f, 1).unwrap();
    assert!(!f.checkout.join("partial").exists());
    assert_eq!(
        std::fs::read_to_string(execution.root.join("file")).unwrap(),
        "base\n"
    );
    f.stop().unwrap();
}

fn recover(f: &Fixture) -> bridge_worker::lifecycle::RecoveryOutcome {
    bridge_worker::lifecycle::recover_worktree_task(&f.layout, &f.project, f.task, &[]).unwrap()
}
fn close(f: &Fixture) -> bridge_worker::lifecycle::RecoveryOutcome {
    bridge_worker::lifecycle::close_worktree_task(&f.layout, &f.project, f.task, "finished", &[])
        .unwrap()
}
#[test]
fn explicit_close_stops_owned_server_removes_only_task_checkout_then_closes() {
    use bridge_worker::lifecycle::RecoveryOutcome;
    let _network = network_fence();
    let f = Fixture::new();
    let before = bridge_git::take_snapshot(f.project.workspace()).unwrap();
    f.start("serve").unwrap();
    std::fs::write(f.checkout.join("file"), "executor result\n").unwrap();
    assert_eq!(close(&f), RecoveryOutcome::Closed);
    assert!(!f.checkout.parent().unwrap().exists());
    let s = f.layout.open().unwrap();
    assert_eq!(
        s.get_task(f.task).unwrap().unwrap().status,
        TaskStatus::Closed
    );
    assert_eq!(
        s.get_worktree(f.task, f.project.id())
            .unwrap()
            .unwrap()
            .status,
        WorktreeStatus::Removed
    );
    assert_eq!(
        bridge_git::take_snapshot(f.project.workspace()).unwrap(),
        before
    );
    assert_eq!(close(&f), RecoveryOutcome::Blocked);
}
#[test]
fn busy_worker_defers_close_and_storage_cannot_terminalize_before_cleanup() {
    use bridge_worker::{WorkerLock, WorkerLockOutcome, lifecycle::RecoveryOutcome};
    let _network = network_fence();
    let f = Fixture::new();
    let WorkerLockOutcome::Acquired(guard) = WorkerLock::try_acquire(&f.layout).unwrap() else {
        panic!()
    };
    assert_eq!(close(&f), RecoveryOutcome::Deferred);
    let mut s = f.layout.open().unwrap();
    assert!(matches!(
        s.complete_requested_close(f.task),
        Err(bridge_storage::RoundUpdateError::InvalidPersistedState)
    ));
    assert!(
        s.finish_round(bridge_storage::FinishRoundInput {
            round: bridge_storage::RoundRef {
                task_id: f.task,
                project_id: f.project.id().clone(),
                round_number: 1
            },
            round_status: bridge_domain::RoundStatus::Failed,
            task_status: TaskStatus::Failed,
            response_message_id: None,
            response: None,
            error_code: None,
            result_json: None,
        })
        .is_err()
    );
    assert_eq!(
        s.get_task(f.task).unwrap().unwrap().status,
        TaskStatus::Implementing
    );
    assert!(f.checkout.exists());
    drop(guard);
    assert_eq!(recover(&f), RecoveryOutcome::Closed);
}
#[test]
fn pending_close_is_logical_and_error_rows_retain_diagnostics() {
    use bridge_worker::lifecycle::RecoveryOutcome;
    let _network = network_fence();
    let f = pending_execution_fixture();
    // A pending row does not authorize deletion of an unexpected directory.
    std::fs::create_dir_all(&f.checkout).unwrap();
    std::fs::write(f.checkout.join("sentinel"), "retain").unwrap();
    assert_eq!(close(&f), RecoveryOutcome::Closed);
    assert_eq!(
        std::fs::read_to_string(f.checkout.join("sentinel")).unwrap(),
        "retain"
    );
    let g = Fixture::new();
    g.layout
        .open()
        .unwrap()
        .connection()
        .execute("UPDATE worktrees SET status='error'", [])
        .unwrap();
    assert_eq!(recover(&g), RecoveryOutcome::Blocked);
    assert_eq!(close(&g), RecoveryOutcome::Deferred);
    assert!(g.checkout.exists());
    assert_ne!(
        g.layout
            .open()
            .unwrap()
            .get_task(g.task)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Closed
    );
}
#[test]
fn cleanup_crash_after_physical_removal_retries_before_terminal_transition() {
    use bridge_worker::lifecycle::RecoveryOutcome;
    let _network = network_fence();
    let f = Fixture::new();
    f.layout.open().unwrap().connection().execute_batch("CREATE TRIGGER defer_removed BEFORE UPDATE OF status ON worktrees WHEN NEW.status='removed' BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert_eq!(close(&f), RecoveryOutcome::Deferred);
    assert!(!f.checkout.parent().unwrap().exists());
    let s = f.layout.open().unwrap();
    assert_eq!(
        s.get_worktree(f.task, f.project.id())
            .unwrap()
            .unwrap()
            .status,
        WorktreeStatus::Removing
    );
    assert_eq!(
        s.get_task(f.task).unwrap().unwrap().status,
        TaskStatus::Implementing
    );
    let n: u32 = s
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind='worktree_cleanup_deferred'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    s.connection()
        .execute_batch("DROP TRIGGER defer_removed")
        .unwrap();
    assert_eq!(recover(&f), RecoveryOutcome::Closed);
}
#[test]
fn foreign_live_record_defers_cleanup_without_signalling_or_removing_checkout() {
    use bridge_worker::lifecycle::RecoveryOutcome;
    let _network = network_fence();
    let f = Fixture::new();
    f.start("serve").unwrap();
    let original = f.change_record(|r| {
        r["task_id"] = serde_json::json!("22222222-2222-4222-8222-222222222222")
    });
    assert_eq!(close(&f), RecoveryOutcome::Deferred);
    assert!(f.checkout.is_dir());
    std::fs::write(f.record(), original).unwrap();
    assert_eq!(
        worktree_server_state(&f.layout, &f.project, f.task).unwrap(),
        ServerState::Live
    );
    assert_eq!(recover(&f), RecoveryOutcome::Closed);
}
#[test]
fn created_missing_active_checkout_fails_without_recreation_or_server_spawn() {
    use bridge_worker::lifecycle::RecoveryOutcome;
    let _network = network_fence();
    let f = Fixture::new();
    bridge_git::checkout::remove_checkout(
        f.project.workspace(),
        &f.layout.project_dir(),
        f.task,
        &f.checkout,
    )
    .unwrap();
    assert_eq!(recover(&f), RecoveryOutcome::MissingCheckout);
    assert!(!f.checkout.exists());
    assert!(!f.record().exists());
    let s = f.layout.open().unwrap();
    assert_eq!(
        s.get_task(f.task).unwrap().unwrap().status,
        TaskStatus::Failed
    );
    let error: String = s
        .connection()
        .query_row(
            "SELECT error_code FROM rounds WHERE task_id=?1",
            [f.task.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(error, "worktree_missing");
    assert!(!recover(&f).may_spawn());
}
#[test]
fn review_failed_needs_user_delivery_unknown_and_accepted_keep_checkout_and_contents() {
    use bridge_worker::lifecycle::RecoveryOutcome;
    let _network = network_fence();
    for status in [
        "awaiting_review",
        "failed",
        "needs_user",
        "delivery_unknown",
        "accepted",
    ] {
        let f = Fixture::new();
        std::fs::write(f.checkout.join("file"), "keep result\n").unwrap();
        f.layout
            .open()
            .unwrap()
            .connection()
            .execute("UPDATE tasks SET status=?1", [status])
            .unwrap();
        assert_eq!(recover(&f), RecoveryOutcome::Retained);
        assert_eq!(
            std::fs::read_to_string(f.checkout.join("file")).unwrap(),
            "keep result\n"
        );
    }
}
#[test]
fn orphan_quarantine_is_idempotent_logical_and_preserves_git_registration_and_files() {
    let _network = network_fence();
    let f = Fixture::new();
    let orphan: TaskId = "22222222-2222-4222-8222-222222222222".parse().unwrap();
    let base = git(f.project.workspace(), &["rev-parse", "HEAD"]);
    let o = bridge_git::checkout::create_checkout(
        f.project.workspace(),
        &f.layout.project_dir(),
        orphan,
        &base,
    )
    .unwrap();
    std::fs::write(o.paths.checkout.join("file"), "keep orphan\n").unwrap();
    let invalid = f.layout.project_dir().join("worktrees/invalid-id");
    std::fs::create_dir_all(&invalid).unwrap();
    std::fs::write(invalid.join("sentinel"), "keep invalid").unwrap();
    let registrations = bridge_git::checkout::registrations(f.project.workspace()).unwrap();
    let original = bridge_git::take_snapshot(&o.paths.checkout).unwrap();
    let metadata = std::fs::metadata(&o.paths.task_dir).unwrap();
    assert_eq!(
        bridge_worker::lifecycle::quarantine_orphans(&f.layout, &f.project).unwrap(),
        2
    );
    let entries = f.layout.open().unwrap().list_worktree_quarantine().unwrap();
    assert_eq!(
        bridge_worker::lifecycle::quarantine_orphans(&f.layout, &f.project).unwrap(),
        0
    );
    assert_eq!(
        entries,
        f.layout.open().unwrap().list_worktree_quarantine().unwrap()
    );
    assert!(entries.iter().all(|e| e.quarantined_path.is_none()));
    assert!(
        entries
            .iter()
            .any(|e| e.reason.as_deref() == Some("orphan_registered"))
    );
    assert!(
        entries
            .iter()
            .any(|e| e.reason.as_deref() == Some("invalid_task_id"))
    );
    assert_eq!(
        bridge_git::checkout::registrations(f.project.workspace()).unwrap(),
        registrations
    );
    assert_eq!(
        bridge_git::take_snapshot(&o.paths.checkout).unwrap(),
        original
    );
    assert_eq!(
        std::fs::metadata(o.paths.task_dir).unwrap().permissions(),
        metadata.permissions()
    );
    assert_eq!(
        std::fs::read_to_string(invalid.join("sentinel")).unwrap(),
        "keep invalid"
    );
}

#[test]
fn creating_close_cleans_proven_partial_checkout_or_absent_slot() {
    use bridge_worker::lifecycle::RecoveryOutcome;
    let _network = network_fence();
    for present in [true, false] {
        let f = Fixture::new();
        if !present {
            bridge_git::checkout::remove_checkout(
                f.project.workspace(),
                &f.layout.project_dir(),
                f.task,
                &f.checkout,
            )
            .unwrap();
        }
        f.layout
            .open()
            .unwrap()
            .connection()
            .execute("UPDATE worktrees SET status='creating'", [])
            .unwrap();
        assert_eq!(close(&f), RecoveryOutcome::Closed);
        assert!(!f.checkout.parent().unwrap().exists());
    }
}
#[test]
fn missing_checkout_with_live_registration_defers_close_without_global_pruning() {
    use bridge_worker::lifecycle::RecoveryOutcome;
    let _network = network_fence();
    let f = Fixture::new();
    std::fs::remove_dir_all(&f.checkout).unwrap();
    let before = bridge_git::checkout::registrations(f.project.workspace()).unwrap();
    assert!(before.contains(&f.checkout));
    assert_eq!(close(&f), RecoveryOutcome::Deferred);
    assert_eq!(
        before,
        bridge_git::checkout::registrations(f.project.workspace()).unwrap()
    );
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(f.task)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Implementing
    );
}
#[cfg(unix)]
#[test]
fn orphan_scan_refuses_symlink_root_and_never_traverses_symlink_entries() {
    let _network = network_fence();
    let f = Fixture::new();
    let outside = f.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), "keep outside").unwrap();
    let root = f.layout.project_dir().join("worktrees");
    std::os::unix::fs::symlink(&outside, root.join("symlink-orphan")).unwrap();
    assert_eq!(
        bridge_worker::lifecycle::quarantine_orphans(&f.layout, &f.project).unwrap(),
        0
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("sentinel")).unwrap(),
        "keep outside"
    );
    std::fs::rename(&root, f.layout.project_dir().join("saved-worktrees")).unwrap();
    std::os::unix::fs::symlink(&outside, &root).unwrap();
    assert_eq!(
        bridge_worker::lifecycle::quarantine_orphans(&f.layout, &f.project),
        Err(bridge_worker::execution::ExecutionError::Binding)
    );
    assert!(
        f.layout
            .open()
            .unwrap()
            .list_worktree_quarantine()
            .unwrap()
            .is_empty()
    );
    std::fs::remove_file(root).unwrap();
    std::fs::rename(
        f.layout.project_dir().join("saved-worktrees"),
        f.layout.project_dir().join("worktrees"),
    )
    .unwrap();
}

#[test]
fn parallel_fenced_rounds_keep_distinct_checkouts_servers_and_task_local_recovery() {
    use bridge_worker::{
        execution::prepare_fenced_round_execution,
        lifecycle::{RecoveryOutcome, recover_project_worktrees},
    };
    let _network = network_fence();
    let f = pending_execution_fixture();
    let mut s = f.layout.open().unwrap();
    s.connection()
        .execute("UPDATE active_writers SET parallel=1", [])
        .unwrap();
    let second: TaskId = "22222222-2222-4222-8222-222222222222".parse().unwrap();
    let base = git(f.project.workspace(), &["rev-parse", "HEAD"]);
    let paths = bridge_git::checkout::CheckoutPaths::new(&f.layout.project_dir(), second).unwrap();
    let profile = f.project.profile_snapshot(None).unwrap();
    s.create_task_with_profile_and_checkout(
        CreateTaskInput {
            task_id: second,
            project_id: f.project.id().clone(),
            workspace: f.project.workspace().to_str().unwrap().into(),
            task: "second executor".into(),
            request_id: "second".into(),
            payload_hash: "second".into(),
            base_head: Some(base.clone()),
            allowed_paths: vec!["other".into()],
            test_commands: vec![],
            snapshot: Some(
                bridge_git::take_snapshot(f.project.workspace())
                    .unwrap()
                    .to_json()
                    .unwrap(),
            ),
        },
        &AdmissionSettings::new(10, true, ExecutionMode::Worktree).unwrap(),
        TaskStatus::Implementing,
        None,
        &profile,
        &bridge_storage::PendingCheckout {
            path: paths.checkout.to_str().unwrap().into(),
            runtime_dir: paths.runtime_dir.to_str().unwrap().into(),
            base_head: base,
        },
    )
    .unwrap();
    let command = ServerCommand::executable(
        Path::new(env!("CARGO_BIN_EXE_worktree_server_fixture")),
        vec!["serve".into()],
    )
    .unwrap();
    let prepare = |id| {
        prepare_fenced_round_execution(
            &f.layout,
            &f.project,
            bridge_storage::RoundRef {
                task_id: id,
                project_id: f.project.id().clone(),
                round_number: 1,
            },
            &[],
            &[],
            &command,
            RuntimeOptions {
                lock_wait: Duration::from_secs(1),
                ready_timeout: Duration::from_secs(1),
                request_timeout: Duration::from_millis(100),
            },
        )
    };
    // The live config is single-writer; persisted parallel admissions survive.
    let first = prepare(f.task).unwrap();
    let second_context = prepare(second).unwrap();
    assert_ne!(first.execution.root, second_context.execution.root);
    assert_ne!(
        first.execution.server_port,
        second_context.execution.server_port
    );
    assert!(prepare(f.task).is_err());
    let main = bridge_git::take_snapshot(f.project.workspace()).unwrap();
    first.execution.dispatch(&f.layout).unwrap();
    second_context.execution.dispatch(&f.layout).unwrap();
    std::fs::write(first.execution.root.join("file"), "first executor").unwrap();
    std::fs::write(
        second_context.execution.root.join("other"),
        "second executor",
    )
    .unwrap();
    assert!(
        first
            .execution
            .collect_changes(&f.layout)
            .unwrap()
            .scope_violations()
            .is_empty()
    );
    assert!(
        second_context
            .execution
            .collect_changes(&f.layout)
            .unwrap()
            .scope_violations()
            .is_empty()
    );
    assert_eq!(
        bridge_git::take_snapshot(f.project.workspace()).unwrap(),
        main
    );
    assert_eq!(close(&f), RecoveryOutcome::Deferred);
    let outcomes = recover_project_worktrees(&f.layout, &f.project, &[]).unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(
        outcomes
            .iter()
            .all(|(_, o)| *o == RecoveryOutcome::Deferred)
    );
    drop(first);
    // Closing one checkout is safe while the disjoint executor is still fenced.
    assert_eq!(recover(&f), RecoveryOutcome::Closed);
    assert!(second_context.execution.root.exists());
    assert!(matches!(
        bridge_worker::WorkerLock::try_acquire_task(&f.layout, second).unwrap(),
        bridge_worker::WorkerLockOutcome::Busy
    ));
    drop(second_context);
    assert_eq!(
        recover_project_worktrees(&f.layout, &f.project, &[])
            .unwrap()
            .len(),
        2
    );
    bridge_runtime::stop_worktree_server(&f.layout, &f.project, second, &[]).unwrap();
}

fn parked_fixture() -> Fixture {
    let f = pending_execution_fixture();
    let context = prepare_fixture(&f, 1).unwrap();
    context.dispatch(&f.layout).unwrap();
    f.layout
        .open()
        .unwrap()
        .finish_round(bridge_storage::FinishRoundInput {
            round: bridge_storage::RoundRef {
                task_id: f.task,
                project_id: f.project.id().clone(),
                round_number: 1,
            },
            round_status: bridge_domain::RoundStatus::NeedsUser,
            task_status: TaskStatus::NeedsUser,
            response_message_id: None,
            response: None,
            error_code: Some("needs_user".into()),
            result_json: Some(serde_json::json!({"blockers":[]})),
        })
        .unwrap();
    f
}
#[test]
fn needs_user_recovery_claims_saved_endpoint_and_never_resends_or_duplicates_spawn() {
    use bridge_worker::recovery::{RecoverySpawnOutcome, recover_needs_user};
    let _network = network_fence();
    let f = parked_fixture();
    let before = std::fs::read_to_string(f.runtime.join("fixture-prompts.jsonl")).unwrap();
    let saved = f
        .layout
        .open()
        .unwrap()
        .connection()
        .query_row(
            "SELECT outbound_message_id,session_id FROM rounds",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .unwrap();
    let result = recover_needs_user(
        &f.layout,
        &f.project,
        f.task,
        true,
        &[],
        Duration::from_millis(100),
        |round| {
            assert_eq!(round.round_number, 1);
            assert!(matches!(
                bridge_worker::WorkerLock::try_acquire_admission(&f.layout).unwrap(),
                bridge_worker::WorkerLockOutcome::Acquired(_)
            ));
            Ok::<_, ()>(42)
        },
    )
    .unwrap();
    assert_eq!(result, RecoverySpawnOutcome::Spawned(42));
    let repeated = recover_needs_user(
        &f.layout,
        &f.project,
        f.task,
        true,
        &[],
        Duration::from_millis(100),
        |_| -> Result<u32, ()> { panic!("must not spawn twice") },
    )
    .unwrap();
    assert_eq!(repeated, RecoverySpawnOutcome::Unchanged);
    let after = f
        .layout
        .open()
        .unwrap()
        .connection()
        .query_row(
            "SELECT outbound_message_id,session_id FROM rounds",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(after, saved);
    assert_eq!(
        std::fs::read_to_string(f.runtime.join("fixture-prompts.jsonl")).unwrap(),
        before
    );
    let requests = std::fs::read_to_string(f.runtime.join("fixture-requests.log")).unwrap();
    assert_eq!(
        requests
            .lines()
            .filter(|s| s.starts_with("POST /session/"))
            .count(),
        1
    );
    assert_eq!(
        requests.lines().filter(|s| *s == "POST /session").count(),
        1
    );
    assert!(!requests.contains("POST /permission"));
}
#[test]
fn needs_user_recovery_background_blockers_and_explicit_spawn_failure_preserve_parked_round() {
    use bridge_worker::recovery::{RecoverySpawnOutcome, recover_needs_user};
    let _network = network_fence();
    let f = parked_fixture();
    for mode in ["permission", "question"] {
        std::fs::write(f.runtime.join("fixture-mode"), mode).unwrap();
        assert_eq!(
            recover_needs_user(
                &f.layout,
                &f.project,
                f.task,
                false,
                &[],
                Duration::from_millis(100),
                |_| Ok::<_, ()>(())
            )
            .unwrap(),
            RecoverySpawnOutcome::Blocked
        );
        // Explicit recovery may re-observe stale blockers; failed spawn rolls back.
        assert_eq!(
            recover_needs_user(
                &f.layout,
                &f.project,
                f.task,
                true,
                &[],
                Duration::from_millis(100),
                |_| Err::<(), _>("private spawn diagnostic")
            )
            .unwrap(),
            RecoverySpawnOutcome::SpawnFailed
        );
        assert_eq!(
            f.layout
                .open()
                .unwrap()
                .get_task(f.task)
                .unwrap()
                .unwrap()
                .status,
            TaskStatus::NeedsUser
        );
    }
    std::fs::write(f.runtime.join("fixture-mode"), "foreign-question").unwrap();
    assert_eq!(
        recover_needs_user(
            &f.layout,
            &f.project,
            f.task,
            false,
            &[],
            Duration::from_millis(100),
            |_| Ok::<_, ()>(())
        )
        .unwrap(),
        RecoverySpawnOutcome::Spawned(())
    );
    assert_eq!(
        std::fs::read_to_string(f.runtime.join("fixture-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}
#[test]
fn needs_user_recovery_unhealthy_wrong_root_and_dead_server_never_claim_or_spawn() {
    use bridge_worker::recovery::{RecoverySpawnOutcome, recover_needs_user};
    let _network = network_fence();
    let f = parked_fixture();
    for mode in ["unhealthy", "bad-path"] {
        std::fs::write(f.runtime.join("fixture-mode"), mode).unwrap();
        assert_eq!(
            recover_needs_user(
                &f.layout,
                &f.project,
                f.task,
                true,
                &[],
                Duration::from_millis(100),
                |_| -> Result<(), ()> { panic!("must not spawn") }
            )
            .unwrap(),
            RecoverySpawnOutcome::Unavailable
        );
        assert_eq!(
            f.layout
                .open()
                .unwrap()
                .get_task(f.task)
                .unwrap()
                .unwrap()
                .status,
            TaskStatus::NeedsUser
        );
    }
    f.stop().unwrap();
    assert_eq!(
        recover_needs_user(
            &f.layout,
            &f.project,
            f.task,
            true,
            &[],
            Duration::from_millis(100),
            |_| Ok::<_, ()>(())
        )
        .unwrap(),
        RecoverySpawnOutcome::Unavailable
    );
}
#[test]
fn needs_user_recovery_close_during_identity_probe_is_revalidated_before_claim() {
    use bridge_worker::recovery::{RecoverySpawnOutcome, recover_needs_user};
    let _network = network_fence();
    let f = parked_fixture();
    std::fs::write(f.runtime.join("pause-path"), "pause").unwrap();
    std::thread::scope(|scope| {
        let recovery = scope.spawn(|| {
            recover_needs_user(
                &f.layout,
                &f.project,
                f.task,
                true,
                &[],
                Duration::from_secs(1),
                |_| -> Result<(), ()> { panic!("close must prevent spawn") },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !f.runtime.join("path-seen").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(f.runtime.join("path-seen").exists());
        f.layout
            .open()
            .unwrap()
            .request_task_close(f.task, "user close")
            .unwrap();
        std::fs::write(f.runtime.join("resume-path"), "resume").unwrap();
        assert_eq!(
            recovery.join().unwrap().unwrap(),
            RecoverySpawnOutcome::Unchanged
        );
    });
    let claims: i64 = f
        .layout
        .open()
        .unwrap()
        .connection()
        .query_row(
            "SELECT count(*) FROM events WHERE kind='needs_user_recovery'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(claims, 0);
}

#[test]
fn concurrent_needs_user_recovery_spawns_once_after_http_probes() {
    use bridge_worker::recovery::{RecoverySpawnOutcome, recover_needs_user};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _network = network_fence();
    let f = parked_fixture();
    let barrier = std::sync::Barrier::new(2);
    let count = AtomicUsize::new(0);
    let outcomes = std::thread::scope(|scope| {
        let run = || {
            barrier.wait();
            recover_needs_user(
                &f.layout,
                &f.project,
                f.task,
                true,
                &[],
                Duration::from_millis(500),
                |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ()>(())
                },
            )
            .unwrap()
        };
        let a = scope.spawn(run);
        let b = scope.spawn(run);
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, RecoverySpawnOutcome::Spawned(())))
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(f.runtime.join("fixture-prompts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn resume_worktree_context_uses_saved_runtime_and_baseline_after_executor_commit() {
    use bridge_worker::execution::resume_round_execution;
    let _network = network_fence();
    let f = pending_execution_fixture();
    let prepared = prepare_fixture(&f, 1).unwrap();
    let before = prepared.baseline_json().unwrap();
    let record = std::fs::read(f.record()).unwrap();
    let round = bridge_storage::RoundRef {
        task_id: f.task,
        project_id: f.project.id().clone(),
        round_number: 1,
    };
    let mut storage = f.layout.open().unwrap();
    storage
        .bind_round_session(round.clone(), "saved-session".into())
        .unwrap();
    storage
        .prepare_round(round.clone(), "saved-outbound".into())
        .unwrap();
    storage.mark_round_sent(round.clone()).unwrap();
    std::fs::write(f.checkout.join("file"), "executor committed\n").unwrap();
    git(&f.checkout, &["add", "file"]);
    git(&f.checkout, &["commit", "-qm", "executor"]);
    let resumed =
        resume_round_execution(&f.layout, &f.project, round.clone(), Duration::from_secs(1))
            .unwrap();
    assert_eq!(resumed.root, f.checkout);
    assert_eq!(resumed.server_port, prepared.server_port);
    assert_eq!(resumed.baseline_json().unwrap(), before);
    assert_eq!(std::fs::read(f.record()).unwrap(), record);
    assert!(resumed.dispatch(&f.layout).is_err());
    storage
        .connection()
        .execute(
            "UPDATE worktrees SET server_endpoint='http://127.0.0.1:1'",
            [],
        )
        .unwrap();
    assert!(resume_round_execution(&f.layout, &f.project, round, Duration::from_secs(1)).is_err());
}

#[test]
fn worker_runner_closes_worktree_only_after_cleanup_and_defers_while_fenced() {
    use bridge_worker::{
        WorkerLock, WorkerLockOutcome,
        runner::{WorkerOutcome, WorkerSettings, run_worker},
    };
    let _network = network_fence();
    let f = Fixture::new();
    let before = bridge_git::take_snapshot(f.project.workspace()).unwrap();
    f.start("serve").unwrap();
    f.layout
        .open()
        .unwrap()
        .request_task_close(f.task, "close")
        .unwrap();
    let reference = bridge_storage::RoundRef {
        task_id: f.task,
        project_id: f.project.id().clone(),
        round_number: 1,
    };
    let guard = match WorkerLock::try_acquire(&f.layout).unwrap() {
        WorkerLockOutcome::Acquired(g) => g,
        _ => panic!("busy"),
    };
    assert_eq!(
        run_worker(
            &f.layout,
            &f.project,
            reference.clone(),
            &[&f.layout],
            &[&f.project],
            &ServerCommand::opencode(),
            RuntimeOptions::default(),
            WorkerSettings::default()
        )
        .unwrap(),
        WorkerOutcome::CloseDeferred
    );
    assert!(f.checkout.exists());
    assert!(f.record().exists());
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(f.task)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Implementing
    );
    drop(guard);
    assert_eq!(
        run_worker(
            &f.layout,
            &f.project,
            reference,
            &[&f.layout],
            &[&f.project],
            &ServerCommand::opencode(),
            RuntimeOptions::default(),
            WorkerSettings::default()
        )
        .unwrap(),
        WorkerOutcome::Closed
    );
    assert!(!f.checkout.parent().unwrap().exists());
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_worktree(f.task, f.project.id())
            .unwrap()
            .unwrap()
            .status,
        WorktreeStatus::Removed
    );
    assert_eq!(
        bridge_git::take_snapshot(f.project.workspace()).unwrap(),
        before
    );
}

#[test]
fn readonly_diagnostics_show_full_active_set_and_sanitize_progress() {
    let f = Fixture::new();
    let mut s = f.layout.open().unwrap();
    let before = std::fs::read(f.layout.database()).unwrap();
    let snapshot = bridge_runtime::diagnostics::snapshot(&f.layout).unwrap();
    assert_eq!(snapshot["active_tasks"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["active_tasks"][0]["phase"], "agent");
    assert!(!snapshot.to_string().contains("fixture-password"));
    assert!(!snapshot.to_string().contains(f.root.to_str().unwrap()));
    s.connection().execute("UPDATE rounds SET verifier_state='running',verifier_json=?1 WHERE task_id=?2",[serde_json::json!({"state":"running","command_index":999,"command_count":2,"secret":"DO_NOT_EMIT"}).to_string(),f.task.to_string()]).unwrap();
    let snap = bridge_runtime::diagnostics::snapshot(&f.layout).unwrap();
    assert_eq!(snap["active_tasks"][0]["phase"], "verifying");
    assert_eq!(
        snap["active_tasks"][0]["verification_progress"]["command_index"],
        2
    );
    assert!(!snap.to_string().contains("DO_NOT_EMIT"));
    s.connection()
        .execute("UPDATE active_writers SET parallel=1", [])
        .unwrap();
    s.create_task_with_admission(
        CreateTaskInput {
            task_id: "22222222-2222-4222-8222-222222222222".parse().unwrap(),
            project_id: f.project.id().clone(),
            workspace: f.project.workspace().to_str().unwrap().into(),
            task: "SECRET TASK".into(),
            request_id: "second".into(),
            payload_hash: "second".into(),
            allowed_paths: vec!["second".into()],
            test_commands: vec![],
            base_head: None,
            snapshot: None,
        },
        &AdmissionSettings::new(2, true, ExecutionMode::Worktree).unwrap(),
        TaskStatus::Implementing,
    )
    .unwrap();
    let multi = bridge_runtime::diagnostics::snapshot(&f.layout).unwrap();
    assert_eq!(multi["active_tasks"].as_array().unwrap().len(), 2);
    assert_eq!(multi["active_writer_count"], 2);
    assert!(!multi.to_string().contains("SECRET TASK"));
    assert_eq!(std::fs::read(f.layout.database()).unwrap(), before);
    s.connection()
        .execute("UPDATE tasks SET updated_at=?1", ["bad\nINJECT"])
        .unwrap();
    assert!(bridge_runtime::diagnostics::snapshot(&f.layout).is_err());
}

#[test]
fn hook_context_has_bounded_identity_only_payload_and_refuses_injection() {
    use bridge_runtime::diagnostics::Brief;
    let base = Brief {
        project_id: "proj".into(),
        task_id: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
        status: TaskStatus::Implementing,
        updated_at: "2026-10-06T12:34:56.123+00:00".into(),
        phase: Some("verifying"),
        round: Some(1),
        progress: None,
    };
    let first = bridge_runtime::hook::context(std::slice::from_ref(&base))
        .unwrap()
        .unwrap();
    assert_eq!(
        first["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    assert!(
        first["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("verifier")
    );
    assert!(first.get("decision").is_none());
    assert_eq!(
        first,
        bridge_runtime::hook::context(std::slice::from_ref(&base))
            .unwrap()
            .unwrap()
    );
    for field in ["project", "time", "phase", "status", "phase_status"] {
        let mut b = base.clone();
        match field {
            "project" => b.project_id = "proj\nINJECT".into(),
            "time" => b.updated_at = "2026-10-06T12:34:56Z\nINJECT".into(),
            "phase" => b.phase = Some("INJECT"),
            "phase_status" => b.status = TaskStatus::AwaitingReview,
            _ => b.status = TaskStatus::Accepted,
        };
        assert!(bridge_runtime::hook::context(&[b]).is_err(), "{field}");
    }
    let multi = bridge_runtime::hook::context(&vec![base; 100])
        .unwrap()
        .unwrap();
    assert!(
        multi["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= 1200
    );
    assert!(bridge_runtime::hook::context(&[]).unwrap().is_none());
}

#[test]
fn attachment_routes_task_checkout_and_refuses_stale_or_unbound_targets() {
    let _network = network_fence();
    let f = Fixture::new();
    let server = f.start("serve").unwrap();
    let mut s = f.layout.open().unwrap();
    s.update_worktree_server(
        f.task,
        f.project.id(),
        &format!("http://127.0.0.1:{}", server.port),
        std::num::NonZeroU16::new(server.port).unwrap(),
        None,
    )
    .unwrap();
    let resolve = || {
        bridge_runtime::attachment::resolve(
            &f.project,
            &f.layout,
            Some(f.task),
            false,
            Duration::from_millis(300),
        )
    };
    let target = resolve().unwrap();
    assert_eq!(target.workspace(), f.checkout);
    assert_eq!(
        target.endpoint(),
        format!("http://127.0.0.1:{}", server.port)
    );
    assert_eq!(target.argv()[0], "attach");
    let capture = f.root.join("attach.json");
    let code = "import json,os,sys;open(sys.argv[1],'w').write(json.dumps({'argv':sys.argv[2:],'cwd':os.getcwd(),'auth':os.environ.get('OPENCODE_SERVER_PASSWORD')=='fixture-password'}));sys.exit(7)";
    let command = ServerCommand::executable(
        Path::new("/usr/bin/python3"),
        vec!["-c".into(), code.into(), capture.clone().into_os_string()],
    )
    .unwrap();
    assert_eq!(target.launch(&command).unwrap().code(), Some(7));
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(capture).unwrap()).unwrap();
    assert_eq!(report["argv"], serde_json::json!(target.argv()));
    assert_eq!(report["cwd"], serde_json::json!(f.checkout));
    assert_eq!(report["auth"], true);

    assert!(!target.argv().iter().any(|v| v == "--session"));
    assert!(
        bridge_runtime::attachment::resolve(
            &f.project,
            &f.layout,
            Some(f.task),
            true,
            Duration::from_millis(300)
        )
        .is_err()
    );
    s.connection()
        .execute("UPDATE tasks SET session_id='ses_fixture_1'", [])
        .unwrap();
    std::fs::write(f.runtime.join("fixture-mode"), "attach-test").unwrap();
    let attached = bridge_runtime::attachment::resolve(
        &f.project,
        &f.layout,
        Some(f.task),
        true,
        Duration::from_millis(300),
    )
    .unwrap();
    assert_eq!(attached.argv().last().unwrap(), "ses_fixture_1");
    s.connection()
        .execute(
            "UPDATE worktrees SET server_endpoint='http://127.0.0.1:1'",
            [],
        )
        .unwrap();
    assert!(resolve().is_err());
    s.update_worktree_server(
        f.task,
        f.project.id(),
        &format!("http://127.0.0.1:{}", server.port),
        std::num::NonZeroU16::new(server.port).unwrap(),
        None,
    )
    .unwrap();
    s.connection()
        .execute("UPDATE tasks SET status='accepted'", [])
        .unwrap();
    assert!(resolve().is_err());
}

#[test]
fn explicit_opencode_stop_routes_by_saved_task_mode_and_preserves_checkout() {
    let _network = network_fence();
    let f = Fixture::new();
    f.start("ready").unwrap();
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.record()).unwrap()).unwrap();
    let tree =
        bridge_runtime::process_tree::ProcessTree::capture(record["pid"].as_u64().unwrap() as u32)
            .unwrap();
    // Changing defaults must not redirect an existing worktree console to main.
    let config_path = f.root.join("projects.toml");
    let config = std::fs::read_to_string(&config_path)
        .unwrap()
        .replace("execution_mode=\"worktree\"", "execution_mode=\"direct\"");
    std::fs::write(&config_path, config).unwrap();
    let project = load_config(&config_path)
        .unwrap()
        .project("proj")
        .unwrap()
        .clone();
    assert!(bridge_runtime::project::stop_opencode(&project, &f.layout, Some(f.task)).unwrap());
    assert!(!tree.is_alive());
    assert!(f.checkout.join("file").exists());
    assert_eq!(
        f.layout
            .open_readonly()
            .unwrap()
            .get_task(f.task)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Implementing
    );
    assert!(!f.record().exists());
    assert!(!bridge_runtime::project::stop_opencode(&project, &f.layout, Some(f.task)).unwrap());
}
