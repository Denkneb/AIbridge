//! Integration tests for the project-scoped worker lock and the startup grace.
//!
//! They use the short-lived `worker_lock_fixture` helper binary to model a
//! detached worker in a separate process: it can delay before acquiring the
//! lock, hold it for a bounded window, exit without acquiring, or skip the
//! acquisition entirely. Every child is short-lived and is reaped before the
//! test returns (a [`ChildGuard`] kills and waits on drop), every wait uses a
//! bounded `try_wait`/poll loop with a deadline, and the startup-grace windows
//! are short. No network, external service or Python state is ever touched.
//!
//! # No probe may race the single acquisition
//!
//! A non-blocking `flock` probe (both [`WorkerLock::try_acquire`] and
//! [`WorkerLock::is_held`], which acquires and immediately releases) holds the
//! exclusive lock for a brief window. If a test probes while the fixture is
//! making its one and only non-blocking acquisition, the fixture can observe
//! `Busy` and exit instead of becoming the planned holder. These tests therefore
//! never probe the lock until the fixture has *published* its `acquired`
//! evidence: the parent waits for that evidence with a bounded deadline, and the
//! fixture publishes every evidence file atomically (write + rename), so
//! `acquired` is a true happens-after for the held lock and no torn read is
//! possible. For the delayed-acquisition case the fixture parks on a
//! parent-created `--wait-for` signal and publishes a `ready` phase first, so
//! the `Pending` observation is deterministic and the acquisition is never
//! overlapped by a transient probe.
//!
//! # Process tests run one at a time
//!
//! A test can hold a real `flock` in the parent while it forks a fixture child.
//! A `fork` from any test thread inherits every open descriptor of the whole
//! multithreaded test process, including the lock descriptor another test
//! thread holds, until the forked child `exec`s. If that other test drops its
//! guard inside that window, the lock stays held by the inherited descriptor and
//! the next non-blocking acquisition spuriously observes `Busy`. The
//! [`PROCESS_TEST_SERIAL`] mutex serializes these tests so no `fork` from one
//! test can inherit another test's held lock; it changes no lock or
//! startup-grace assertion.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bridge_domain::ProjectId;
use bridge_storage::RustStateLayout;
use bridge_worker::{
    StartupGrace, StartupObservation, WorkerLock, WorkerLockErrorKind, WorkerLockOutcome,
};

/// Deadline for a fixture that must finish or change state on its own.
const DEADLINE: Duration = Duration::from_secs(10);

/// Serializes the process-spawning lock tests so no `fork` from one test can
/// inherit another test's held lock descriptor. See the module docs.
static PROCESS_TEST_SERIAL: Mutex<()> = Mutex::new(());

/// Acquires the process-test serialization guard, tolerating a poisoned lock
/// (a previously panicking test must not fail the remaining tests).
fn serialize_process_test() -> MutexGuard<'static, ()> {
    PROCESS_TEST_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Returns the absolute path of the lock fixture binary built by cargo.
fn fixture_executable() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_worker_lock_fixture"))
}

/// Creates a unique absolute temporary directory for one test.
fn unique_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "bridge-worker-lock-{label}-{nanos}-{}",
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

/// Owns a spawned fixture and kills/reaps it when the test ends, even on panic.
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn wait_bounded(&mut self, timeout: Duration) -> Option<ExitStatus> {
        wait_bounded(self.child.as_mut().expect("child handle"), timeout)
    }

    /// Kills and reaps the child now, so a later drop has nothing to do.
    fn kill_and_reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = wait_bounded(&mut child, Duration::from_secs(5));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = wait_bounded(&mut child, Duration::from_secs(5));
        }
    }
}

/// Polls `try_wait` with a deadline; `None` means the child was still running.
fn wait_bounded(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("try_wait must not fail") {
            Some(status) => return Some(status),
            None if Instant::now() >= deadline => return None,
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Initializes a fresh, valid Rust state for `project` under `state_root`.
fn initialized_layout(state_root: &Path, project: &ProjectId) -> RustStateLayout {
    let layout = RustStateLayout::new(state_root.to_path_buf(), project.clone()).expect("layout");
    layout.initialize().expect("rust state must initialize");
    layout
}

/// Spawns the lock fixture for `layout` with the given extra arguments.
fn spawn_fixture(layout: &RustStateLayout, evidence: &Path, extra: &[&str]) -> Child {
    let mut command = Command::new(fixture_executable());
    command
        .arg("--state-root")
        .arg(layout.state_root())
        .arg("--project")
        .arg(layout.project_id().as_str())
        .arg("--evidence")
        .arg(evidence);
    command.args(extra);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("fixture must spawn")
}

/// Reads the fixture evidence file, failing when it is absent.
fn read_evidence(path: &Path) -> String {
    fs::read_to_string(path)
        .expect("evidence must exist")
        .trim()
        .to_owned()
}

/// Polls an evidence file until it holds exactly `expected` or the deadline
/// passes.
///
/// This is the bounded readiness handshake: it never probes the lock, so it can
/// run while the fixture is making its single acquisition. The fixture publishes
/// evidence atomically, so a successful read is never torn.
fn wait_for_evidence(path: &Path, expected: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(content) = fs::read_to_string(path)
            && content.trim() == expected
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Polls the lock probe until it reports free or the deadline passes.
fn wait_for_free(layout: &RustStateLayout, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if WorkerLock::is_free(layout).expect("probe must not fail") {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn project(id: &str) -> ProjectId {
    ProjectId::from_str(id).expect("valid project id")
}

#[test]
fn concurrent_process_lock_is_busy_and_kill_releases_it() {
    let _serial = serialize_process_test();
    let dir = TestDir::new("busy");
    let layout = initialized_layout(&dir.path().join("state"), &project("proj-1"));
    let evidence = dir.path().join("evidence");

    let mut guard = ChildGuard::new(spawn_fixture(&layout, &evidence, &["--hold-ms", "30000"]));
    assert!(
        wait_for_evidence(&evidence, "acquired", DEADLINE),
        "the fixture must acquire and hold the lock"
    );
    assert_eq!(read_evidence(&evidence), "acquired");

    let outcome = WorkerLock::try_acquire(&layout).expect("probe acquisition");
    assert!(
        matches!(outcome, WorkerLockOutcome::Busy),
        "another process must make the lock busy"
    );

    guard.kill_and_reap();
    assert!(
        wait_for_free(&layout, DEADLINE),
        "killing the holder must release the lock"
    );
    let outcome = WorkerLock::try_acquire(&layout).expect("acquisition after kill");
    assert!(matches!(outcome, WorkerLockOutcome::Acquired(_)));
}

#[test]
fn process_exit_releases_the_lock() {
    let _serial = serialize_process_test();
    let dir = TestDir::new("exit");
    let layout = initialized_layout(&dir.path().join("state"), &project("proj-1"));
    let evidence = dir.path().join("evidence");

    let mut guard = ChildGuard::new(spawn_fixture(&layout, &evidence, &["--hold-ms", "300"]));
    assert!(
        wait_for_evidence(&evidence, "acquired", DEADLINE),
        "the fixture must acquire the lock before it exits"
    );

    let status = guard.wait_bounded(DEADLINE).expect("fixture must exit");
    assert!(status.success(), "the fixture must exit cleanly: {status}");
    assert!(
        wait_for_free(&layout, DEADLINE),
        "process exit must release the flock"
    );
    let outcome = WorkerLock::try_acquire(&layout).expect("acquisition after exit");
    assert!(matches!(outcome, WorkerLockOutcome::Acquired(_)));
}

#[test]
fn guard_drop_allows_another_process_to_acquire() {
    let _serial = serialize_process_test();
    let dir = TestDir::new("release");
    let layout = initialized_layout(&dir.path().join("state"), &project("proj-1"));
    let busy_evidence = dir.path().join("busy-evidence");
    let free_evidence = dir.path().join("free-evidence");

    let guard = WorkerLock::try_acquire(&layout).expect("parent acquisition");
    assert!(matches!(guard, WorkerLockOutcome::Acquired(_)));

    let mut child = ChildGuard::new(spawn_fixture(&layout, &busy_evidence, &[]));
    let status = child.wait_bounded(DEADLINE).expect("fixture must exit");
    let busy = read_evidence(&busy_evidence);
    assert_eq!(
        status.code(),
        Some(2),
        "the fixture must observe busy: status={status}, evidence={busy}"
    );
    assert_eq!(busy, "busy");

    drop(guard);

    let mut child = ChildGuard::new(spawn_fixture(&layout, &free_evidence, &[]));
    let status = child.wait_bounded(DEADLINE).expect("fixture must exit");
    let free = read_evidence(&free_evidence);
    assert!(
        status.success(),
        "the fixture must acquire after release: status={status}, evidence={free}"
    );
    assert_eq!(free, "acquired");
}

#[test]
fn different_projects_do_not_block_across_processes() {
    let _serial = serialize_process_test();
    let dir = TestDir::new("projects");
    let first = initialized_layout(&dir.path().join("state"), &project("proj-1"));
    let second = initialized_layout(&dir.path().join("state"), &project("proj-2"));
    let evidence = dir.path().join("evidence");

    let guard = WorkerLock::try_acquire(&first).expect("first acquisition");
    assert!(matches!(guard, WorkerLockOutcome::Acquired(_)));

    let mut child = ChildGuard::new(spawn_fixture(&second, &evidence, &[]));
    let status = child.wait_bounded(DEADLINE).expect("fixture must exit");
    assert!(
        status.success(),
        "a different project must not be blocked: {status}"
    );
    assert_eq!(read_evidence(&evidence), "acquired");
}

#[test]
fn delayed_acquisition_within_grace_becomes_held() {
    let _serial = serialize_process_test();
    let dir = TestDir::new("delayed");
    let layout = initialized_layout(&dir.path().join("state"), &project("proj-1"));
    let evidence = dir.path().join("evidence");

    let ready = dir.path().join("ready");
    let go = dir.path().join("go");

    let mut tracker = StartupGrace::new(Instant::now(), Duration::from_secs(10));
    let mut guard = ChildGuard::new(spawn_fixture(
        &layout,
        &evidence,
        &[
            "--ready",
            ready.to_str().expect("utf-8 ready path"),
            "--wait-for",
            go.to_str().expect("utf-8 go path"),
            "--hold-ms",
            "3000",
        ],
    ));

    // The fixture parks before acquisition and publishes its readiness phase.
    // Observing now cannot overlap the single acquisition.
    assert!(
        wait_for_evidence(&ready, "ready", DEADLINE),
        "the fixture must publish its readiness before acquiring"
    );

    let first = tracker
        .observe_lock(&layout, Instant::now())
        .expect("probe must not fail");
    assert_eq!(
        first,
        StartupObservation::Pending,
        "a free lock while the worker is still starting is startup, not a finished worker"
    );

    // Parent -> child signal: the fixture now makes its one acquisition.
    fs::write(&go, "go\n").expect("go signal must be written");

    // Wait for the acquisition to be published before probing again, so the
    // transient exclusive probe cannot make the single acquisition fail.
    assert!(
        wait_for_evidence(&evidence, "acquired", DEADLINE),
        "the released fixture must acquire the lock"
    );

    let second = tracker
        .observe_lock(&layout, Instant::now())
        .expect("probe must not fail");
    assert_eq!(
        second,
        StartupObservation::Held,
        "the delayed worker must be observed holding the lock"
    );

    guard.kill_and_reap();
}

#[test]
fn never_acquiring_worker_is_bounded_by_grace() {
    let _serial = serialize_process_test();
    let dir = TestDir::new("never");
    let layout = initialized_layout(&dir.path().join("state"), &project("proj-1"));
    let evidence = dir.path().join("evidence");

    let started = Instant::now();
    let mut tracker = StartupGrace::new(started, Duration::from_millis(300));
    let mut guard = ChildGuard::new(spawn_fixture(
        &layout,
        &evidence,
        &["--never-acquire", "--hold-ms", "5000"],
    ));

    let deadline = Instant::now() + DEADLINE;
    let mut expired = false;
    while Instant::now() < deadline {
        match tracker
            .observe_lock(&layout, Instant::now())
            .expect("probe must not fail")
        {
            StartupObservation::GraceExpired => {
                expired = true;
                break;
            }
            StartupObservation::Pending => thread::sleep(Duration::from_millis(20)),
            other => panic!("a never-acquiring worker must not be {other:?}"),
        }
    }
    assert!(expired, "the bounded grace must expire");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the observation must be bounded by the grace, not the worker hold"
    );

    guard.kill_and_reap();
}

#[test]
fn early_exit_without_acquiring_is_bounded_and_not_released() {
    let _serial = serialize_process_test();
    let dir = TestDir::new("early-exit");
    let layout = initialized_layout(&dir.path().join("state"), &project("proj-1"));
    let evidence = dir.path().join("evidence");

    let started = Instant::now();
    let mut tracker = StartupGrace::new(started, Duration::from_millis(300));
    let mut guard = ChildGuard::new(spawn_fixture(
        &layout,
        &evidence,
        &["--never-acquire", "--hold-ms", "0"],
    ));
    let status = guard.wait_bounded(DEADLINE).expect("fixture must exit");
    assert!(status.success());
    assert_eq!(read_evidence(&evidence), "skipped");

    let deadline = Instant::now() + DEADLINE;
    let mut expired = false;
    while Instant::now() < deadline {
        match tracker
            .observe_lock(&layout, Instant::now())
            .expect("probe must not fail")
        {
            StartupObservation::GraceExpired => {
                expired = true;
                break;
            }
            StartupObservation::Pending => thread::sleep(Duration::from_millis(20)),
            other => panic!("an early exit must never look released: {other:?}"),
        }
    }
    assert!(
        expired,
        "the bounded grace must expire after the early exit"
    );
}

#[test]
fn ownership_refusal_is_typed_and_does_not_create_the_lock() {
    let _serial = serialize_process_test();
    let dir = TestDir::new("ownership");
    let state_root = dir.path().join("state");
    let layout = RustStateLayout::new(state_root, project("proj-1")).expect("layout");

    let error = WorkerLock::try_acquire(&layout).expect_err("missing state must fail");
    assert_eq!(error.kind(), WorkerLockErrorKind::StateOwnership);
    assert!(!layout.project_dir().exists());
    assert!(!layout.lock(bridge_storage::RuntimeLock::Worker).exists());
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains("proj-1"));
    assert!(!rendered.contains("worker.lock"));
    assert!(error.source().is_some());
}
