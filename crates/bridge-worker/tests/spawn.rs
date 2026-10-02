//! Integration tests for the typed worker argv and the detached spawn path.
//!
//! They use the short-lived `worker_spawn_fixture` helper binary and never
//! touch the network, Python state or an external service. Every spawned child
//! is short-lived and is reaped before the test returns: a [`WorkerGuard`] kills
//! and waits on drop, and every wait uses a bounded `try_wait` loop with a
//! deadline instead of an unbounded `wait`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bridge_domain::{ProjectId, TaskId};
use bridge_storage::{RuntimeLog, RustStateLayout};
use bridge_worker::{SpawnedWorker, WorkerErrorKind, WorkerInvocation, spawn_worker};

const TASK_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

/// Deadline for a fixture that must finish on its own.
const EXIT_DEADLINE: Duration = Duration::from_secs(10);

/// Returns the absolute path of the test fixture binary built by cargo.
fn fixture_executable() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_worker_spawn_fixture"))
}

/// Creates a unique absolute temporary directory for one test.
fn unique_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "bridge-worker-{label}-{nanos}-{}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temporary directory must be created");
    path
}

/// Owns a temporary root and removes it when the test ends, even on panic.
struct TestDir {
    root: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        Self {
            root: unique_dir(label),
        }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Owns a spawned worker and kills/reaps it when the test ends, even on panic.
struct WorkerGuard {
    worker: Option<SpawnedWorker>,
}

impl WorkerGuard {
    fn new(worker: SpawnedWorker) -> Self {
        Self {
            worker: Some(worker),
        }
    }

    fn worker_mut(&mut self) -> &mut SpawnedWorker {
        self.worker.as_mut().expect("worker handle")
    }

    fn take(&mut self) -> SpawnedWorker {
        self.worker.take().expect("worker handle")
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.as_mut() {
            let _ = worker.kill();
            let _ = wait_bounded(worker, Duration::from_secs(5));
        }
    }
}

/// Polls `try_wait` with a deadline; `None` means the child was still running.
fn wait_bounded(worker: &mut SpawnedWorker, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match worker.try_wait().expect("try_wait must not fail") {
            Some(status) => return Some(status),
            None if Instant::now() >= deadline => return None,
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Reads the fixture evidence file, or `None` when it does not exist yet.
fn read_evidence_opt(workspace: &Path) -> Option<BTreeMap<String, Vec<String>>> {
    let text = fs::read_to_string(workspace.join("worker-spawn-fixture.evidence")).ok()?;
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('\t') {
            map.entry(key.to_owned())
                .or_default()
                .push(value.to_owned());
        }
    }
    Some(map)
}

/// Reads the fixture evidence file, failing when it is absent.
fn read_evidence(workspace: &Path) -> BTreeMap<String, Vec<String>> {
    read_evidence_opt(workspace).expect("fixture evidence must exist")
}

/// Waits for a held fixture to publish its evidence while asserting that the
/// child stays alive the whole time (the spawn returned before exit).
fn wait_for_held_evidence(
    worker: &mut SpawnedWorker,
    workspace: &Path,
) -> BTreeMap<String, Vec<String>> {
    let deadline = Instant::now() + EXIT_DEADLINE;
    loop {
        assert!(
            worker.try_wait().expect("try_wait").is_none(),
            "a held fixture must still be running"
        );
        if let Some(evidence) = read_evidence_opt(workspace) {
            return evidence;
        }
        assert!(
            Instant::now() < deadline,
            "held fixture did not publish evidence in time"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Reads the session id of the current test process on Linux, or `0`.
fn parent_session_id() -> u32 {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string("/proc/self/stat").expect("self stat");
        let (_, rest) = stat.rsplit_once(')').expect("comm field");
        let mut fields = rest.split_whitespace();
        let _state = fields.next();
        let _ppid = fields.next();
        let _pgrp = fields.next();
        fields
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Initializes a fresh, valid Rust state for `project` under `state_root`.
fn initialized_layout(state_root: &Path, project: &ProjectId) -> RustStateLayout {
    let layout = RustStateLayout::new(state_root.to_path_buf(), project.clone()).expect("layout");
    layout.initialize().expect("rust state must initialize");
    layout
}

#[test]
fn spawn_runs_exact_argv_in_workspace_with_new_session() {
    let dir = TestDir::new("spawn");
    let state_root = dir.path().join("state root");
    let workspace = dir.path().join("workspace with spaces");
    fs::create_dir_all(&workspace).expect("workspace");
    let config_path = dir.path().join("config dir").join("projects.toml");

    let project = ProjectId::from_str("proj-1").expect("project id");
    let layout = initialized_layout(&state_root, &project);
    let invocation = WorkerInvocation::new(
        fixture_executable(),
        project,
        config_path,
        state_root.clone(),
        TaskId::from_str(TASK_ID).expect("task id"),
        3,
    )
    .expect("invocation");

    let mut guard = WorkerGuard::new(
        spawn_worker(&invocation, &layout, &workspace).expect("spawn must succeed"),
    );
    let status = wait_bounded(guard.worker_mut(), EXIT_DEADLINE).expect("fixture must exit");
    assert!(status.success(), "fixture must exit successfully: {status}");

    let evidence = read_evidence(&workspace);
    let recorded_args: Vec<String> = evidence.get("arg").cloned().unwrap_or_default();
    let expected_args: Vec<String> = invocation
        .argv()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(recorded_args, expected_args);

    assert_eq!(
        evidence["cwd"],
        vec![workspace.to_string_lossy().into_owned()]
    );
    assert_eq!(evidence["stdin_eof"], vec!["true".to_owned()]);

    let pid: u32 = evidence["pid"][0].parse().expect("pid");
    let pgrp: u32 = evidence["pgrp"][0].parse().expect("pgrp");
    let sid: u32 = evidence["sid"][0].parse().expect("sid");
    assert_eq!(guard.take().pid(), pid);
    assert_eq!(pgrp, pid, "the child must lead its own process group");
    assert_eq!(sid, pid, "the child must lead a new session (setsid)");
    #[cfg(target_os = "linux")]
    assert_ne!(
        sid,
        parent_session_id(),
        "setsid must detach the child from the parent session"
    );

    let log_path = layout.log(RuntimeLog::Worker);
    assert_eq!(log_path, state_root.join("proj-1").join("worker.log"));
    let log = fs::read_to_string(&log_path).expect("worker log");
    assert!(
        log.contains("fixture-stdout"),
        "stdout must be appended: {log}"
    );
    assert!(
        log.contains("fixture-stderr"),
        "stderr must be appended: {log}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(state_root.join("proj-1"))
            .expect("state dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "the project state dir must be private");
    }
}

#[test]
fn spawn_appends_to_an_existing_worker_log() {
    let dir = TestDir::new("append");
    let state_root = dir.path().join("state");
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    let project = ProjectId::from_str("proj-1").expect("project id");
    let layout = initialized_layout(&state_root, &project);
    let log_path = layout.log(RuntimeLog::Worker);
    fs::write(&log_path, "preexisting-line\n").expect("pre-existing log");

    let invocation = WorkerInvocation::new(
        fixture_executable(),
        project,
        dir.path().join("projects.toml"),
        state_root,
        TaskId::from_str(TASK_ID).expect("task id"),
        1,
    )
    .expect("invocation");

    let mut guard = WorkerGuard::new(
        spawn_worker(&invocation, &layout, &workspace).expect("spawn must succeed"),
    );
    let status = wait_bounded(guard.worker_mut(), EXIT_DEADLINE).expect("fixture must exit");
    assert!(status.success(), "fixture must exit successfully: {status}");

    let log = fs::read_to_string(&log_path).expect("worker log");
    assert!(
        log.starts_with("preexisting-line\n"),
        "the log must be appended, not truncated"
    );
    assert!(log.contains("fixture-stdout"));
    assert!(log.contains("fixture-stderr"));
}

#[test]
fn missing_executable_is_a_typed_redacted_spawn_error() {
    let dir = TestDir::new("missing");
    let state_root = dir.path().join("state");
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    let missing = dir.path().join("does-not-exist-agent-bridge");
    let project = ProjectId::from_str("proj-1").expect("project id");
    let layout = initialized_layout(&state_root, &project);
    let invocation = WorkerInvocation::new(
        missing.clone(),
        project,
        dir.path().join("projects.toml"),
        state_root,
        TaskId::from_str(TASK_ID).expect("task id"),
        1,
    )
    .expect("invocation");

    let error = spawn_worker(&invocation, &layout, &workspace).expect_err("spawn must fail");
    assert_eq!(error.kind(), WorkerErrorKind::Spawn);

    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains("does-not-exist-agent-bridge"));
    assert!(!rendered.contains(&missing.to_string_lossy().into_owned()));
    assert!(error.source().is_some(), "the I/O cause stays in source");
}

#[test]
fn project_mismatch_fails_closed_without_touching_state() {
    let dir = TestDir::new("project-mismatch");
    let state_root = dir.path().join("state");
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    let project = ProjectId::from_str("proj-1").expect("project id");
    let layout = initialized_layout(&state_root, &project);
    let database_before = fs::read(layout.database()).expect("database bytes");
    let marker_before = fs::read(layout.marker()).expect("marker bytes");

    let other = ProjectId::from_str("proj-2").expect("other project id");
    let invocation = WorkerInvocation::new(
        fixture_executable(),
        other,
        dir.path().join("projects.toml"),
        state_root.clone(),
        TaskId::from_str(TASK_ID).expect("task id"),
        1,
    )
    .expect("invocation");

    let error = spawn_worker(&invocation, &layout, &workspace).expect_err("spawn must fail");
    assert_eq!(error.kind(), WorkerErrorKind::ProjectMismatch);
    assert!(!format!("{error} {error:?}").contains("proj-2"));

    assert!(
        !state_root.join("proj-2").exists(),
        "the foreign project dir must not be created"
    );
    assert!(
        !layout.log(RuntimeLog::Worker).exists(),
        "no worker log may be created"
    );
    assert!(
        read_evidence_opt(&workspace).is_none(),
        "no child may be spawned"
    );
    assert_eq!(
        fs::read(layout.database()).expect("database bytes"),
        database_before,
        "the database must not change"
    );
    assert_eq!(
        fs::read(layout.marker()).expect("marker bytes"),
        marker_before,
        "the marker must not change"
    );
}

#[test]
fn foreign_marker_fails_closed_without_touching_state() {
    let dir = TestDir::new("foreign-marker");
    let state_root = dir.path().join("state");
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    let project = ProjectId::from_str("proj-1").expect("project id");
    let layout = RustStateLayout::new(state_root.clone(), project.clone()).expect("layout");
    fs::create_dir_all(layout.project_dir()).expect("project dir");
    let foreign_marker =
        r#"{"implementation":"python","format_version":1,"project_id":"proj-1","state_root":"00"}"#;
    fs::write(layout.marker(), foreign_marker).expect("foreign marker");
    fs::write(layout.log(RuntimeLog::Worker), "foreign-log\n").expect("foreign log");
    let marker_before = fs::read(layout.marker()).expect("marker bytes");
    let log_before = fs::read(layout.log(RuntimeLog::Worker)).expect("log bytes");

    let invocation = WorkerInvocation::new(
        fixture_executable(),
        project,
        dir.path().join("projects.toml"),
        state_root,
        TaskId::from_str(TASK_ID).expect("task id"),
        1,
    )
    .expect("invocation");

    let error = spawn_worker(&invocation, &layout, &workspace).expect_err("spawn must fail");
    assert_eq!(error.kind(), WorkerErrorKind::StateOwnership);
    assert!(!format!("{error} {error:?}").contains("python"));
    assert!(error.source().is_some(), "the guard cause stays in source");

    assert_eq!(
        fs::read(layout.marker()).expect("marker bytes"),
        marker_before,
        "the foreign marker must not change"
    );
    assert_eq!(
        fs::read(layout.log(RuntimeLog::Worker)).expect("log bytes"),
        log_before,
        "the foreign log must not change"
    );
    assert!(
        read_evidence_opt(&workspace).is_none(),
        "no child may be spawned"
    );
}

#[test]
fn missing_marker_fails_closed_without_touching_state() {
    let dir = TestDir::new("missing-marker");
    let state_root = dir.path().join("state");
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    let project = ProjectId::from_str("proj-1").expect("project id");
    let layout = RustStateLayout::new(state_root.clone(), project.clone()).expect("layout");
    fs::create_dir_all(layout.project_dir()).expect("project dir");

    let invocation = WorkerInvocation::new(
        fixture_executable(),
        project,
        dir.path().join("projects.toml"),
        state_root,
        TaskId::from_str(TASK_ID).expect("task id"),
        1,
    )
    .expect("invocation");

    let error = spawn_worker(&invocation, &layout, &workspace).expect_err("spawn must fail");
    assert_eq!(error.kind(), WorkerErrorKind::StateOwnership);

    let entries: Vec<_> = fs::read_dir(layout.project_dir())
        .expect("project dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "an unmarked project dir must stay untouched: {entries:?}"
    );
    assert!(
        read_evidence_opt(&workspace).is_none(),
        "no child may be spawned"
    );
}

#[test]
fn state_copied_under_another_root_fails_closed() {
    let dir = TestDir::new("copied-root");
    let source_root = dir.path().join("source");
    let other_root = dir.path().join("other");
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");

    let project = ProjectId::from_str("proj-1").expect("project id");
    let source = initialized_layout(&source_root, &project);
    let other = RustStateLayout::new(other_root.clone(), project.clone()).expect("layout");
    fs::create_dir_all(other.project_dir()).expect("other project dir");
    fs::copy(source.marker(), other.marker()).expect("copy marker");
    let marker_before = fs::read(other.marker()).expect("marker bytes");

    let invocation = WorkerInvocation::new(
        fixture_executable(),
        project,
        dir.path().join("projects.toml"),
        other_root,
        TaskId::from_str(TASK_ID).expect("task id"),
        1,
    )
    .expect("invocation");

    let error = spawn_worker(&invocation, &other, &workspace).expect_err("spawn must fail");
    assert_eq!(error.kind(), WorkerErrorKind::StateOwnership);

    assert_eq!(
        fs::read(other.marker()).expect("marker bytes"),
        marker_before,
        "the copied marker must not change"
    );
    assert!(
        !other.log(RuntimeLog::Worker).exists(),
        "no worker log may be created"
    );
    assert!(
        read_evidence_opt(&workspace).is_none(),
        "no child may be spawned"
    );
}

#[test]
fn spawn_returns_before_a_held_child_exits_and_kill_reaps_it() {
    let dir = TestDir::new("kill");
    let state_root = dir.path().join("state");
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::write(workspace.join("worker-spawn-fixture.hold"), "hold\n").expect("hold file");

    let project = ProjectId::from_str("proj-1").expect("project id");
    let layout = initialized_layout(&state_root, &project);
    let invocation = WorkerInvocation::new(
        fixture_executable(),
        project,
        dir.path().join("projects.toml"),
        state_root,
        TaskId::from_str(TASK_ID).expect("task id"),
        1,
    )
    .expect("invocation");

    let mut guard = WorkerGuard::new(
        spawn_worker(&invocation, &layout, &workspace).expect("spawn must succeed"),
    );
    let _evidence = wait_for_held_evidence(guard.worker_mut(), &workspace);

    guard.worker_mut().kill().expect("kill must succeed");
    let status = wait_bounded(guard.worker_mut(), EXIT_DEADLINE).expect("child must be reaped");
    assert!(
        !status.success(),
        "a killed child must not report success: {status}"
    );
}

#[test]
fn held_child_exits_when_released() {
    let dir = TestDir::new("release");
    let state_root = dir.path().join("state");
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).expect("workspace");
    let hold = workspace.join("worker-spawn-fixture.hold");
    fs::write(&hold, "hold\n").expect("hold file");

    let project = ProjectId::from_str("proj-1").expect("project id");
    let layout = initialized_layout(&state_root, &project);
    let invocation = WorkerInvocation::new(
        fixture_executable(),
        project,
        dir.path().join("projects.toml"),
        state_root,
        TaskId::from_str(TASK_ID).expect("task id"),
        1,
    )
    .expect("invocation");

    let mut guard = WorkerGuard::new(
        spawn_worker(&invocation, &layout, &workspace).expect("spawn must succeed"),
    );
    let _evidence = wait_for_held_evidence(guard.worker_mut(), &workspace);

    fs::remove_file(&hold).expect("release the fixture");
    let status = wait_bounded(guard.worker_mut(), EXIT_DEADLINE).expect("child must exit");
    assert!(
        status.success(),
        "a released child must exit cleanly: {status}"
    );
}
