//! Short-lived local helper executable for `bridge-worker` integration tests.
//!
//! It is spawned through `bridge_worker::spawn_worker` with the production
//! worker argv. To prove the spawn contract it records its argv, working
//! directory, stdin state, pid, process-group id and session id in
//! `worker-spawn-fixture.evidence` inside its working directory, writes one
//! marker line to stdout and one to stderr, then exits with status 0. It never
//! touches the network or any external service.
//!
//! When a `worker-spawn-fixture.hold` file exists in its working directory the
//! fixture instead stays alive in a bounded loop (removing the file makes it
//! exit, and it self-exits after [`MAX_HOLD`]) so the tests can observe a live
//! child through `try_wait`, kill it, and prove that `spawn_worker` returns
//! before the child exits. The loop is intentionally short-lived and never
//! outlives a test run.

use std::env;
use std::fs;
use std::io::Read;
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound on the hold loop, so a misbehaving test cannot leave a process
/// running indefinitely.
const MAX_HOLD: Duration = Duration::from_secs(30);

/// Name of the control file that keeps the fixture alive.
const HOLD_FILE: &str = "worker-spawn-fixture.hold";

fn main() {
    let mut stdin = String::new();
    let stdin_eof = std::io::stdin().read_to_string(&mut stdin).is_ok();

    let mut evidence = String::new();
    for arg in env::args_os() {
        evidence.push_str("arg\t");
        evidence.push_str(&arg.to_string_lossy());
        evidence.push('\n');
    }
    let cwd = env::current_dir().unwrap_or_default();
    evidence.push_str("cwd\t");
    evidence.push_str(&cwd.to_string_lossy());
    evidence.push('\n');
    evidence.push_str(&format!("pid\t{}\n", std::process::id()));
    let (pgrp, sid) = process_ids();
    evidence.push_str(&format!("pgrp\t{pgrp}\nsid\t{sid}\n"));
    evidence.push_str(&format!("stdin_eof\t{stdin_eof}\n"));

    let _ = fs::write(cwd.join("worker-spawn-fixture.evidence"), evidence);
    println!("fixture-stdout");
    eprintln!("fixture-stderr");

    let hold = cwd.join(HOLD_FILE);
    if hold.exists() {
        hold_until_released(&hold);
    }
}

/// Sleeps in short increments until the control file disappears or [`MAX_HOLD`]
/// elapses, then returns so the process exits normally.
fn hold_until_released(hold: &std::path::Path) {
    let started = Instant::now();
    while hold.exists() && started.elapsed() < MAX_HOLD {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Returns `(pgrp, sid)` from `/proc/self/stat` on Linux, or `(0, 0)`
/// elsewhere. Fields follow the comm field, which may itself contain spaces and
/// parentheses, so parsing starts after the final `)`.
fn process_ids() -> (u32, u32) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(stat) = fs::read_to_string("/proc/self/stat")
            && let Some((_, rest)) = stat.rsplit_once(')')
        {
            let mut fields = rest.split_whitespace();
            let _state = fields.next();
            let _ppid = fields.next();
            let pgrp = fields
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let sid = fields
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            return (pgrp, sid);
        }
        (0, 0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (0, 0)
    }
}
