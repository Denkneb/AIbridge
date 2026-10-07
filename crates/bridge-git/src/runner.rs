//! Bounded Git command runner.
//!
//! Every Git invocation goes through [`run_bounded`]: the executable is fixed
//! by the caller, standard input is closed, standard error is discarded (never
//! captured, so it can never be surfaced through an error), standard output is
//! read as raw bytes, and the whole child lifecycle is bounded by a wall-clock
//! timeout. A child that outlives its deadline is killed and reaped, and the
//! stdout reader thread is joined, so no process or thread is left behind. The
//! snapshot callers use read-only commands; explicit branch/worktree APIs may
//! perform their documented mutations. There is no shell or string interpolation.

use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::GitError;

/// How often the parent polls a running child before its deadline.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// The captured outcome of one bounded Git command.
pub(crate) struct GitOutput {
    /// The child exit status.
    pub(crate) status: ExitStatus,
    /// The raw standard-output bytes; standard error is never captured.
    pub(crate) stdout: Vec<u8>,
}

/// Runs `program` with `args` in `workspace` under a wall-clock `timeout`.
///
/// Standard input is closed and standard error is discarded, so neither can
/// leak through the returned error. Standard output is read on a dedicated
/// thread, which avoids a pipe-buffer deadlock when a read-only command emits
/// more than the pipe capacity. On timeout or a wait error the child is killed
/// and reaped before returning; on timeout the reader thread is joined after
/// the kill closes the pipe.
///
/// # Errors
///
/// Returns [`GitError::Spawn`] when the process cannot be started,
/// [`GitError::Io`] when the stdout pipe is unavailable or unreadable,
/// [`GitError::Wait`] when waiting fails and [`GitError::Timeout`] when the
/// deadline is reached. The returned error carries no program name, arguments,
/// workspace, output or OS error text.
pub(crate) fn run_bounded(
    program: &OsStr,
    workspace: &Path,
    args: &[&OsStr],
    timeout: Duration,
) -> Result<GitOutput, GitError> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(workspace)
        // Read-only status must not refresh and rewrite the index stat cache.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| GitError::Spawn)?;

    let Some(mut pipe) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(GitError::Io);
    };
    let reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let read = pipe.read_to_end(&mut buffer);
        (read, buffer)
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(GitError::Timeout);
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(GitError::Wait);
            }
        }
    };

    let (read, stdout) = reader.join().map_err(|_| GitError::Io)?;
    read.map_err(|_| GitError::Io)?;
    let status = status?;
    Ok(GitOutput { status, stdout })
}

#[cfg(test)]
mod tests {
    use super::run_bounded;
    use crate::GitError;
    use std::ffi::OsStr;
    use std::time::{Duration, Instant};

    #[test]
    fn timeout_kills_and_reaps_within_bounds() {
        let start = Instant::now();
        let result = run_bounded(
            OsStr::new("sleep"),
            &std::env::temp_dir(),
            &[OsStr::new("30")],
            Duration::from_millis(50),
        );
        assert_eq!(result.err(), Some(GitError::Timeout));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed-out child was not reaped promptly"
        );
    }

    #[test]
    fn successful_command_returns_raw_stdout() {
        let output = run_bounded(
            OsStr::new("printf"),
            &std::env::temp_dir(),
            &[OsStr::new("hello")],
            Duration::from_secs(5),
        )
        .expect("printf should succeed");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello");
    }

    #[test]
    fn missing_executable_is_a_spawn_error() {
        let result = run_bounded(
            OsStr::new("bridge-git-no-such-executable"),
            &std::env::temp_dir(),
            &[],
            Duration::from_secs(5),
        );
        assert_eq!(result.err(), Some(GitError::Spawn));
    }

    #[test]
    fn large_stdout_does_not_deadlock() {
        let output = run_bounded(
            OsStr::new("seq"),
            &std::env::temp_dir(),
            &[OsStr::new("1"), OsStr::new("200000")],
            Duration::from_secs(30),
        )
        .expect("seq should succeed");
        assert!(output.status.success());
        assert!(output.stdout.len() > 1_000_000);
    }
}
