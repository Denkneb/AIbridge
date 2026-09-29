//! Single verifier command runner (task 5.2).
//!
//! This crate implements exactly one narrow production primitive: it runs one
//! already-agreed test command string and returns a typed outcome compatible in
//! meaning with the reference Python verifier's per-command entry
//! (`verifier.py:_run_command`). It deliberately does **not** run a command
//! sequence, capture Git fingerprints, detect side effects or persist anything;
//! those are tasks 5.3-5.6.
//!
//! # Contract
//!
//! [`run_test_command`] validates the command with the existing verifier
//! fail-closed policy ([`bridge_command_policy::validate_test_commands`]) before
//! anything is spawned: an empty, assignment-only, shell, glob or Git-write
//! command is rejected as [`CommandRunError::Rejected`] and never reaches the
//! operating system. A validated command is tokenized with
//! [`bridge_command_policy::split_command`] and its leading `NAME=value`
//! assignments are separated with
//! [`bridge_command_policy::leading_assignments`]. The remaining argv is passed
//! to the child as distinct OS arguments (there is no shell and no string
//! interpolation) and the leading assignments are overlaid on the inherited
//! environment of the child.
//!
//! The child runs in the explicitly supplied `workspace` directory, with closed
//! standard input and both standard output and standard error piped. The whole
//! lifecycle is bounded by the explicitly supplied `timeout`:
//!
//! - a normal exit yields [`CommandRunOutcome`] with `exit_code` and `duration`;
//! - a non-zero exit additionally carries a bounded `output_tail`;
//! - a timeout is explicitly marked ([`CommandRunOutcome::timed_out`]);
//! - a spawn or wait failure yields [`CommandRunError::Spawn`] or
//!   [`CommandRunError::Wait`].
//!
//! The child starts as the leader of its own process group through the safe
//! [`command_group::CommandGroup`] API (the standard library's
//! `process_group(0)`), mirroring the reference `start_new_session` process
//! group. A command is considered finished only when the direct child has been
//! reaped *and* both pipes have reached end of file; a descendant that inherits
//! the pipes keeps the command running until the deadline. On timeout or a wait
//! failure the whole process group is killed with `SIGKILL` and the direct child
//! is reaped before returning. On Unix the pipe read ends are non-blocking, so a
//! reader thread stops at the deadline even when a descendant that created its
//! own group/session escaped the kill and still holds an inherited pipe; the
//! reader threads are therefore always joined within one poll interval and can
//! never block the caller. Only the direct child is reaped; an in-group
//! descendant is reaped by the operating system after the group kill, exactly
//! like the reference `killpg` cleanup, while an escaped descendant survives the
//! timeout just as it does under the Python reference.
//!
//! # Output bound
//!
//! Output is never accumulated without limit: each stream is drained on its own
//! thread and only the last `tail_bytes` bytes are retained. The tail is exposed
//! only for a failed command, exactly like the reference (`output_tail` is
//! absent for a successful command), and is deterministically bounded in bytes.
//!
//! # Redaction
//!
//! [`CommandRunError`] carries no command text, argv, environment, workspace
//! path or output, so `Debug`/`Display` can never leak a sensitive input.

use std::error::Error;
use std::fmt;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bridge_command_policy::{
    PolicyReason, TestCommandReason, leading_assignments, split_command, validate_test_commands,
};
use command_group::CommandGroup;

/// Default bounded output tail in bytes, matching the reference
/// `DEFAULT_TAIL_BYTES`.
pub const DEFAULT_TAIL_BYTES: usize = 4000;

/// How often the parent polls a running child before its deadline.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Size of each read chunk while draining a child stream.
const READ_CHUNK: usize = 65_536;

/// Why a test command did not produce a normal outcome.
///
/// The error carries no command text, argv, environment, workspace path or
/// output; both `Debug` and `Display` render only a static identifier (and, for
/// [`CommandRunError::Rejected`], the payload-free policy reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CommandRunError {
    /// Verifier validation rejected the command; it was never spawned. The
    /// wrapped [`TestCommandReason`] is the stable reference category
    /// (`missing_executable`, `git_write_blocked`, ...).
    Rejected(TestCommandReason),
    /// The process could not be started.
    Spawn,
    /// Waiting for the process failed; the child was killed and reaped.
    Wait,
}

impl CommandRunError {
    /// Returns the stable, payload-free identifier for this error.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rejected(reason) => reason.as_str(),
            Self::Spawn => "spawn_failed",
            Self::Wait => "wait_failed",
        }
    }
}

impl fmt::Display for CommandRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for CommandRunError {}

/// The outcome of one executed test command.
///
/// The shape mirrors the reference per-command entry: `duration` and
/// `exit_code` describe a command that actually ran, `timed_out` marks a
/// timeout, and `output_tail` is present only for a failed command (a non-zero
/// exit or a timeout). A command rejected before running and a spawn/wait
/// failure are reported as [`CommandRunError`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRunOutcome {
    exit_code: Option<i32>,
    timed_out: bool,
    duration: Duration,
    output_tail: Option<String>,
}

impl CommandRunOutcome {
    /// Returns the process exit code, or `None` when the process was not
    /// reaped with a status. A process killed by a signal is reported as the
    /// negated signal number, matching the Python reference `returncode`
    /// (for example `-9` after `SIGKILL`).
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Returns whether the command exceeded its timeout and was terminated.
    #[must_use]
    pub fn timed_out(&self) -> bool {
        self.timed_out
    }

    /// Returns the wall-clock duration from spawn to reap.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.duration
    }

    /// Returns the bounded combined output tail, present only for a failed
    /// command.
    #[must_use]
    pub fn output_tail(&self) -> Option<&str> {
        self.output_tail.as_deref()
    }

    /// Returns whether the command exited successfully (`0`) without timing out.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// Runs one validated test command in `workspace` under a wall-clock `timeout`.
///
/// The command is validated first with the shared fail-closed verifier policy;
/// an unsafe, empty or assignment-only command is rejected as
/// [`CommandRunError::Rejected`] without spawning anything. A validated command
/// is tokenized and split into leading `NAME=value` assignments (overlaid on the
/// child environment) and argv (passed as distinct OS arguments, never through
/// a shell).
///
/// The child starts as the leader of its own process group and runs in
/// `workspace` with closed standard input and piped standard output and standard
/// error. At most the last `tail_bytes` bytes of each stream are retained; the
/// combined tail is exposed only for a failed command and is bounded in bytes.
/// A command finishes when the direct child is reaped and both pipes reach end
/// of file; on timeout or a wait failure the whole process group is killed and
/// the direct child is reaped before returning. The reader threads are bounded
/// by the same deadline (the pipe read ends are non-blocking on Unix), so they
/// are joined promptly even if a descendant escaped the process group and still
/// holds an inherited pipe.
///
/// # Errors
///
/// Returns [`CommandRunError::Rejected`] when verifier validation rejects the
/// command, [`CommandRunError::Spawn`] when the process cannot be started and
/// [`CommandRunError::Wait`] when waiting for it fails. The error carries no
/// command text, argv, environment, workspace path or output.
pub fn run_test_command(
    workspace: &Path,
    command: &str,
    timeout: Duration,
    tail_bytes: usize,
) -> Result<CommandRunOutcome, CommandRunError> {
    let problems = validate_test_commands(&[command]);
    if let Some(problem) = problems.first() {
        return Err(CommandRunError::Rejected(problem.reason()));
    }

    // The command already passed `validate_test_commands`, so tokenization
    // cannot fail here; fall back fail closed if it ever does.
    let tokens = split_command(command).map_err(|_| {
        CommandRunError::Rejected(TestCommandReason::Policy(
            PolicyReason::UnparsableBashPattern,
        ))
    })?;
    let assignments = leading_assignments(&tokens);
    let argv = assignments.argv();
    if argv.is_empty() {
        return Err(CommandRunError::Rejected(
            TestCommandReason::MissingExecutable,
        ));
    }

    let mut builder = Command::new(&argv[0]);
    builder.args(&argv[1..]);
    builder.current_dir(workspace);
    builder.stdin(Stdio::null());
    builder.stdout(Stdio::piped());
    builder.stderr(Stdio::piped());
    for (name, value) in assignments.variables() {
        builder.env(name, value);
    }

    let started = Instant::now();
    // `group_spawn` places the child in its own process group so a timeout can
    // terminate the whole group (the direct child and every descendant that did
    // not create a new group), like the reference `start_new_session` + `killpg`.
    let mut child = builder.group_spawn().map_err(|_| CommandRunError::Spawn)?;

    let Some(stdout) = child.inner().stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CommandRunError::Wait);
    };
    let Some(stderr) = child.inner().stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CommandRunError::Wait);
    };
    let deadline = started + timeout;
    // Make the pipe read ends non-blocking so a reader can stop at the deadline
    // even if a descendant that escaped the process group keeps a pipe open.
    set_nonblocking(&stdout);
    set_nonblocking(&stderr);
    let mut out_reader = TailReader::spawn(stdout, tail_bytes, deadline);
    let mut err_reader = TailReader::spawn(stderr, tail_bytes, deadline);

    let mut timed_out = false;
    let mut wait_failed = false;
    let mut status: Option<ExitStatus> = None;

    // A command is finished only when the direct child has been reaped *and*
    // both pipes have reached end of file. A descendant that inherited the pipes
    // therefore keeps the command running until the deadline, at which point the
    // whole group is killed and the readers stop at EOF or at the deadline.
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit_status)) => status = Some(exit_status),
                Ok(None) => {}
                Err(_) => {
                    wait_failed = true;
                    break;
                }
            }
        }
        if status.is_some() && out_reader.eof() && err_reader.eof() {
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }

    // On timeout or a wait failure, kill the whole process group and reap the
    // direct child. This closes inherited pipes held by in-group descendants; an
    // escaped descendant keeps its pipe, but the non-blocking readers stop at the
    // deadline, so both readers can still be joined instead of blocking the
    // caller forever. A group that has already exited is ignored (`kill` fails
    // with `ESRCH`).
    if timed_out || wait_failed {
        let _ = child.kill();
        if let Ok(reaped) = child.wait() {
            status = Some(reaped);
        }
    }

    let duration = started.elapsed();
    out_reader.join();
    err_reader.join();

    if wait_failed {
        return Err(CommandRunError::Wait);
    }

    let exit_code = status.as_ref().and_then(exit_code_of);
    let failed = timed_out || exit_code != Some(0);
    let output_tail =
        failed.then(|| combined_tail(&out_reader.bytes(), &err_reader.bytes(), tail_bytes));

    Ok(CommandRunOutcome {
        exit_code,
        timed_out,
        duration,
        output_tail,
    })
}

/// Returns the exit code of `status`, mapping a signal death to its negated
/// signal number like the Python reference `returncode`.
#[cfg(unix)]
fn exit_code_of(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
}

/// Returns the exit code of `status` on platforms without Unix signals.
#[cfg(not(unix))]
fn exit_code_of(status: &ExitStatus) -> Option<i32> {
    status.code()
}

/// Combines the two stream tails exactly like the reference: stdout first,
/// then a single newline when both are non-empty, then stderr, truncated to the
/// last `limit` bytes.
fn combined_tail(stdout: &[u8], stderr: &[u8], limit: usize) -> String {
    let mut combined = Vec::with_capacity(stdout.len() + stderr.len() + 1);
    combined.extend_from_slice(stdout);
    if !stdout.is_empty() && !stderr.is_empty() {
        combined.push(b'\n');
    }
    combined.extend_from_slice(stderr);
    bounded_tail(&combined, limit)
}

/// Returns the last `limit` bytes of `bytes` decoded lossily, with the decoded
/// byte length also bounded by `limit`.
fn bounded_tail(bytes: &[u8], limit: usize) -> String {
    let slice = if bytes.len() > limit {
        &bytes[bytes.len() - limit..]
    } else {
        bytes
    };
    let mut text = String::from_utf8_lossy(slice).into_owned();
    if text.len() > limit {
        // A lossy decode can expand a truncated multi-byte character into a
        // replacement character; drop leading characters until the byte bound
        // holds again.
        let mut start = 0;
        while start < text.len() && text.len() - start > limit {
            let step = text[start..].chars().next().map_or(1, char::len_utf8);
            start += step;
        }
        text = text[start..].to_owned();
    }
    text
}

/// Marks a child pipe read end non-blocking on Unix so a reader thread can stop
/// at the deadline even when a descendant that escaped the process group keeps
/// the write end open.
///
/// On platforms without `fcntl` the stream stays blocking; there the group kill
/// closes the pipes of every descendant that stayed in the process group, which
/// is the only case the contract covers.
#[cfg(unix)]
fn set_nonblocking<R: std::os::fd::AsRawFd>(stream: &R) {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    let fd = stream.as_raw_fd();
    if let Ok(flags) = fcntl(fd, FcntlArg::F_GETFL) {
        let flags = OFlag::from_bits_truncate(flags);
        let _ = fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK));
    }
}

/// No-op on platforms without non-blocking pipe reads.
#[cfg(not(unix))]
fn set_nonblocking<R>(_stream: &R) {}

/// A bounded rolling window over one child stream, filled by a drain thread.
struct TailReader {
    buffer: Arc<Mutex<Vec<u8>>>,
    eof: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TailReader {
    /// Spawns a thread that drains `stream` and keeps the last `limit` bytes in
    /// `buffer`. The thread stops at end of file or at `deadline`, so it always
    /// terminates even when the write end is held by a descendant that escaped
    /// the process group. `eof` is set only on end of file (or a read error), so
    /// the caller can distinguish a finished stream from a deadline stop.
    fn spawn<R: Read + Send + 'static>(mut stream: R, limit: usize, deadline: Instant) -> Self {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let eof = Arc::new(AtomicBool::new(false));
        let buffer_writer = Arc::clone(&buffer);
        let eof_writer = Arc::clone(&eof);
        let handle = thread::spawn(move || {
            let mut chunk = [0_u8; READ_CHUNK];
            loop {
                if Instant::now() >= deadline {
                    break;
                }
                match stream.read(&mut chunk) {
                    Ok(0) => {
                        eof_writer.store(true, Ordering::Release);
                        break;
                    }
                    Ok(read) => {
                        if let Ok(mut guard) = buffer_writer.lock() {
                            guard.extend_from_slice(&chunk[..read]);
                            if guard.len() > limit {
                                let overflow = guard.len() - limit;
                                guard.drain(..overflow);
                            }
                        }
                    }
                    Err(ref error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(POLL_INTERVAL);
                    }
                    Err(_) => {
                        eof_writer.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        });
        Self {
            buffer,
            eof,
            handle: Some(handle),
        }
    }

    /// Returns whether the stream reached end of file (or a read error) before
    /// the deadline.
    fn eof(&self) -> bool {
        self.eof.load(Ordering::Acquire)
    }

    /// Joins the drain thread. The thread stops at end of file or at the
    /// deadline, so it cannot stay blocked on an inherited pipe.
    fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Returns a snapshot of the retained tail bytes.
    fn bytes(&self) -> Vec<u8> {
        self.buffer
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandRunError, CommandRunOutcome, DEFAULT_TAIL_BYTES, run_test_command};
    use bridge_command_policy::{PolicyReason, TestCommandReason};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bridge-verifier-test-{}-{tag}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Returns whether any process on the system currently has `marker` as one
    /// of its command-line arguments.
    #[cfg(target_os = "linux")]
    fn process_has_argument(marker: &str) -> bool {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline"))
                && bytes
                    .split(|byte| *byte == 0)
                    .any(|arg| arg == marker.as_bytes())
            {
                return true;
            }
        }
        false
    }

    fn run(
        dir: &Path,
        command: &str,
        timeout: Duration,
        tail_bytes: usize,
    ) -> Result<CommandRunOutcome, CommandRunError> {
        run_test_command(dir, command, timeout, tail_bytes)
    }

    #[test]
    fn successful_command_reports_zero_exit_and_no_output() {
        let dir = TempDir::new("ok");
        let outcome = run(
            dir.path(),
            "true",
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("true should run");
        assert!(outcome.succeeded());
        assert_eq!(outcome.exit_code(), Some(0));
        assert!(!outcome.timed_out());
        assert_eq!(outcome.output_tail(), None);
        assert!(outcome.duration() <= Duration::from_secs(5));
    }

    #[test]
    fn nonzero_exit_is_reported_with_an_empty_tail() {
        let dir = TempDir::new("nonzero");
        let outcome = run(
            dir.path(),
            "false",
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("false should run");
        assert!(!outcome.succeeded());
        assert_eq!(outcome.exit_code(), Some(1));
        assert!(!outcome.timed_out());
        assert_eq!(outcome.output_tail(), Some(""));
    }

    #[test]
    fn command_runs_in_the_given_workspace() {
        let dir = TempDir::new("cwd");
        let outcome = run(
            dir.path(),
            "touch created.txt",
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("touch should run");
        assert!(outcome.succeeded());
        assert!(dir.path().join("created.txt").is_file());
    }

    #[test]
    fn leading_assignments_are_added_to_the_environment() {
        let dir = TempDir::new("env");
        let name = "BRIDGE_VERIFIER_TEST_VAR_5c1f";
        let with_assignment = format!("{name}=hello printenv {name}");
        let outcome = run(
            dir.path(),
            &with_assignment,
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("printenv should run");
        assert!(outcome.succeeded(), "assignment should be visible");

        let without = format!("printenv {name}");
        let outcome = run(
            dir.path(),
            &without,
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("printenv should run");
        assert!(!outcome.succeeded(), "unset variable should fail");
        assert_eq!(outcome.exit_code(), Some(1));
    }

    #[test]
    fn assignment_only_command_is_rejected_without_running() {
        let dir = TempDir::new("assign-only");
        let outcome = run(
            dir.path(),
            "FOO=bar",
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        );
        assert_eq!(
            outcome,
            Err(CommandRunError::Rejected(
                TestCommandReason::MissingExecutable
            ))
        );
    }

    #[test]
    fn unsafe_command_is_rejected_without_running() {
        let dir = TempDir::new("unsafe");
        let marker = dir.path().join("marker.txt");
        let command = format!("touch {} && touch second", marker.display());
        let outcome = run(
            dir.path(),
            &command,
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        );
        assert_eq!(
            outcome,
            Err(CommandRunError::Rejected(TestCommandReason::Policy(
                PolicyReason::UnprovableShellSyntax
            )))
        );
        assert!(!marker.exists(), "rejected command must not run");
    }

    #[test]
    fn git_write_is_rejected_without_running() {
        let dir = TempDir::new("git-write");
        let outcome = run(
            dir.path(),
            "git push origin main",
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        );
        assert_eq!(
            outcome,
            Err(CommandRunError::Rejected(TestCommandReason::Policy(
                PolicyReason::GitWriteBlocked
            )))
        );
    }

    #[test]
    fn missing_executable_is_a_spawn_error() {
        let dir = TempDir::new("spawn");
        let outcome = run(
            dir.path(),
            "bridge-verifier-no-such-executable-2f7a",
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        );
        assert_eq!(outcome, Err(CommandRunError::Spawn));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_and_reaps_the_child() {
        let dir = TempDir::new("timeout");
        let start = Instant::now();
        let outcome = run(
            dir.path(),
            "sleep 98765.4321",
            Duration::from_millis(200),
            DEFAULT_TAIL_BYTES,
        )
        .expect("sleep should run");
        assert!(outcome.timed_out());
        assert_eq!(outcome.exit_code(), Some(-9));
        assert!(start.elapsed() < Duration::from_secs(5));

        #[cfg(target_os = "linux")]
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            while process_has_argument("98765.4321") && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                !process_has_argument("98765.4321"),
                "timed-out child should have been killed and reaped"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_the_whole_process_group() {
        let dir = TempDir::new("group");
        // `xargs` runs `sleep <value>` as a child in the same process group, so
        // this command has both a direct child (xargs) and a long-lived
        // descendant (sleep). The unique list-file name marks the direct child
        // and the sleep argument marks the descendant. The command contains no
        // shell metacharacters, so it passes the fail-closed policy.
        let marker = format!("bridge-verifier-group-{}.txt", std::process::id());
        std::fs::write(dir.path().join(&marker), b"98765.4322\n").expect("write xargs input");
        // `-t` traces the spawned child on stderr, proving the descendant was
        // really started rather than the test passing on a childless `sleep`.
        let command = format!("xargs -t -a {marker} -n1 sleep");

        let outcome = run(
            dir.path(),
            &command,
            Duration::from_millis(300),
            DEFAULT_TAIL_BYTES,
        )
        .expect("xargs should run");
        assert!(outcome.timed_out());
        assert_eq!(outcome.exit_code(), Some(-9));
        let tail = outcome.output_tail().expect("failed command has a tail");
        assert!(
            tail.contains("98765.4322"),
            "the descendant was never spawned: {tail:?}"
        );

        #[cfg(target_os = "linux")]
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            while (process_has_argument(&marker) || process_has_argument("98765.4322"))
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                !process_has_argument(&marker),
                "the direct child survived the process-group kill"
            );
            assert!(
                !process_has_argument("98765.4322"),
                "the descendant survived the process-group kill"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn escaped_descendant_does_not_block_the_timeout() {
        let dir = TempDir::new("escape");
        // `setsid` forks and starts the long-lived `sleep` in a new session, so
        // the descendant escapes the child's process group while still holding
        // the inherited stdout/stderr pipes. The reader threads must stop at the
        // deadline instead of blocking the call until that descendant exits.
        let start = Instant::now();
        let outcome = run(
            dir.path(),
            "setsid sleep 3",
            Duration::from_millis(200),
            DEFAULT_TAIL_BYTES,
        )
        .expect("setsid should run");
        assert!(outcome.timed_out());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the runner blocked on a descendant that escaped the process group"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_tail_is_bounded() {
        let dir = TempDir::new("tail");
        // `yes` produces an unbounded stream, so a short timeout forces a
        // failed command with more output than the tail may retain.
        let outcome =
            run(dir.path(), "yes", Duration::from_millis(200), 64).expect("yes should run");
        assert!(outcome.timed_out());
        let tail = outcome.output_tail().expect("failed command has a tail");
        assert!(tail.len() <= 64, "tail must be bounded: {}", tail.len());
        assert!(
            tail.chars().all(|ch| ch == 'y' || ch == '\n'),
            "tail must be a real stream suffix: {tail:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn combined_tail_contains_both_streams() {
        let dir = TempDir::new("both");
        std::fs::write(dir.path().join("probe.txt"), b"probe\n").expect("write probe");
        let command = "sh -c 'ls . /nonexistent-bridge-verifier-xyz'";
        let outcome = run(
            dir.path(),
            command,
            Duration::from_secs(10),
            DEFAULT_TAIL_BYTES,
        )
        .expect("ls should run");
        assert!(!outcome.succeeded());
        assert!(
            outcome.exit_code().is_some_and(|code| code != 0),
            "ls should exit non-zero: {:?}",
            outcome.exit_code()
        );
        let tail = outcome.output_tail().expect("failed command has a tail");
        assert!(
            tail.contains("probe.txt"),
            "stdout must be captured: {tail:?}"
        );
        assert!(
            tail.contains("nonexistent-bridge-verifier-xyz"),
            "stderr must be captured: {tail:?}"
        );
        assert!(tail.len() <= DEFAULT_TAIL_BYTES);
    }

    #[test]
    fn errors_do_not_reveal_sensitive_inputs() {
        let dir = TempDir::new("redact");
        let error = run(
            dir.path(),
            "SUPER_SECRET_VALUE=leaked git push",
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("git push must be rejected");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("SUPER_SECRET_VALUE"));
        assert!(!rendered.contains("leaked"));
        assert!(!rendered.contains("push"));
        assert!(!rendered.contains(&dir.path().display().to_string()));

        let error = run(
            dir.path(),
            "bridge-verifier-secret-executable-9f2c",
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("missing executable must fail");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains(&dir.path().display().to_string()));
    }

    #[test]
    fn error_identifiers_are_stable() {
        assert_eq!(CommandRunError::Spawn.as_str(), "spawn_failed");
        assert_eq!(CommandRunError::Wait.as_str(), "wait_failed");
        assert_eq!(
            CommandRunError::Rejected(TestCommandReason::MissingExecutable).as_str(),
            "missing_executable"
        );
        assert_eq!(CommandRunError::Spawn.to_string(), "spawn_failed");
    }
}
