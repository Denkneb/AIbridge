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
