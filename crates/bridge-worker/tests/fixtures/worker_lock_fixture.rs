//! Short-lived local helper executable for `bridge-worker` lock integration
//! tests.
//!
//! It builds the same [`RustStateLayout`] as the tests and exposes a small,
//! explicit readiness handshake so a test never races the single non-blocking
//! acquisition:
//!
//! * `--ready PATH` publishes a `ready` evidence file atomically just before the
//!   acquisition phase begins.
//! * `--wait-for PATH` parks before acquiring until the parent creates `PATH`
//!   (a parent -> child signal), so a `Pending` observation is deterministic and
//!   no transient exclusive probe can overlap the child's one acquisition.
//! * `--acquire-delay-ms N` optionally waits before the readiness phase, to model
//!   a detached worker that only takes the lock after its own startup.
//! * `--never-acquire` skips the acquisition entirely (a worker that dies or
//!   never starts).
//! * `--hold-ms N` keeps the process (and any held lock) alive after the outcome
//!   is published.
//!
//! The typed outcome is recorded in `--evidence` (`acquired`/`busy`/`skipped`/
//! `error`) and, when it acquired the lock, the fixture keeps holding it for
//! `--hold-ms` before dropping the guard and exiting. Every evidence file is
//! published atomically (write to a sibling temporary file, then rename) so a
//! reader never observes a missing or torn value. It never touches the network
//! or an external service, and every wait/hold loop is capped by [`MAX_HOLD`] so
//! it can never outlive a test run.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};

use bridge_domain::ProjectId;
use bridge_storage::RustStateLayout;
use bridge_worker::{WorkerLock, WorkerLockOutcome};

/// Upper bound on the wait/hold loops, so a misbehaving test cannot leak a
/// process.
const MAX_HOLD: Duration = Duration::from_secs(30);

/// Poll interval for the bounded signal wait.
const POLL: Duration = Duration::from_millis(5);

/// Parsed fixture arguments.
struct Args {
    state_root: PathBuf,
    project: ProjectId,
    evidence: Option<PathBuf>,
    ready: Option<PathBuf>,
    wait_for: Option<PathBuf>,
    acquire_delay: Duration,
    hold: Duration,
    never_acquire: bool,
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(code) => return code,
    };

    if !args.acquire_delay.is_zero() {
        thread::sleep(args.acquire_delay.min(MAX_HOLD));
    }

    if let Some(ready) = &args.ready {
        write_evidence(ready, "ready");
    }

    if let Some(signal) = &args.wait_for
        && !wait_for_file(signal, MAX_HOLD)
    {
        write_outcome(&args, "error");
        return ExitCode::from(3);
    }

    if args.never_acquire {
        write_outcome(&args, "skipped");
        hold(&args);
        return ExitCode::SUCCESS;
    }

    let layout = match RustStateLayout::new(args.state_root.clone(), args.project.clone()) {
        Ok(layout) => layout,
        Err(_) => {
            write_outcome(&args, "error");
            return ExitCode::from(3);
        }
    };

    match WorkerLock::try_acquire(&layout) {
        Ok(WorkerLockOutcome::Acquired(guard)) => {
            write_outcome(&args, "acquired");
            hold(&args);
            drop(guard);
            ExitCode::SUCCESS
        }
        Ok(WorkerLockOutcome::Busy) => {
            write_outcome(&args, "busy");
            ExitCode::from(2)
        }
        Err(_) => {
            write_outcome(&args, "error");
            ExitCode::from(3)
        }
    }
}

/// Writes the typed outcome to the `--evidence` file, when one was requested.
fn write_outcome(args: &Args, outcome: &str) {
    if let Some(path) = &args.evidence {
        write_evidence(path, outcome);
    }
}

/// Publishes `content` to `path` atomically, so readers never see a torn value.
fn write_evidence(path: &Path, content: &str) {
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, format!("{content}\n")).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// Waits (bounded) until `path` exists, i.e. the parent released the child.
fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
}

/// Keeps the process (and any held lock) alive for the requested hold window.
fn hold(args: &Args) {
    if !args.hold.is_zero() {
        thread::sleep(args.hold.min(MAX_HOLD));
    }
}

/// Parses the fixture argv, returning a non-zero exit code on malformed input.
fn parse_args() -> Result<Args, ExitCode> {
    let mut state_root: Option<PathBuf> = None;
    let mut project: Option<ProjectId> = None;
    let mut evidence: Option<PathBuf> = None;
    let mut ready: Option<PathBuf> = None;
    let mut wait_for: Option<PathBuf> = None;
    let mut acquire_delay = Duration::ZERO;
    let mut hold = Duration::ZERO;
    let mut never_acquire = false;

    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--state-root") => state_root = args.next().map(PathBuf::from),
            Some("--project") => {
                let value = args.next().and_then(|value| value.into_string().ok());
                project = value.and_then(|value| ProjectId::from_str(&value).ok());
            }
            Some("--evidence") => evidence = args.next().map(PathBuf::from),
            Some("--ready") => ready = args.next().map(PathBuf::from),
            Some("--wait-for") => wait_for = args.next().map(PathBuf::from),
            Some("--acquire-delay-ms") => {
                acquire_delay = millis(&args.next()).unwrap_or(Duration::ZERO);
            }
            Some("--hold-ms") => hold = millis(&args.next()).unwrap_or(Duration::ZERO),
            Some("--never-acquire") => never_acquire = true,
            _ => return Err(ExitCode::from(64)),
        }
    }

    match (state_root, project) {
        (Some(state_root), Some(project)) => Ok(Args {
            state_root,
            project,
            evidence,
            ready,
            wait_for,
            acquire_delay,
            hold,
            never_acquire,
        }),
        _ => Err(ExitCode::from(64)),
    }
}

/// Parses a millisecond count from an `OsString`.
fn millis(value: &Option<std::ffi::OsString>) -> Option<Duration> {
    let text = value.as_ref()?.to_str()?;
    let millis = text.parse::<u64>().ok()?;
    Some(Duration::from_millis(millis))
}
