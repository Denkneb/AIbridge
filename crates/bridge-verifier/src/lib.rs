//! Verifier command runners, workspace fingerprints, side-effect detection and
//! persist-once orchestration (tasks 5.2-5.6).
//!
//! This crate implements five narrow production primitives over the shared
//! fail-closed command policy, the read-only Git snapshot layer and the atomic
//! storage verifier lifecycle:
//!
//! - [`run_test_command`] runs one already-agreed test command string and
//!   returns a typed outcome compatible in meaning with the reference Python
//!   verifier's per-command entry (`verifier.py:_run_command`);
//! - [`run_test_command_sequence`] runs an agreed list of commands strictly in
//!   order, stopping at the first failed command, and returns only the commands
//!   that actually ran;
//! - [`run_test_command_sequence_fingerprinted`] wraps such a sequence with the
//!   Git fingerprint of the workspace captured immediately before and after
//!   it, reusing the read-only [`bridge_git::take_snapshot`] instead of
//!   duplicating any Git snapshot or fingerprint logic;
//! - [`FingerprintedSequenceOutcome::side_effects`] compares the worktree
//!   manifests captured by those two snapshots into a typed
//!   [`WorkspaceSideEffects`] that lists the repository-relative paths created
//!   or modified by the run, matching the frozen reference
//!   `Verification.side_effects` contract without inferring paths from the
//!   aggregate fingerprint digest;
//! - [`run_round_verification_persisted`] links that verification flow to the
//!   existing atomic storage lifecycle ([`bridge_storage::StorageConnection::begin_verifier`]
//!   and [`bridge_storage::StorageConnection::complete_verifier`]): a finished
//!   persisted result is reused without spawning anything, and a fresh result is
//!   converted into the compact [`bridge_domain::Verification`] and persisted
//!   exactly once, so a concurrent or recovery replay is idempotent and a
//!   conflicting result fails closed.
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
//! # Sequence
//!
//! [`run_test_command_sequence`] runs an agreed list of commands strictly in
//! input order over the single-command primitive [`run_test_command`]. Exactly
//! like the reference `verifier.run_round_verification`, the **whole** list is
//! validated up front with [`bridge_command_policy::validate_test_commands`]
//! before anything is spawned: a rejected command anywhere fails the list
//! closed ([`CommandSequenceError::Rejected`]) and no command runs, so
//! validation can never cause a partial run. An empty list is valid and yields
//! an empty, successful [`TestCommandSequenceOutcome`].
//!
//! The sequence stops immediately at the first non-zero exit or timeout; the
//! commands after it are never spawned. The outcome records only the commands
//! that actually ran, in order, and carries no command text. Overall
//! success/failure is available through [`TestCommandSequenceOutcome::succeeded`];
//! a command that passed validation but could not be spawned or waited on stops
//! the sequence as [`CommandSequenceError::Run`].
//!
//! # Fingerprints
//!
//! [`run_test_command_sequence_fingerprinted`] reproduces the reference
//! `verifier.run_round_verification` order exactly:
//!
//! 1. the **whole** list is validated up front; a rejected command anywhere
//!    fails as [`FingerprintedSequenceError::Rejected`] before any fingerprint
//!    is captured and before any command runs;
//! 2. an empty list is valid and returns a successful outcome **without**
//!    capturing any fingerprint (the reference returns before
//!    fingerprinting);
//! 3. the before fingerprint is captured through one read-only
//!    [`bridge_git::take_snapshot`]; a Git failure fails closed as
//!    [`FingerprintedSequenceError::BeforeSnapshot`] and guarantees that not a
//!    single test command was spawned;
//! 4. the commands run strictly in order through the same shared loop as the
//!    task 5.3 sequence, with the same `timeout` and `tail_bytes`, so the
//!    stop-at-the-first-failure semantics are exactly those of task 5.3. A
//!    validated command that cannot be spawned or waited on is recorded in
//!    the outcome like the reference `spawn_failed` command entry — the
//!    commands that already ran and the typed
//!    [`FingerprintedRunFailure`] are retained — and the loop stops;
//! 5. the after fingerprint is captured for every non-empty list, including
//!    failed, timed-out and spawn/wait-failed sequences. A Git failure there
//!    does not discard the run: like the reference `error` variant with
//!    reason `git_fingerprint_failed`, the outcome keeps the recorded
//!    commands, the run failure and the before fingerprint while `after`
//!    stays `None`, and the typed overall status
//!    [`FingerprintedSequenceStatus::GitFingerprintFailed`] — which
//!    [`FingerprintedSequenceOutcome::succeeded`] never reports as success —
//!    exposes the failure.
//!
//! [`WorkspaceFingerprint`] records exactly the reference compact triple — the
//! HEAD commit, the index fingerprint and the worktree fingerprint — taken from
//! that single bridge-git snapshot and exposed through typed accessors.
//!
//! # Side effects
//!
//! [`FingerprintedSequenceOutcome::side_effects`] is exactly the frozen
//! reference `Verification.side_effects`: the repository-relative paths created
//! or modified by the run. It is computed from the before and after worktree
//! manifests captured by [`bridge_git::take_snapshot`] — the same manifest data
//! that feeds the worktree fingerprint — never from the aggregate fingerprint
//! digest alone:
//!
//! - an entry whose after digest differs from its before digest is reported;
//! - an entry that exists only after the run (created) is reported;
//! - an entry that exists only before the run (deleted) is reported.
//!
//! The resulting paths are sorted and deduplicated and are carried as raw
//! [`OsString`] bytes, so a non-UTF-8 repository-relative path survives
//! unchanged (for example `.pytest_cache/v`). The comparison is independent of
//! how the commands went: a non-zero exit, a timeout or a recorded spawn/wait
//! failure still compares the captured snapshots.
//!
//! A clean non-empty run reports an empty [`WorkspaceSideEffects`] (`Some` with
//! no paths). An empty command list captures no fingerprints and reports no
//! side effects (`side_effects` is `None`). A failed after Git snapshot leaves
//! the after fingerprint unavailable; it remains the infrastructure failure
//! reported by [`FingerprintedSequenceOutcome::after_snapshot_failed`] and
//! [`FingerprintedSequenceStatus::GitFingerprintFailed`], and `side_effects`
//! stays `None` rather than being misclassified as a clean path result.
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
//! [`CommandRunError`], [`CommandSequenceError`],
//! [`FingerprintedSequenceError`], [`FingerprintedRunFailure`],
//! [`FingerprintedSequenceStatus`], [`TestCommandSequenceOutcome`],
//! [`FingerprintedSequenceOutcome`] and [`WorkspaceSideEffects`] carry no
//! command text, argv, environment, workspace path, output or fingerprint
//! values, so `Debug`/`Display` can never leak a sensitive input. The sequence
//! outcome exposes per-command output only through the explicit
//! [`CommandRunOutcome::output_tail`] accessor, never through its `Debug`
//! rendering; the fingerprint values are likewise available only through the
//! explicit [`WorkspaceFingerprint`] accessors, never through a `Debug`
//! rendering of a verification result. The exact side-effect paths are available
//! only through the explicit [`WorkspaceSideEffects::paths`] accessor; their
//! `Debug`/`Display` rendering reports only the number of paths.
//!
//! # Persist-once
//!
//! [`run_round_verification_persisted`] is the narrow orchestration boundary
//! between the verifier flow above and the existing atomic storage lifecycle
//! (task 3.9b). It neither re-implements command running or fingerprinting nor
//! touches SQLite directly; it composes
//! [`run_test_command_sequence_fingerprinted`] with
//! [`bridge_storage::StorageConnection::begin_verifier`] and
//! [`bridge_storage::StorageConnection::complete_verifier`]:
//!
//! 1. **begin first.** [`bridge_storage::StorageConnection::begin_verifier`] is
//!    called before any command. When the round is already
//!    [`bridge_domain::VerifierState::Done`], the stored
//!    [`bridge_domain::Verification`] is reused and **no** test command is ever
//!    spawned ([`PersistedVerificationOutcome::Reused`]);
//! 2. **run and convert.** For a new or `running` verifier the exact reference
//!    order of [`run_test_command_sequence_fingerprinted`] is preserved
//!    (pre-validation of the whole list, before fingerprint, commands, after
//!    fingerprint) and its typed outcome is converted into the compact
//!    [`bridge_domain::Verification`]: `passed`/`failed`/`timed_out` for a run,
//!    `unsafe` with `index`/`reason` for a rejected list and `error` with
//!    `git_fingerprint_failed` for a failed fingerprint (a before-snapshot
//!    failure keeps no commands/fingerprints, while an after-snapshot failure
//!    keeps the commands that ran and the before fingerprint with `after`/
//!    `side_effects` absent);
//! 3. **persist once.** The converted verification is handed to
//!    [`bridge_storage::StorageConnection::complete_verifier`], which persists
//!    it exactly once. An identical already-persisted result is replayed
//!    ([`PersistedVerificationOutcome::Replayed`]) and a **different** one fails
//!    closed as [`PersistVerificationError::Conflict`] without overwriting the
//!    stored result.
//!
//! The conversion only builds the compact persisted record; it never re-runs a
//! command or recomputes a fingerprint. `Debug`/`Display` of
//! [`PersistedVerification`], [`PersistedVerificationOutcome`] and
//! [`PersistVerificationError`] are redacting: they never render task/project
//! ids, commands, paths, output, fingerprints, SQL or secrets.
//!
use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsString;
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
use bridge_domain::{
    GitFingerprint, Verification, VerificationCommand, VerificationRepository, VerificationStatus,
};
use bridge_git::{CommitId, IndexFingerprint, WorktreeFingerprint, WorktreeManifest};
use bridge_storage::{
    CompleteVerifierInput, RoundRef, RoundUpdateError, RoundUpdateOutcome, StorageConnection,
    VerifierUpdateOutcome,
};
use command_group::CommandGroup;

/// Default bounded output tail in bytes, matching the reference
/// `DEFAULT_TAIL_BYTES`.
pub const DEFAULT_TAIL_BYTES: usize = 4000;

/// The stable reference reason persisted for a verifier Git fingerprint
/// failure (`status=error`), matching the frozen Python contract.
const GIT_FINGERPRINT_FAILED: &str = "git_fingerprint_failed";

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

/// Why an agreed sequence of test commands did not complete.
///
/// The error carries no command text, argv, environment, workspace path or
/// output; both `Debug` and `Display` render only an index and a static,
/// payload-free identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CommandSequenceError {
    /// The **whole** list failed verifier validation before any command ran.
    ///
    /// Like the reference `verifier.run_round_verification`, the list is
    /// validated up front, so a rejected command anywhere prevents every
    /// command from running. `index` is the zero-based position of the first
    /// rejected command and `reason` is its payload-free category.
    Rejected {
        /// Zero-based position of the first rejected command.
        index: usize,
        /// Payload-free rejection category.
        reason: TestCommandReason,
    },
    /// A command that passed validation could not be spawned or waited on.
    ///
    /// The sequence stops and the commands after it are not run. `index` is the
    /// zero-based position of the command that could not run.
    Run {
        /// Zero-based position of the command that could not run.
        index: usize,
        /// Payload-free single-command failure.
        error: CommandRunError,
    },
}

impl CommandSequenceError {
    /// Returns the zero-based index of the command that stopped the sequence.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Rejected { index, .. } | Self::Run { index, .. } => index,
        }
    }

    /// Returns the stable, payload-free identifier for this error.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rejected { reason, .. } => reason.as_str(),
            Self::Run { error, .. } => error.as_str(),
        }
    }
}

impl fmt::Display for CommandSequenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { index, reason } => write!(f, "rejected command {index}: {reason}"),
            Self::Run { index, error } => write!(f, "command {index} did not run: {error}"),
        }
    }
}

impl Error for CommandSequenceError {}

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

/// The outcome of an agreed sequence of test commands.
///
/// The result records only the commands that actually ran, strictly in input
/// order, and never the command text. The sequence stops at the first failed
/// command, so [`succeeded`](Self::succeeded) reports the overall result and the
/// recorded outcomes describe every command up to and including that failure. An
/// empty input yields an empty result that is successful. A command rejected by
/// up-front validation or a spawn/wait failure is reported as
/// [`CommandSequenceError`] instead and is not recorded here.
///
/// `Debug` is redacting: it reports only the number of commands that ran and the
/// overall success, never command output.
#[derive(Clone, PartialEq, Eq)]
pub struct TestCommandSequenceOutcome {
    commands: Vec<CommandRunOutcome>,
}

impl TestCommandSequenceOutcome {
    /// Returns the outcomes of the commands that actually ran, in order.
    #[must_use]
    pub fn commands(&self) -> &[CommandRunOutcome] {
        &self.commands
    }

    /// Returns whether the whole sequence succeeded.
    ///
    /// True when every recorded command succeeded; an empty sequence is
    /// successful. The sequence stops at the first failure, so this is false
    /// exactly when the last recorded command failed.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.commands.iter().all(CommandRunOutcome::succeeded)
    }

    /// Returns the number of commands that actually ran.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Returns whether no command actually ran.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

impl fmt::Debug for TestCommandSequenceOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestCommandSequenceOutcome")
            .field("commands", &self.commands.len())
            .field("succeeded", &self.succeeded())
            .finish()
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

/// Runs an agreed list of test commands strictly in order in `workspace`.
///
/// The **whole** list is validated first with the shared fail-closed verifier
/// policy ([`bridge_command_policy::validate_test_commands`]); if any command is
/// rejected, the first problem fails the list closed as
/// [`CommandSequenceError::Rejected`] and **no** command runs, matching the
/// reference `verifier.run_round_verification` pre-validation of the entire
/// list. An empty list is valid and returns an empty, successful
/// [`TestCommandSequenceOutcome`].
///
/// Each validated command is then executed through [`run_test_command`] with the
/// same `timeout` and `tail_bytes`, strictly in input order. The sequence stops
/// immediately at the first non-zero exit or timeout; later commands are never
/// spawned. The returned outcome records only the commands that actually ran, in
/// order. A validated command that cannot be spawned or waited on stops the
/// sequence as [`CommandSequenceError::Run`] without running the rest.
///
/// # Errors
///
/// Returns [`CommandSequenceError::Rejected`] when up-front validation rejects
/// any command (before anything runs) and [`CommandSequenceError::Run`] when a
/// validated command cannot be spawned or waited on. Neither error carries
/// command text, argv, environment, workspace path or output.
pub fn run_test_command_sequence(
    workspace: &Path,
    commands: &[&str],
    timeout: Duration,
    tail_bytes: usize,
) -> Result<TestCommandSequenceOutcome, CommandSequenceError> {
    // Fail closed on the whole list before spawning anything, so a rejected
    // command can never leave an earlier command partially run.
    let problems = validate_test_commands(commands);
    if let Some(problem) = problems.first() {
        return Err(CommandSequenceError::Rejected {
            index: problem.index(),
            reason: problem.reason(),
        });
    }

    let (outcome, failure) =
        run_commands_stopping_at_first_failure(workspace, commands, timeout, tail_bytes);
    match failure {
        Some((index, error)) => Err(CommandSequenceError::Run { index, error }),
        None => Ok(outcome),
    }
}

/// Runs already-validated `commands` strictly in order through
/// [`run_test_command`], stopping at the first non-zero exit, timeout or
/// spawn/wait failure, and returns the outcomes of the commands that actually
/// ran plus, for a spawn/wait failure, its zero-based index and payload-free
/// error.
///
/// This is the shared loop of the task 5.3 sequence primitive and the task
/// 5.4 fingerprinted orchestration: the former maps the returned failure to
/// [`CommandSequenceError::Run`], the latter records it in the outcome like
/// the reference `spawn_failed` command entry.
fn run_commands_stopping_at_first_failure(
    workspace: &Path,
    commands: &[&str],
    timeout: Duration,
    tail_bytes: usize,
) -> (TestCommandSequenceOutcome, Option<(usize, CommandRunError)>) {
    let mut ran = Vec::with_capacity(commands.len());
    for (index, command) in commands.iter().enumerate() {
        match run_test_command(workspace, command, timeout, tail_bytes) {
            Ok(outcome) => {
                let failed = !outcome.succeeded();
                ran.push(outcome);
                if failed {
                    break;
                }
            }
            Err(error) => {
                return (
                    TestCommandSequenceOutcome { commands: ran },
                    Some((index, error)),
                );
            }
        }
    }
    (TestCommandSequenceOutcome { commands: ran }, None)
}

/// The Git fingerprint of a workspace captured around a test command sequence.
///
/// The triple is exactly the reference `verifier.fingerprint` compact record:
/// the HEAD commit, the index fingerprint and the worktree fingerprint. The
/// values, together with the worktree manifest that produced the worktree
/// fingerprint, are taken from a single read-only [`bridge_git::take_snapshot`]
/// and are never recomputed here, so this layer adds no Git logic of its own.
/// The manifest is retained privately so that [`WorkspaceSideEffects`] can list
/// the exact changed paths instead of inferring them from the aggregate digest.
///
/// `Debug` renders only whether a HEAD commit exists; the exact commit id and
/// the digests are available through the accessors, never through a debug
/// rendering of a verification result.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkspaceFingerprint {
    head: Option<CommitId>,
    index_fingerprint: IndexFingerprint,
    worktree_fingerprint: WorktreeFingerprint,
    manifest: WorktreeManifest,
}

impl WorkspaceFingerprint {
    /// Returns the HEAD commit, or `None` for a valid repository without
    /// commits.
    #[must_use]
    pub fn head(&self) -> Option<&CommitId> {
        self.head.as_ref()
    }

    /// Returns the fingerprint of the staged index.
    #[must_use]
    pub fn index_fingerprint(&self) -> &IndexFingerprint {
        &self.index_fingerprint
    }

    /// Returns the fingerprint of the tracked/untracked worktree.
    #[must_use]
    pub fn worktree_fingerprint(&self) -> &WorktreeFingerprint {
        &self.worktree_fingerprint
    }
}

impl fmt::Debug for WorkspaceFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceFingerprint")
            .field("head", &self.head.is_some())
            .finish()
    }
}

/// Returns the sorted, deduplicated repository-relative paths whose worktree
/// manifest entry changed between `before` and `after`.
///
/// This is exactly the reference `verifier._manifest_changes`: a path is
/// reported when its after digest differs from its before digest, when it
/// exists only in `after` (created) or when it exists only in `before`
/// (deleted). The aggregate worktree fingerprint digest is never used to infer
/// paths; the manifest entries are. Paths are raw [`OsString`] bytes and stay
/// repository-relative.
fn changed_paths(before: &WorktreeManifest, after: &WorktreeManifest) -> Vec<OsString> {
    let mut paths = BTreeSet::new();
    for entry in after.entries() {
        if before.digest(entry.path()) != Some(entry.digest()) {
            paths.insert(entry.path().to_os_string());
        }
    }
    for entry in before.entries() {
        if after.digest(entry.path()).is_none() {
            paths.insert(entry.path().to_os_string());
        }
    }
    paths.into_iter().collect()
}

/// The repository-relative paths created or modified by a fingerprinted command
/// sequence.
///
/// This is exactly the frozen reference `Verification.side_effects`: the
/// worktree manifest entries whose content or type changed between the before
/// and after snapshots plus every entry that was deleted, sorted and
/// deduplicated. Paths are repository-relative (for example `.pytest_cache/v`)
/// and are carried as raw [`OsString`] bytes so a non-UTF-8 path is never
/// decoded lossily.
///
/// The exact paths are exposed through [`paths`](Self::paths); `Debug` and
/// `Display` are redacting and render only the number of paths, so a
/// verification result can never leak workspace file names through its debug
/// rendering.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkspaceSideEffects {
    paths: Vec<OsString>,
}

impl WorkspaceSideEffects {
    /// Returns the sorted, deduplicated repository-relative side-effect paths.
    #[must_use]
    pub fn paths(&self) -> &[OsString] {
        &self.paths
    }

    /// Returns the number of side-effect paths.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Returns whether the sequence produced no side effects.
    ///
    /// True exactly when no repository-relative path changed. A clean non-empty
    /// run reports an empty result; no available result is `None` instead.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

impl fmt::Debug for WorkspaceSideEffects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceSideEffects")
            .field("paths", &self.paths.len())
            .finish()
    }
}

impl fmt::Display for WorkspaceSideEffects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "side_effects={}", self.paths.len())
    }
}

/// Why a fingerprinted sequence of test commands did not produce a result.
///
/// Only failures that prevent the sequence from running at all are errors:
/// everything that happens while the commands run — non-zero exits, timeouts,
/// spawn/wait failures, even an after-fingerprint failure — is recorded in the
/// returned [`FingerprintedSequenceOutcome`] instead. The error carries no
/// command text, argv, environment, workspace path, output or fingerprint
/// values; both `Debug` and `Display` render only an index and a static,
/// payload-free identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FingerprintedSequenceError {
    /// The **whole** list failed verifier validation before any fingerprint
    /// was captured or any command spawned (the reference `unsafe` variant).
    ///
    /// Like [`CommandSequenceError::Rejected`], `index` is the zero-based
    /// position of the first rejected command and `reason` is its payload-free
    /// category.
    Rejected {
        /// Zero-based position of the first rejected command.
        index: usize,
        /// Payload-free rejection category.
        reason: TestCommandReason,
    },
    /// The before Git snapshot failed, so no command ran (the reference
    /// `error` variant with reason `git_fingerprint_failed`).
    BeforeSnapshot,
}

impl FingerprintedSequenceError {
    /// Returns the stable, payload-free identifier for this error.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rejected { reason, .. } => reason.as_str(),
            Self::BeforeSnapshot => "git_fingerprint_failed",
        }
    }
}

impl fmt::Display for FingerprintedSequenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { index, reason } => write!(f, "rejected command {index}: {reason}"),
            Self::BeforeSnapshot => f.write_str("git_fingerprint_failed"),
        }
    }
}

impl Error for FingerprintedSequenceError {}

/// A validated command that could not be spawned or waited on while a
/// fingerprinted sequence was running.
///
/// The reference records such a failure as a failed command entry with
/// `reason=spawn_failed` (or `wait_failed`); this typed pair carries the same
/// information: the zero-based position of the command and the payload-free
/// failure. The commands before it kept their outcomes, the loop stopped at
/// it and the after fingerprint is still captured.
///
/// `Debug` and `Display` render only the index and a static, payload-free
/// identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FingerprintedRunFailure {
    index: usize,
    error: CommandRunError,
}

impl FingerprintedRunFailure {
    /// Returns the zero-based position of the command that could not run.
    #[must_use]
    pub fn index(&self) -> usize {
        self.index
    }

    /// Returns the payload-free single-command failure.
    #[must_use]
    pub fn error(&self) -> CommandRunError {
        self.error
    }

    /// Returns the stable, payload-free identifier for this failure.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.error.as_str()
    }
}

impl fmt::Display for FingerprintedRunFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "command {} did not run: {}", self.index, self.error)
    }
}

/// The typed overall status of a fingerprinted test command sequence,
/// mirroring the overall result of the reference `run_round_verification`.
///
/// The variants are payload-free; `Debug` and `Display` render only the stable
/// identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FingerprintedSequenceStatus {
    /// Every command succeeded, none failed to spawn or be waited on, and
    /// both fingerprints were captured. The status of an empty command list.
    Succeeded,
    /// A command failed: a non-zero exit, a timeout, or a spawn/wait failure
    /// recorded by [`FingerprintedSequenceOutcome::run_failure`]. The after
    /// fingerprint is still captured.
    Failed,
    /// The after Git snapshot failed after the commands ran: the reference
    /// overwrites the overall status with `error` and reason
    /// `git_fingerprint_failed`. The commands, any recorded run failure and
    /// the `before` fingerprint stay recorded; the `after` fingerprint does
    /// not.
    GitFingerprintFailed,
}

impl FingerprintedSequenceStatus {
    /// Returns the stable, payload-free identifier for this status.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::GitFingerprintFailed => "git_fingerprint_failed",
        }
    }
}

impl fmt::Display for FingerprintedSequenceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of a fingerprinted sequence of test commands.
///
/// The result wraps the [`TestCommandSequenceOutcome`] of the commands that
/// actually ran, an optional recorded run failure, and the Git fingerprints
/// captured immediately before and after the sequence. Exactly like the
/// reference `run_round_verification`:
///
/// - an empty command list is valid, successful and captures **no**
///   fingerprints (`before` and `after` are both `None`);
/// - a non-empty list always captures `before`; command failures (a non-zero
///   exit, a timeout, or a spawn/wait failure recorded by
///   [`run_failure`](Self::run_failure)) do not prevent the `after` capture;
/// - an `after` that stays `None` after commands ran means the after Git
///   snapshot itself failed — the reference `error` variant with reason
///   `git_fingerprint_failed` — which
///   [`after_snapshot_failed`](Self::after_snapshot_failed) reports while the
///   commands, the recorded run failure and the `before` fingerprint stay
///   recorded.
///
/// The overall result is [`status`](Self::status): the reference overwrites
/// it with `error` (reason `git_fingerprint_failed`) when the after
/// fingerprint fails, so [`succeeded`](Self::succeeded) is false then, no
/// matter how the commands went.
///
/// `Debug` is redacting: it reports only the number of commands that ran and
/// the typed overall status, never command output or fingerprint values.
#[derive(Clone, PartialEq, Eq)]
pub struct FingerprintedSequenceOutcome {
    sequence: TestCommandSequenceOutcome,
    run_failure: Option<FingerprintedRunFailure>,
    before: Option<WorkspaceFingerprint>,
    after: Option<WorkspaceFingerprint>,
    after_snapshot_failed: bool,
}

impl FingerprintedSequenceOutcome {
    /// Returns the fingerprint captured immediately before the sequence, or
    /// `None` when the command list was empty (no snapshot is taken then).
    #[must_use]
    pub fn before(&self) -> Option<&WorkspaceFingerprint> {
        self.before.as_ref()
    }

    /// Returns the fingerprint captured immediately after the sequence.
    ///
    /// `None` when the command list was empty, or when the after Git snapshot
    /// failed after the commands ran; the latter is reported by
    /// [`after_snapshot_failed`](Self::after_snapshot_failed).
    #[must_use]
    pub fn after(&self) -> Option<&WorkspaceFingerprint> {
        self.after.as_ref()
    }

    /// Returns whether the after Git snapshot failed after the commands ran.
    ///
    /// This is the reference `error` variant: the commands, any recorded run
    /// failure and the `before` fingerprint are still recorded, the `after`
    /// fingerprint is not, and the overall status is
    /// [`FingerprintedSequenceStatus::GitFingerprintFailed`].
    #[must_use]
    pub fn after_snapshot_failed(&self) -> bool {
        self.after_snapshot_failed
    }

    /// Returns the recorded failure of a validated command that could not be
    /// spawned or waited on, if one stopped the sequence.
    ///
    /// This is the reference `spawn_failed`/`wait_failed` command entry: the
    /// commands before it kept their outcomes, the loop stopped at it and the
    /// after fingerprint was still captured.
    #[must_use]
    pub fn run_failure(&self) -> Option<FingerprintedRunFailure> {
        self.run_failure
    }

    /// Returns the outcomes of the commands that actually ran, in order.
    #[must_use]
    pub fn commands(&self) -> &[CommandRunOutcome] {
        self.sequence.commands()
    }

    /// Returns the typed overall status of the round.
    ///
    /// [`FingerprintedSequenceStatus::GitFingerprintFailed`] overwrites a
    /// command failure, exactly like the reference overwrites the overall
    /// status with `error` when the after fingerprint fails.
    #[must_use]
    pub fn status(&self) -> FingerprintedSequenceStatus {
        if self.after_snapshot_failed {
            FingerprintedSequenceStatus::GitFingerprintFailed
        } else if self.run_failure.is_some() || !self.sequence.succeeded() {
            FingerprintedSequenceStatus::Failed
        } else {
            FingerprintedSequenceStatus::Succeeded
        }
    }

    /// Returns whether the whole verification round succeeded.
    ///
    /// True only for [`FingerprintedSequenceStatus::Succeeded`]: every
    /// recorded command succeeded, no command failed to spawn or be waited
    /// on, and the after fingerprint was captured. An after-fingerprint
    /// infrastructure failure is the reference `error` with reason
    /// `git_fingerprint_failed` and is never a success, even when every
    /// command passed. An empty sequence is successful.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.status() == FingerprintedSequenceStatus::Succeeded
    }

    /// Returns the repository-relative paths created or modified by the
    /// sequence, or `None` when no result is available.
    ///
    /// This is exactly the frozen reference `Verification.side_effects`: the
    /// worktree manifest entries whose content or type changed between the
    /// before and after snapshots plus every entry that was deleted, sorted and
    /// deduplicated. The paths are repository-relative (for example
    /// `.pytest_cache/v`) and are computed from the manifests captured by the
    /// two read-only snapshots, never inferred from the aggregate fingerprint
    /// digest. The comparison is independent of how the commands went: a
    /// non-zero exit, a timeout or a recorded spawn/wait failure still compares
    /// the captured snapshots.
    ///
    /// `None` means no side-effect result is available, never "no changes":
    ///
    /// - an empty command list captures no fingerprints at all and reports no
    ///   side effects;
    /// - a failed after Git snapshot leaves [`after`](Self::after) unavailable.
    ///   That remains the infrastructure failure reported by
    ///   [`after_snapshot_failed`](Self::after_snapshot_failed) and
    ///   [`status`](Self::status) ([`FingerprintedSequenceStatus::GitFingerprintFailed`])
    ///   and is never misclassified as a clean path result.
    ///
    /// A clean non-empty run reports an empty [`WorkspaceSideEffects`]
    /// (`Some` with no paths), which stays distinct from both `None` cases. The
    /// result carries only repository-relative paths, never a commit id,
    /// digest, command text or output.
    #[must_use]
    pub fn side_effects(&self) -> Option<WorkspaceSideEffects> {
        match (self.before.as_ref(), self.after.as_ref()) {
            (Some(before), Some(after)) => Some(WorkspaceSideEffects {
                paths: changed_paths(&before.manifest, &after.manifest),
            }),
            _ => None,
        }
    }
}

impl fmt::Debug for FingerprintedSequenceOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FingerprintedSequenceOutcome")
            .field("commands", &self.commands().len())
            .field("status", &self.status().as_str())
            .finish()
    }
}

/// Runs an agreed list of test commands strictly in order in `workspace` and
/// captures the Git fingerprint of `workspace` immediately before and after the
/// sequence.
///
/// The reference order is preserved exactly:
///
/// 1. the **whole** list is validated first with the shared fail-closed
///    verifier policy; a rejected command anywhere fails as
///    [`FingerprintedSequenceError::Rejected`] before any fingerprint is
///    captured and before any command runs;
/// 2. an empty list is valid and returns a successful outcome **without**
///    capturing any fingerprint, exactly like the reference early return;
/// 3. the before fingerprint is captured through one read-only
///    [`bridge_git::take_snapshot`]; a Git failure fails closed as
///    [`FingerprintedSequenceError::BeforeSnapshot`] and **no** command runs;
/// 4. the commands run strictly in order through the same shared loop as the
///    task 5.3 sequence, with the same `timeout` and `tail_bytes`, so the
///    stop-at-the-first-failure semantics are exactly those of task 5.3. A
///    validated command that cannot be spawned or waited on is recorded in
///    the outcome like the reference `spawn_failed` command entry — the
///    commands that already ran and the typed
///    [`FingerprintedRunFailure`] are retained — and the loop stops;
/// 5. the after fingerprint is captured for every non-empty list, including
///    failed, timed-out and spawn/wait-failed sequences. A Git failure here
///    does **not** discard the run: the outcome keeps the commands, the run
///    failure and the before fingerprint, leaves `after` as `None` and
///    reports the failure through
///    [`FingerprintedSequenceOutcome::after_snapshot_failed`] and the typed
///    overall status
///    [`FingerprintedSequenceStatus::GitFingerprintFailed`] — the reference
///    `error` variant with reason `git_fingerprint_failed`, which
///    [`FingerprintedSequenceOutcome::succeeded`] never reports as success.
///
/// # Errors
///
/// Returns [`FingerprintedSequenceError::Rejected`] when up-front validation
/// rejects any command (before anything runs or is fingerprinted) and
/// [`FingerprintedSequenceError::BeforeSnapshot`] when the before Git snapshot
/// fails (no command runs). Everything that happens while the commands run is
/// recorded in the returned outcome instead. No error carries command text,
/// argv, environment, workspace path, output or fingerprint values.
pub fn run_test_command_sequence_fingerprinted(
    workspace: &Path,
    commands: &[&str],
    timeout: Duration,
    tail_bytes: usize,
) -> Result<FingerprintedSequenceOutcome, FingerprintedSequenceError> {
    Ok(run_fingerprinted_repositories(workspace, &[], commands, timeout, tail_bytes)?.outcome)
}

struct RepositoryRun {
    outcome: FingerprintedSequenceOutcome,
    repositories: Vec<VerificationRepository>,
    external_failure: bool,
}
fn run_fingerprinted_repositories(
    workspace: &Path,
    external_roots: &[&Path],
    commands: &[&str],
    timeout: Duration,
    tail_bytes: usize,
) -> Result<RepositoryRun, FingerprintedSequenceError> {
    // The reference validates the entire list before the first fingerprint,
    // so a rejected command can never leave a captured fingerprint or a
    // partially run sequence behind.
    let problems = validate_test_commands(commands);
    if let Some(problem) = problems.first() {
        return Err(FingerprintedSequenceError::Rejected {
            index: problem.index(),
            reason: problem.reason(),
        });
    }

    // Fail closed before anything runs: a workspace that cannot be
    // fingerprinted must not execute a single test command. The reference
    // returns the empty list before fingerprinting, so no snapshot at all is
    // taken for an empty command list.
    let before = if commands.is_empty() {
        None
    } else {
        Some(
            capture_fingerprint(workspace)
                .map_err(|_| FingerprintedSequenceError::BeforeSnapshot)?,
        )
    };

    let mut roots = external_roots.to_vec();
    roots.sort();
    roots.dedup();
    let mut external_before = Vec::new();
    if !commands.is_empty() {
        for root in roots {
            // The caller authenticates trusted roots. Never run commands if
            // even one affected repository cannot be fingerprinted.
            external_before.push((
                root,
                capture_fingerprint(root)
                    .map_err(|_| FingerprintedSequenceError::BeforeSnapshot)?,
            ));
        }
    }

    // A spawn/wait failure is recorded like the reference `spawn_failed`
    // command entry — the run and the typed failure are retained — and the
    // loop stops; the after fingerprint below is still captured.
    let (sequence, run_failure) =
        run_commands_stopping_at_first_failure(workspace, commands, timeout, tail_bytes);

    // The after fingerprint is captured for every non-empty list, whatever
    // happened to the commands; a Git failure here keeps the run and
    // overwrites the overall status, exactly like the reference `error`
    // variant with reason `git_fingerprint_failed`.
    let (after, after_snapshot_failed) = if commands.is_empty() {
        (None, false)
    } else {
        match capture_fingerprint(workspace) {
            Ok(fingerprint) => (Some(fingerprint), false),
            Err(_) => (None, true),
        }
    };

    let mut repositories = Vec::new();
    let mut external_failure = false;
    if after.is_some() {
        for (root, before) in external_before {
            let Ok(after) = capture_fingerprint(root) else {
                external_failure = true;
                break;
            };
            let effects = changed_paths(&before.manifest, &after.manifest);
            let Some(root) = root.to_str() else {
                external_failure = true;
                break;
            };
            let effects = effects
                .iter()
                .map(|p| p.to_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>();
            let Some(effects) = effects else {
                external_failure = true;
                break;
            };
            repositories.push(VerificationRepository {
                root: root.to_owned(),
                before: to_git_fingerprint(&before),
                after: to_git_fingerprint(&after),
                side_effects: (!effects.is_empty()).then_some(effects),
            });
        }
    }
    Ok(RepositoryRun {
        outcome: FingerprintedSequenceOutcome {
            sequence,
            run_failure: run_failure.map(|(index, error)| FingerprintedRunFailure { index, error }),
            before,
            after,
            after_snapshot_failed,
        },
        repositories,
        external_failure,
    })
}

/// Captures the workspace fingerprint through one read-only bridge-git
/// snapshot; the Git logic itself is owned by `bridge-git`.
fn capture_fingerprint(workspace: &Path) -> Result<WorkspaceFingerprint, bridge_git::GitError> {
    let snapshot = bridge_git::take_snapshot(workspace)?;
    Ok(WorkspaceFingerprint {
        head: snapshot.head().cloned(),
        index_fingerprint: *snapshot.index_fingerprint(),
        worktree_fingerprint: *snapshot.worktree_fingerprint(),
        manifest: snapshot.manifest().clone(),
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

// ---------------------------------------------------------------------------
// Persist-once orchestration (task 5.6).
//
// The orchestration composes the existing fingerprinted verifier flow with the
// atomic storage verifier lifecycle (task 3.9b). It never runs a command or
// touches SQLite itself: `begin_verifier` decides whether a finished result is
// reused, the fingerprinted sequence produces the outcome, and
// `complete_verifier` persists it exactly once.
// ---------------------------------------------------------------------------

/// How a persisted verifier result was obtained (task 5.6).
///
/// The variants are payload-free; `Debug`/`Display` render only the stable
/// identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PersistedVerificationOutcome {
    /// A finished (`done`) result was already persisted; it was reused and **no**
    /// test command ran.
    Reused,
    /// This call ran the verification and persisted a new result for the first
    /// time.
    Persisted,
    /// An identical result was already persisted by a concurrent writer; the
    /// stored result was replayed without a write.
    Replayed,
}

impl PersistedVerificationOutcome {
    /// Returns the stable, payload-free identifier for this outcome.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reused => "reused",
            Self::Persisted => "persisted",
            Self::Replayed => "replayed",
        }
    }
}

impl fmt::Display for PersistedVerificationOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The result of [`run_round_verification_persisted`].
///
/// The compact [`Verification`] is always the exact persisted value (the reused
/// stored one, the newly written one or the replayed one) and the outcome
/// records which of those happened. `Debug` is redacting: it renders only the
/// outcome and the verification status, never command text, output,
/// fingerprints or paths.
#[derive(Clone, PartialEq)]
pub struct PersistedVerification {
    verification: Verification,
    outcome: PersistedVerificationOutcome,
}

impl PersistedVerification {
    /// Returns the persisted compact verification.
    #[must_use]
    pub fn verification(&self) -> &Verification {
        &self.verification
    }

    /// Returns how the persisted result was obtained.
    #[must_use]
    pub fn outcome(&self) -> PersistedVerificationOutcome {
        self.outcome
    }

    /// Returns whether a finished result was reused without running any command.
    #[must_use]
    pub fn reused(&self) -> bool {
        self.outcome == PersistedVerificationOutcome::Reused
    }

    /// Returns whether this call persisted a new result.
    #[must_use]
    pub fn persisted(&self) -> bool {
        self.outcome == PersistedVerificationOutcome::Persisted
    }

    /// Returns whether an identical already-persisted result was replayed.
    #[must_use]
    pub fn replayed(&self) -> bool {
        self.outcome == PersistedVerificationOutcome::Replayed
    }
}

impl fmt::Debug for PersistedVerification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistedVerification")
            .field("outcome", &self.outcome.as_str())
            .field("status", &self.verification.status.as_str())
            .finish()
    }
}

/// Why a persist-once verification round did not complete.
///
/// The error carries no task/project ids, commands, paths, output, fingerprints,
/// SQL or secrets. Both `Debug` and `Display` render only a stable, payload-free
/// identifier; the underlying storage error is reachable only through
/// [`Error::source`].
#[non_exhaustive]
pub enum PersistVerificationError {
    /// A storage lifecycle operation (`begin_verifier`/`complete_verifier`)
    /// failed. The wrapped error is reachable through [`Error::source`] and is
    /// never rendered by this type's `Debug`/`Display`.
    Storage(RoundUpdateError),
    /// A different verifier result is already persisted for the round; nothing
    /// was overwritten.
    Conflict,
    /// The storage returned an outcome without a persisted verification for a
    /// round that must have one.
    InvalidState,
}

impl PersistVerificationError {
    /// Returns the stable, payload-free identifier for this error.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Storage(_) => "storage_failed",
            Self::Conflict => "verifier_result_conflict",
            Self::InvalidState => "invalid_persisted_state",
        }
    }
}

impl fmt::Display for PersistVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for PersistVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PersistVerificationError")
            .field(&self.as_str())
            .finish()
    }
}

impl Error for PersistVerificationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Conflict | Self::InvalidState => None,
        }
    }
}

/// Runs a round verification and persists its compact result exactly once
/// (task 5.6).
///
/// The orchestration composes the existing primitives without duplicating any
/// command, fingerprint or storage logic:
///
/// 1. [`bridge_storage::StorageConnection::begin_verifier`] is called **before**
///    any command. When the round is already `done`, the stored
///    [`Verification`] is returned and **no** test command is spawned
///    ([`PersistedVerificationOutcome::Reused`]);
/// 2. for a new or `running` verifier the exact reference order of
///    [`run_test_command_sequence_fingerprinted`] is preserved (whole-list
///    pre-validation, before fingerprint, commands, after fingerprint). Its
///    typed outcome is converted into the compact [`Verification`]:
///    `passed`/`failed`/`timed_out` for a run, `unsafe` with `index`/`reason`
///    for a rejected list and `error` with `git_fingerprint_failed` for a
///    failed fingerprint. A **before**-snapshot failure fails closed with no
///    commands or fingerprints; an **after**-snapshot failure keeps the
///    commands that ran (including a recorded `spawn_failed`/`wait_failed`
///    entry) and the `before` fingerprint, leaves `after`/`side_effects` absent
///    and only overwrites the status, exactly like the frozen 5.4 contract. The
///    log reference is the frozen `verification/<task_id>/round_<round_number>`;
/// 3. the converted verification is handed to
///    [`bridge_storage::StorageConnection::complete_verifier`], which persists
///    it exactly once. An identical already-persisted result is replayed
///    ([`PersistedVerificationOutcome::Replayed`]); a different one fails closed
///    as [`PersistVerificationError::Conflict`] without overwriting the stored
///    result.
///
/// A valid repository without commits has no HEAD; the compact persisted
/// contract models `head` as a required string, so it is rendered as the empty
/// string in [`GitFingerprint::head`]. Non-UTF-8 side-effect paths are rendered
/// lossily in the string-typed compact contract.
///
/// # Errors
///
/// Returns [`PersistVerificationError::Storage`] when a storage lifecycle call
/// fails, [`PersistVerificationError::Conflict`] when a different result is
/// already persisted and [`PersistVerificationError::InvalidState`] when the
/// storage reports an outcome without a persisted verification. No error
/// carries task/project ids, commands, paths, output, fingerprints, SQL or
/// secrets.
pub fn run_round_verification_persisted(
    storage: &mut StorageConnection,
    round: RoundRef,
    workspace: &Path,
    commands: &[&str],
    timeout: Duration,
    tail_bytes: usize,
) -> Result<PersistedVerification, PersistVerificationError> {
    run_round_verification_persisted_with_repositories(
        storage,
        round,
        workspace,
        &[],
        commands,
        timeout,
        tail_bytes,
    )
}

/// Runs commands in the main workspace, fingerprinting every affected root.
/// The caller authenticates canonical external roots and retains worker fences.
/// # Errors
/// Storage lifecycle/conflict failures fail closed; fingerprint failures are
/// persisted as reviewable error outcomes. A completed result is reused intact.
pub fn run_round_verification_persisted_with_repositories(
    storage: &mut StorageConnection,
    round: RoundRef,
    workspace: &Path,
    external_roots: &[&Path],
    commands: &[&str],
    timeout: Duration,
    tail_bytes: usize,
) -> Result<PersistedVerification, PersistVerificationError> {
    // Begin first: a finished result is reused without spawning anything.
    let begun = storage
        .begin_verifier(round.clone())
        .map_err(map_storage_error)?;
    if matches!(begun, VerifierUpdateOutcome::AlreadyDone(_)) {
        return Ok(PersistedVerification {
            verification: persisted_verification(begun.outcome())?,
            outcome: PersistedVerificationOutcome::Reused,
        });
    }

    // Run the existing fingerprinted flow and convert its outcome into the
    // compact persisted contract; verifier-level failures become the reference
    // `unsafe`/`error` verification variants instead of errors.
    let verification = match run_fingerprinted_repositories(
        workspace,
        external_roots,
        commands,
        timeout,
        tail_bytes,
    ) {
        Ok(run) => {
            let mut verification = verification_from_outcome(&run.outcome, commands, &round);
            if run.external_failure {
                verification.status = VerificationStatus::Error;
                verification.reason = Some(GIT_FINGERPRINT_FAILED.to_owned());
                verification.side_effects = None;
            } else if run.outcome.after().is_some() {
                let mut effects = verification.side_effects.take().unwrap_or_default();
                for repo in &run.repositories {
                    for path in repo.side_effects.iter().flatten() {
                        effects.push(format!("{}/{path}", repo.root));
                    }
                }
                effects.sort();
                effects.dedup();
                verification.side_effects = (!effects.is_empty()).then_some(effects);
            }
            verification.repositories = (!run.repositories.is_empty()).then_some(run.repositories);
            verification
        }
        Err(FingerprintedSequenceError::Rejected { index, reason }) => {
            rejected_verification(index, reason, &round)
        }
        Err(FingerprintedSequenceError::BeforeSnapshot) => error_verification(&round),
    };

    // Persist exactly once; the storage rejects a conflicting result.
    let completed = storage
        .complete_verifier(CompleteVerifierInput {
            round,
            verification,
        })
        .map_err(map_storage_error)?;

    let verification = persisted_verification(completed.outcome())?;
    let outcome = match completed {
        VerifierUpdateOutcome::Completed(_) => PersistedVerificationOutcome::Persisted,
        VerifierUpdateOutcome::Replayed(_) | VerifierUpdateOutcome::AlreadyDone(_) => {
            PersistedVerificationOutcome::Replayed
        }
        // `complete_verifier` never produces a running outcome; fail closed
        // instead of panicking if the contract is ever violated.
        VerifierUpdateOutcome::Started(_) | VerifierUpdateOutcome::AlreadyRunning(_) => {
            return Err(PersistVerificationError::InvalidState);
        }
    };

    Ok(PersistedVerification {
        verification,
        outcome,
    })
}

/// Maps a storage lifecycle failure to the typed orchestration error, keeping
/// the fail-closed conflict category distinct from other storage failures.
fn map_storage_error(error: RoundUpdateError) -> PersistVerificationError {
    match error {
        RoundUpdateError::VerifierResultConflict => PersistVerificationError::Conflict,
        other => PersistVerificationError::Storage(other),
    }
}

/// Reads the persisted verification from a storage outcome, failing closed when
/// the storage returned a round without one.
fn persisted_verification(
    outcome: &RoundUpdateOutcome,
) -> Result<Verification, PersistVerificationError> {
    outcome
        .round
        .verifier_json
        .clone()
        .ok_or(PersistVerificationError::InvalidState)
}

/// Converts a completed fingerprinted sequence into the compact persisted
/// verification, preserving the reference `failed`/`timed_out`/`error`
/// semantics.
fn verification_from_outcome(
    outcome: &FingerprintedSequenceOutcome,
    commands: &[&str],
    round: &RoundRef,
) -> Verification {
    match outcome.status() {
        FingerprintedSequenceStatus::Succeeded => {
            normal_verification(VerificationStatus::Passed, outcome, commands, round)
        }
        FingerprintedSequenceStatus::Failed => {
            let status = if last_command_timed_out(outcome) {
                VerificationStatus::TimedOut
            } else {
                VerificationStatus::Failed
            };
            normal_verification(status, outcome, commands, round)
        }
        FingerprintedSequenceStatus::GitFingerprintFailed => {
            after_snapshot_error_verification(outcome, commands, round)
        }
    }
}

/// Returns whether the command that stopped the sequence timed out; a
/// spawn/wait failure is reported as `failed`, never `timed_out`.
fn last_command_timed_out(outcome: &FingerprintedSequenceOutcome) -> bool {
    outcome
        .commands()
        .last()
        .is_some_and(CommandRunOutcome::timed_out)
}

/// Builds the compact verification for a completed command run.
fn normal_verification(
    status: VerificationStatus,
    outcome: &FingerprintedSequenceOutcome,
    commands: &[&str],
    round: &RoundRef,
) -> Verification {
    Verification {
        status,
        commands: Some(command_entries(outcome, commands)),
        index: None,
        reason: None,
        log: verification_log_reference(round),
        before: outcome.before().map(to_git_fingerprint),
        after: outcome.after().map(to_git_fingerprint),
        side_effects: side_effect_paths(outcome),
        repositories: None,
    }
}

/// Builds the compact reference `unsafe` verification for a rejected list.
fn rejected_verification(
    index: usize,
    reason: TestCommandReason,
    round: &RoundRef,
) -> Verification {
    Verification {
        status: VerificationStatus::Unsafe,
        commands: None,
        index: Some(index as i64),
        reason: Some(reason.as_str().to_owned()),
        log: verification_log_reference(round),
        before: None,
        after: None,
        side_effects: None,
        repositories: None,
    }
}

/// Builds the compact reference `error` verification for a Git fingerprint
/// failure **before** the commands ran (`git_fingerprint_failed`).
///
/// The reference fails closed at the before snapshot: no command ran, so there
/// is no command entry and no captured fingerprint to preserve.
fn error_verification(round: &RoundRef) -> Verification {
    Verification {
        status: VerificationStatus::Error,
        commands: None,
        index: None,
        reason: Some(GIT_FINGERPRINT_FAILED.to_owned()),
        log: verification_log_reference(round),
        before: None,
        after: None,
        side_effects: None,
        repositories: None,
    }
}

/// Builds the compact reference `error` verification for a Git fingerprint
/// failure **after** the commands ran (`git_fingerprint_failed`).
///
/// This is the frozen 5.4 contract: the after snapshot failed, so the overall
/// status is `error` with reason `git_fingerprint_failed`, but the run is not
/// discarded — the commands that actually ran (including a recorded
/// `spawn_failed`/`wait_failed` entry) and the `before` fingerprint stay
/// recorded, while `after` and `side_effects` are absent.
fn after_snapshot_error_verification(
    outcome: &FingerprintedSequenceOutcome,
    commands: &[&str],
    round: &RoundRef,
) -> Verification {
    Verification {
        status: VerificationStatus::Error,
        commands: Some(command_entries(outcome, commands)),
        index: None,
        reason: Some(GIT_FINGERPRINT_FAILED.to_owned()),
        log: verification_log_reference(round),
        before: outcome.before().map(to_git_fingerprint),
        after: None,
        side_effects: None,
        repositories: None,
    }
}

/// Builds the per-command entries of the compact verification, preserving the
/// reference key set: `duration`/`exit_code` for a run command, `timed_out`
/// when it timed out, `output_tail` for a failed command and `reason` for a
/// command that did not run.
fn command_entries(
    outcome: &FingerprintedSequenceOutcome,
    commands: &[&str],
) -> Vec<VerificationCommand> {
    let mut entries: Vec<VerificationCommand> = outcome
        .commands()
        .iter()
        .enumerate()
        .map(|(index, command_outcome)| VerificationCommand {
            command: commands.get(index).copied().unwrap_or_default().to_owned(),
            timed_out: command_outcome.timed_out().then_some(true),
            duration: Some(command_outcome.duration().as_secs_f64()),
            exit_code: command_outcome.exit_code(),
            output_tail: command_outcome.output_tail().map(str::to_owned),
            reason: None,
        })
        .collect();

    if let Some(failure) = outcome.run_failure() {
        entries.push(VerificationCommand {
            command: commands
                .get(failure.index())
                .copied()
                .unwrap_or_default()
                .to_owned(),
            timed_out: None,
            duration: None,
            exit_code: None,
            output_tail: None,
            reason: Some(failure.as_str().to_owned()),
        });
    }

    entries
}

/// Converts a captured workspace fingerprint into the compact persisted triple.
fn to_git_fingerprint(fingerprint: &WorkspaceFingerprint) -> GitFingerprint {
    GitFingerprint {
        head: fingerprint
            .head()
            .map_or_else(String::new, |commit| commit.as_str().to_owned()),
        index_fingerprint: fingerprint.index_fingerprint().to_hex(),
        worktree_fingerprint: fingerprint.worktree_fingerprint().to_hex(),
    }
}

/// Returns the sorted side-effect paths as strings, or `None` when there are no
/// available side effects, matching the compact contract that omits an empty
/// `side_effects` list.
fn side_effect_paths(outcome: &FingerprintedSequenceOutcome) -> Option<Vec<String>> {
    let effects = outcome.side_effects()?;
    if effects.is_empty() {
        return None;
    }
    Some(
        effects
            .paths()
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
    )
}

/// Builds the frozen relative verification log reference.
fn verification_log_reference(round: &RoundRef) -> String {
    let task_id = round.task_id;
    let round_number = round.round_number;
    format!("verification/{task_id}/round_{round_number}")
}

#[cfg(test)]
mod tests {
    use super::{
        CommandRunError, CommandRunOutcome, CommandSequenceError, DEFAULT_TAIL_BYTES,
        FingerprintedRunFailure, FingerprintedSequenceError, FingerprintedSequenceOutcome,
        FingerprintedSequenceStatus, PersistVerificationError, PersistedVerification,
        PersistedVerificationOutcome, TestCommandSequenceOutcome, WorkspaceSideEffects,
        capture_fingerprint, run_round_verification_persisted, run_test_command,
        run_test_command_sequence, run_test_command_sequence_fingerprinted,
    };
    use bridge_command_policy::{PolicyReason, TestCommandReason};
    use bridge_domain::{ProjectId, TaskId, VerificationStatus, VerifierState};
    use bridge_storage::{CreateTaskInput, RoundRef, StorageConnection, VerifierUpdateOutcome};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::str::FromStr;
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

    fn run_sequence(
        dir: &Path,
        commands: &[&str],
        timeout: Duration,
        tail_bytes: usize,
    ) -> Result<TestCommandSequenceOutcome, CommandSequenceError> {
        run_test_command_sequence(dir, commands, timeout, tail_bytes)
    }

    fn run_fingerprinted(
        dir: &Path,
        commands: &[&str],
        timeout: Duration,
        tail_bytes: usize,
    ) -> Result<FingerprintedSequenceOutcome, FingerprintedSequenceError> {
        run_test_command_sequence_fingerprinted(dir, commands, timeout, tail_bytes)
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).expect("create repo dir");
        git(dir, &["init", "-q"]);
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(path, content).expect("write file");
    }

    fn commit(dir: &Path, message: &str, add: &[&str]) {
        for path in add {
            git(dir, &["add", "--", path]);
        }
        git(
            dir,
            &[
                "-c",
                "user.email=bridge-verifier@example.invalid",
                "-c",
                "user.name=bridge-verifier",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
    }

    fn rev_parse_head(dir: &Path) -> String {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git rev-parse");
        assert!(output.status.success(), "git rev-parse HEAD failed");
        String::from_utf8(output.stdout)
            .expect("decode HEAD")
            .trim()
            .to_owned()
    }

    /// Renders the exact side-effect paths for an assertion.
    fn paths(effects: &WorkspaceSideEffects) -> Vec<String> {
        effects
            .paths()
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
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

    #[test]
    fn empty_sequence_succeeds_without_running_anything() {
        let dir = TempDir::new("seq-empty");
        let outcome = run_sequence(dir.path(), &[], Duration::from_secs(5), DEFAULT_TAIL_BYTES)
            .expect("an empty list is valid");
        assert!(outcome.succeeded());
        assert!(outcome.is_empty());
        assert_eq!(outcome.len(), 0);
        assert!(outcome.commands().is_empty());
    }

    #[test]
    fn sequence_runs_commands_in_order() {
        let dir = TempDir::new("seq-order");
        // Each command depends on the artifact of the previous one, so the whole
        // sequence can succeed only when the commands run strictly in order.
        let outcome = run_sequence(
            dir.path(),
            &["mkdir step", "touch step/one", "cp step/one step/two"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        assert_eq!(outcome.len(), 3);
        assert!(outcome.commands().iter().all(CommandRunOutcome::succeeded));
        assert!(dir.path().join("step/one").is_file());
        assert!(dir.path().join("step/two").is_file());
    }

    #[test]
    fn sequence_stops_at_first_nonzero_exit() {
        let dir = TempDir::new("seq-nonzero");
        let outcome = run_sequence(
            dir.path(),
            &["touch first.txt", "false", "touch third.txt"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(!outcome.succeeded());
        assert_eq!(outcome.len(), 2);
        assert!(outcome.commands()[0].succeeded());
        assert_eq!(outcome.commands()[1].exit_code(), Some(1));
        assert!(!outcome.commands()[1].timed_out());
        assert!(dir.path().join("first.txt").is_file());
        assert!(
            !dir.path().join("third.txt").exists(),
            "commands after the first failure must not run"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sequence_stops_at_first_timeout() {
        let dir = TempDir::new("seq-timeout");
        let outcome = run_sequence(
            dir.path(),
            &["true", "sleep 98765.4321", "touch third.txt"],
            Duration::from_millis(200),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(!outcome.succeeded());
        assert_eq!(outcome.len(), 2);
        assert!(outcome.commands()[0].succeeded());
        assert!(outcome.commands()[1].timed_out());
        assert!(
            !dir.path().join("third.txt").exists(),
            "commands after the timeout must not run"
        );
    }

    #[test]
    fn validation_of_whole_list_happens_before_any_command_runs() {
        let dir = TempDir::new("seq-prevalidate");
        // The reference validates the entire list up front: a rejected command
        // anywhere must prevent the earlier safe command from running at all.
        let error = run_sequence(
            dir.path(),
            &["touch marker.txt", "git push origin main"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("a rejected command must fail the whole list");
        assert_eq!(
            error,
            CommandSequenceError::Rejected {
                index: 1,
                reason: TestCommandReason::Policy(PolicyReason::GitWriteBlocked),
            }
        );
        assert_eq!(error.index(), 1);
        assert!(
            !dir.path().join("marker.txt").exists(),
            "no command may run when the list is rejected up front"
        );
    }

    #[test]
    fn rejected_unsafe_command_prevents_earlier_command_from_running() {
        let dir = TempDir::new("seq-unsafe");
        let error = run_sequence(
            dir.path(),
            &["touch marker.txt", "touch a && touch b"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("unsafe shell syntax must fail the whole list");
        assert_eq!(
            error,
            CommandSequenceError::Rejected {
                index: 1,
                reason: TestCommandReason::Policy(PolicyReason::UnprovableShellSyntax),
            }
        );
        assert!(!dir.path().join("marker.txt").exists());
    }

    #[test]
    fn spawn_failure_stops_without_running_later_commands() {
        let dir = TempDir::new("seq-spawn");
        let error = run_sequence(
            dir.path(),
            &[
                "true",
                "bridge-verifier-no-such-executable-2f7a",
                "touch third.txt",
            ],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("a missing executable must stop the sequence");
        assert_eq!(
            error,
            CommandSequenceError::Run {
                index: 1,
                error: CommandRunError::Spawn,
            }
        );
        assert_eq!(error.index(), 1);
        assert!(
            !dir.path().join("third.txt").exists(),
            "commands after the spawn failure must not run"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sequence_preserves_bounded_output_semantics() {
        let dir = TempDir::new("seq-tail");
        // The sequence must delegate to the single-command runner, so its
        // bounded-tail contract still holds for the recorded failure.
        let outcome = run_sequence(dir.path(), &["yes"], Duration::from_millis(200), 64)
            .expect("yes should run");
        assert!(!outcome.succeeded());
        assert_eq!(outcome.len(), 1);
        let command = &outcome.commands()[0];
        assert!(command.timed_out());
        let tail = command.output_tail().expect("a failed command has a tail");
        assert!(tail.len() <= 64, "tail must be bounded: {}", tail.len());
    }

    #[test]
    fn sequence_errors_and_outcome_do_not_reveal_sensitive_inputs() {
        let dir = TempDir::new("seq-redact");
        let error = run_sequence(
            dir.path(),
            &["touch marker.txt", "SUPER_SECRET_VALUE=leaked git push"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("git push must be rejected");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("SUPER_SECRET_VALUE"));
        assert!(!rendered.contains("leaked"));
        assert!(!rendered.contains("push"));
        assert!(!rendered.contains("marker"));
        assert!(!rendered.contains(&dir.path().display().to_string()));

        let error = run_sequence(
            dir.path(),
            &["true", "bridge-verifier-secret-executable-9f2c"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("a missing executable must stop the sequence");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains(&dir.path().display().to_string()));

        // A failed command's output is available through the explicit accessor
        // but must never leak through the sequence outcome's `Debug`.
        let outcome = run_sequence(
            dir.path(),
            &["cat SUPER_SECRET_OUTPUT_7a3f"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("cat should run");
        assert!(!outcome.succeeded());
        assert!(
            outcome.commands()[0]
                .output_tail()
                .is_some_and(|tail| tail.contains("SUPER_SECRET_OUTPUT_7a3f")),
            "the failure output should be captured by the single runner"
        );
        let rendered = format!("{outcome:?}");
        assert!(!rendered.contains("SUPER_SECRET_OUTPUT_7a3f"));
        assert!(!rendered.contains(&dir.path().display().to_string()));
    }

    #[test]
    fn sequence_error_identifiers_are_stable() {
        assert_eq!(
            CommandSequenceError::Rejected {
                index: 3,
                reason: TestCommandReason::MissingExecutable,
            }
            .as_str(),
            "missing_executable"
        );
        assert_eq!(
            CommandSequenceError::Run {
                index: 0,
                error: CommandRunError::Spawn,
            }
            .as_str(),
            "spawn_failed"
        );
    }

    #[test]
    fn clean_sequence_captures_matching_fingerprints() {
        let dir = TempDir::new("fp-clean");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        // An independent read-only snapshot taken now must equal the captured
        // before fingerprint: the values come from bridge-git, never from this
        // crate.
        let expected = bridge_git::take_snapshot(dir.path()).expect("snapshot the repo");
        let outcome = run_fingerprinted(
            dir.path(),
            &["true"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        assert_eq!(outcome.status(), FingerprintedSequenceStatus::Succeeded);
        let before = outcome.before().expect("before fingerprint");
        let after = outcome.after().expect("after fingerprint");
        assert_eq!(before, after);
        assert_eq!(before.head(), expected.head());
        assert_eq!(before.index_fingerprint(), expected.index_fingerprint());
        assert_eq!(
            before.worktree_fingerprint(),
            expected.worktree_fingerprint()
        );
        // The typed HEAD mapping is the exact commit id of the repository.
        assert_eq!(
            before.head().map(|commit| commit.as_str().to_owned()),
            Some(rev_parse_head(dir.path()))
        );
        assert!(!outcome.after_snapshot_failed());
    }

    #[test]
    fn worktree_change_makes_after_fingerprint_differ() {
        let dir = TempDir::new("fp-worktree");
        init_repo(dir.path());
        write_file(dir.path(), "a.txt", "one\n");
        write_file(dir.path(), "b.txt", "two\n");
        commit(dir.path(), "init", &["a.txt", "b.txt"]);

        let outcome = run_fingerprinted(
            dir.path(),
            &["cp a.txt b.txt", "touch untracked.txt"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        let before = outcome.before().expect("before fingerprint");
        let after = outcome.after().expect("after fingerprint");
        assert_ne!(before, after);
        // The tracked content change and the new untracked file are both
        // visible in the worktree fingerprint.
        assert_ne!(before.worktree_fingerprint(), after.worktree_fingerprint());
        // Nothing was staged and no commit happened, so the index and HEAD
        // stay stable.
        assert_eq!(before.index_fingerprint(), after.index_fingerprint());
        assert_eq!(before.head(), after.head());
    }

    #[test]
    fn staged_change_updates_index_fingerprint() {
        let dir = TempDir::new("fp-index");
        init_repo(dir.path());
        write_file(dir.path(), "a.txt", "one\n");
        write_file(dir.path(), "b.txt", "two\n");
        commit(dir.path(), "init", &["a.txt", "b.txt"]);

        // `git update-index` is not one of the Git write subcommands the
        // frozen policy blocks (`add`/`commit`/`push`), so the command runs
        // and stages the modified worktree content without creating a commit.
        let outcome = run_fingerprinted(
            dir.path(),
            &["cp a.txt b.txt", "git update-index b.txt"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        let before = outcome.before().expect("before fingerprint");
        let after = outcome.after().expect("after fingerprint");
        assert_ne!(before.index_fingerprint(), after.index_fingerprint());
        assert_ne!(before, after);
        // A staged change creates no commit, so HEAD stays stable: no allowed
        // command can move HEAD, because `git commit` is rejected by the
        // unchanged policy.
        assert_eq!(before.head(), after.head());
    }

    #[test]
    fn repository_without_commits_reports_no_head() {
        let dir = TempDir::new("fp-no-commit");
        init_repo(dir.path());
        write_file(dir.path(), "pending.txt", "pending\n");

        let outcome = run_fingerprinted(
            dir.path(),
            &["true"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        assert_eq!(outcome.status(), FingerprintedSequenceStatus::Succeeded);
        // A valid repository without commits maps HEAD to `None`; the rest of
        // the fingerprint is still captured.
        assert_eq!(
            outcome.before().and_then(|fingerprint| fingerprint.head()),
            None
        );
        assert_eq!(
            outcome.after().and_then(|fingerprint| fingerprint.head()),
            None
        );
        assert!(outcome.before().is_some());
        assert!(outcome.after().is_some());
        assert!(!outcome.after_snapshot_failed());
    }

    #[test]
    fn nonzero_failure_stops_the_sequence_and_still_captures_after() {
        let dir = TempDir::new("fp-nonzero");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let outcome = run_fingerprinted(
            dir.path(),
            &["touch ran.txt", "false", "touch never.txt"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(!outcome.succeeded());
        assert_eq!(outcome.status(), FingerprintedSequenceStatus::Failed);
        assert_eq!(outcome.commands().len(), 2);
        assert!(dir.path().join("ran.txt").is_file());
        assert!(
            !dir.path().join("never.txt").exists(),
            "commands after the first failure must not run"
        );
        let before = outcome.before().expect("before fingerprint");
        let after = outcome
            .after()
            .expect("after is captured for a failed sequence");
        assert_ne!(
            before, after,
            "the untracked ran.txt must be visible in after"
        );
        assert!(!outcome.after_snapshot_failed());
    }

    #[cfg(unix)]
    #[test]
    fn timeout_still_captures_after() {
        let dir = TempDir::new("fp-timeout");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let outcome = run_fingerprinted(
            dir.path(),
            &["sleep 98765.4321"],
            Duration::from_millis(200),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(!outcome.succeeded());
        assert_eq!(outcome.status(), FingerprintedSequenceStatus::Failed);
        assert!(outcome.commands()[0].timed_out());
        let before = outcome.before().expect("before fingerprint");
        let after = outcome
            .after()
            .expect("after is captured for a timed-out sequence");
        assert_eq!(before, after);
        assert!(!outcome.after_snapshot_failed());
    }

    #[test]
    fn empty_sequence_captures_no_fingerprints() {
        let dir = TempDir::new("fp-empty");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let outcome =
            run_fingerprinted(dir.path(), &[], Duration::from_secs(5), DEFAULT_TAIL_BYTES)
                .expect("an empty list is valid");
        assert!(outcome.succeeded());
        assert_eq!(outcome.status(), FingerprintedSequenceStatus::Succeeded);
        assert!(outcome.commands().is_empty());
        assert_eq!(outcome.before(), None);
        assert_eq!(outcome.after(), None);
        assert!(!outcome.after_snapshot_failed());

        // The reference returns the empty list before fingerprinting, so
        // even a non-repository must succeed: no snapshot is ever attempted.
        let plain = TempDir::new("fp-empty-plain");
        let outcome = run_fingerprinted(
            plain.path(),
            &[],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("an empty list never touches Git");
        assert!(outcome.succeeded());
        assert_eq!(outcome.before(), None);
        assert_eq!(outcome.after(), None);
    }

    #[test]
    fn validation_happens_before_the_before_snapshot() {
        // A non-repository would fail the before snapshot, so observing the
        // policy rejection instead proves the reference order: the whole list
        // is validated before any fingerprint is captured.
        let plain = TempDir::new("fp-order");
        let error = run_fingerprinted(
            plain.path(),
            &["touch marker.txt", "git push origin main"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("a rejected command must fail the whole list");
        assert_eq!(
            error,
            FingerprintedSequenceError::Rejected {
                index: 1,
                reason: TestCommandReason::Policy(PolicyReason::GitWriteBlocked),
            }
        );
        assert!(
            !plain.path().join("marker.txt").exists(),
            "no command may run when the list is rejected up front"
        );
    }

    #[test]
    fn before_snapshot_failure_runs_no_command() {
        let plain = TempDir::new("fp-before-failed");
        let error = run_fingerprinted(
            plain.path(),
            &["touch marker.txt"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("a non-repository cannot be fingerprinted");
        assert_eq!(error, FingerprintedSequenceError::BeforeSnapshot);
        assert_eq!(error.as_str(), "git_fingerprint_failed");
        assert_eq!(error.to_string(), "git_fingerprint_failed");
        assert!(
            !plain.path().join("marker.txt").exists(),
            "a failed before snapshot must not run any command"
        );
    }

    #[test]
    fn after_snapshot_failure_overwrites_the_overall_status() {
        let dir = TempDir::new("fp-after-failed");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        // `rm` is a simple command without metacharacters, so the frozen
        // policy allows it; deleting `.git` makes only the after snapshot fail
        // while the sequence itself succeeds. The reference overwrites the
        // overall status with `error` (reason `git_fingerprint_failed`)
        // instead of reporting success, so the typed outcome must not be
        // interpretable as an overall success either.
        let outcome = run_fingerprinted(
            dir.path(),
            &["rm -rf .git"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert_eq!(outcome.commands().len(), 1);
        assert!(outcome.commands()[0].succeeded());
        assert!(
            !outcome.succeeded(),
            "an after-fingerprint failure is never an overall success"
        );
        assert_eq!(
            outcome.status(),
            FingerprintedSequenceStatus::GitFingerprintFailed
        );
        assert_eq!(outcome.status().as_str(), "git_fingerprint_failed");
        assert_eq!(outcome.status().to_string(), "git_fingerprint_failed");
        // Reference semantics: the commands and the before fingerprint stay
        // recorded, the after fingerprint is absent and the failure is
        // reported (reference `error` + `git_fingerprint_failed`).
        assert!(outcome.before().is_some());
        assert_eq!(outcome.after(), None);
        assert!(outcome.after_snapshot_failed());
        assert_eq!(outcome.run_failure(), None);
    }

    #[test]
    fn spawn_failure_retains_the_run_and_captures_after() {
        let dir = TempDir::new("fp-spawn");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        // The reference records a spawn failure as a failed command entry
        // (`spawn_failed`) and still runs the after-fingerprint block, so the
        // orchestration retains the commands that already ran, records the
        // typed failure and captures the after fingerprint.
        let outcome = run_fingerprinted(
            dir.path(),
            &[
                "touch ran.txt",
                "bridge-verifier-no-such-executable-2f7a",
                "touch third.txt",
            ],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the run and the failure are retained in the outcome");
        let failure = outcome
            .run_failure()
            .expect("the spawn failure is recorded");
        assert_eq!(failure.index(), 1);
        assert_eq!(failure.error(), CommandRunError::Spawn);
        assert_eq!(failure.as_str(), "spawn_failed");
        assert_eq!(outcome.commands().len(), 1);
        assert!(outcome.commands()[0].succeeded());
        assert!(dir.path().join("ran.txt").is_file());
        assert!(
            !dir.path().join("third.txt").exists(),
            "commands after the spawn failure must not run"
        );
        assert!(!outcome.succeeded());
        assert_eq!(outcome.status(), FingerprintedSequenceStatus::Failed);
        let before = outcome.before().expect("before fingerprint");
        let after = outcome
            .after()
            .expect("after is captured after a spawn failure");
        assert_ne!(
            before, after,
            "the untracked ran.txt must be visible in after"
        );
        assert!(!outcome.after_snapshot_failed());
    }

    #[test]
    fn spawn_failure_with_after_snapshot_failure_reports_git_error() {
        let dir = TempDir::new("fp-spawn-after-failed");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        // The first command deletes `.git` and succeeds; the second cannot be
        // spawned. The reference still runs the after-fingerprint block after
        // the failed entry, so the block fails and overwrites the overall
        // status with `git_fingerprint_failed` while the commands, the spawn
        // failure and the before fingerprint stay recorded.
        let outcome = run_fingerprinted(
            dir.path(),
            &["rm -rf .git", "bridge-verifier-no-such-executable-2f7a"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the run and the failure are retained in the outcome");
        assert_eq!(outcome.commands().len(), 1);
        assert!(outcome.commands()[0].succeeded());
        let failure = outcome
            .run_failure()
            .expect("the spawn failure is recorded");
        assert_eq!(failure.index(), 1);
        assert_eq!(failure.error(), CommandRunError::Spawn);
        assert!(outcome.before().is_some());
        assert_eq!(outcome.after(), None);
        assert!(outcome.after_snapshot_failed());
        assert_eq!(
            outcome.status(),
            FingerprintedSequenceStatus::GitFingerprintFailed
        );
        assert!(!outcome.succeeded());
    }

    #[test]
    fn wait_failure_is_recorded_like_a_spawn_failure() {
        // A wait failure goes through the same recording path as a spawn
        // failure (both are the `Err` arm of the shared command loop), so its
        // typed mapping is proven on the outcome shape itself: the failure is
        // retained, the overall status is `failed` and `succeeded()` is false
        // even though every recorded command succeeded.
        let dir = TempDir::new("fp-wait");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let fingerprint = capture_fingerprint(dir.path()).expect("fingerprint");

        let outcome = FingerprintedSequenceOutcome {
            sequence: TestCommandSequenceOutcome {
                commands: vec![CommandRunOutcome {
                    exit_code: Some(0),
                    timed_out: false,
                    duration: Duration::from_secs(0),
                    output_tail: None,
                }],
            },
            run_failure: Some(FingerprintedRunFailure {
                index: 1,
                error: CommandRunError::Wait,
            }),
            before: Some(fingerprint.clone()),
            after: Some(fingerprint),
            after_snapshot_failed: false,
        };
        assert!(!outcome.succeeded());
        assert_eq!(outcome.status(), FingerprintedSequenceStatus::Failed);
        let failure = outcome.run_failure().expect("the wait failure is recorded");
        assert_eq!(failure.index(), 1);
        assert_eq!(failure.error(), CommandRunError::Wait);
        assert_eq!(failure.as_str(), "wait_failed");
        assert_eq!(failure.to_string(), "command 1 did not run: wait_failed");
        assert!(!outcome.after_snapshot_failed());
    }

    #[test]
    fn fingerprinted_errors_and_outcome_do_not_reveal_sensitive_inputs() {
        let dir = TempDir::new("fp-redact");
        init_repo(dir.path());
        write_file(dir.path(), "SUPER_SECRET_OUTPUT_7a3f", "leaked\n");
        commit(dir.path(), "init", &["SUPER_SECRET_OUTPUT_7a3f"]);

        let error = run_fingerprinted(
            dir.path(),
            &["SUPER_SECRET_VALUE=leaked git push"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("git push must be rejected");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("SUPER_SECRET"));
        assert!(!rendered.contains("leaked"));
        assert!(!rendered.contains("push"));

        let plain = TempDir::new("fp-redact-plain");
        let error = run_fingerprinted(
            plain.path(),
            &["true"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("a non-repository cannot be fingerprinted");
        let rendered = format!("{error:?} {error}");
        assert!(
            !rendered.contains("tmp"),
            "no workspace path may leak: {rendered}"
        );
        assert!(!rendered.contains(&plain.path().display().to_string()));

        // A failed command's output is available through the explicit
        // accessor but must never leak through the outcome's `Debug`, and the
        // fingerprint values are available only through their accessors.
        let outcome = run_fingerprinted(
            dir.path(),
            &["cat SUPER_SECRET_OUTPUT_7a3f no-such-file-9f2c"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("cat should run");
        assert!(!outcome.succeeded());
        assert!(
            outcome.commands()[0]
                .output_tail()
                .is_some_and(|tail| tail.contains("leaked")),
            "the failure output should be captured by the single runner"
        );
        let rendered = format!("{outcome:?}");
        assert!(!rendered.contains("SUPER_SECRET"));
        assert!(!rendered.contains("leaked"));
        assert!(!rendered.contains(&dir.path().display().to_string()));
        let before = outcome.before().expect("before fingerprint");
        assert!(!rendered.contains(&before.index_fingerprint().to_hex()));
        assert!(!rendered.contains(&before.worktree_fingerprint().to_hex()));
        assert!(!rendered.contains(before.head().expect("committed head").as_str()));

        // The fingerprint's own `Debug` renders presence only, never the
        // values.
        let rendered = format!("{before:?}");
        assert!(!rendered.contains(&before.index_fingerprint().to_hex()));
        assert!(!rendered.contains(&before.worktree_fingerprint().to_hex()));
        assert!(!rendered.contains(before.head().expect("committed head").as_str()));

        // A spawn failure is retained in the outcome; neither the outcome nor
        // the recorded failure may reveal the command text.
        let outcome = run_fingerprinted(
            dir.path(),
            &["bridge-verifier-secret-executable-9f2c"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the run failure is retained in the outcome");
        let failure = outcome
            .run_failure()
            .expect("the spawn failure is recorded");
        let rendered = format!("{outcome:?} {failure:?} {failure}");
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("executable-9f2c"));
        assert!(!rendered.contains(&dir.path().display().to_string()));
    }

    #[test]
    fn fingerprinted_error_and_status_identifiers_are_stable() {
        assert_eq!(
            FingerprintedSequenceError::BeforeSnapshot.as_str(),
            "git_fingerprint_failed"
        );
        assert_eq!(
            FingerprintedSequenceError::Rejected {
                index: 3,
                reason: TestCommandReason::MissingExecutable,
            }
            .as_str(),
            "missing_executable"
        );
        assert_eq!(
            FingerprintedSequenceError::BeforeSnapshot.to_string(),
            "git_fingerprint_failed"
        );
        assert_eq!(FingerprintedSequenceStatus::Succeeded.as_str(), "succeeded");
        assert_eq!(FingerprintedSequenceStatus::Failed.as_str(), "failed");
        assert_eq!(
            FingerprintedSequenceStatus::GitFingerprintFailed.as_str(),
            "git_fingerprint_failed"
        );
        assert_eq!(
            FingerprintedSequenceStatus::GitFingerprintFailed.to_string(),
            "git_fingerprint_failed"
        );
        assert_eq!(
            FingerprintedSequenceStatus::Succeeded.to_string(),
            "succeeded"
        );
    }

    #[test]
    fn clean_sequence_reports_no_side_effect_paths() {
        let dir = TempDir::new("se-clean");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let outcome = run_fingerprinted(
            dir.path(),
            &["true"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        let effects = outcome
            .side_effects()
            .expect("both fingerprints are captured");
        assert!(effects.is_empty());
        assert_eq!(effects.len(), 0);
        assert!(effects.paths().is_empty());
    }

    #[test]
    fn side_effects_report_tracked_modification_and_untracked_creation() {
        let dir = TempDir::new("se-paths");
        init_repo(dir.path());
        write_file(dir.path(), "a.txt", "one\n");
        write_file(dir.path(), "b.txt", "two\n");
        commit(dir.path(), "init", &["a.txt", "b.txt"]);

        // A tracked content change and a new untracked file are both reported as
        // repository-relative paths; nothing is staged and no commit happens.
        let outcome = run_fingerprinted(
            dir.path(),
            &["cp a.txt b.txt", "touch untracked.txt"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        let effects = outcome
            .side_effects()
            .expect("both fingerprints are captured");
        assert_eq!(
            paths(&effects),
            vec!["b.txt".to_owned(), "untracked.txt".to_owned()]
        );
    }

    #[test]
    fn side_effects_report_a_deleted_tracked_path() {
        let dir = TempDir::new("se-delete");
        init_repo(dir.path());
        write_file(dir.path(), "a.txt", "one\n");
        write_file(dir.path(), "b.txt", "two\n");
        commit(dir.path(), "init", &["a.txt", "b.txt"]);

        let outcome = run_fingerprinted(
            dir.path(),
            &["rm b.txt"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        let effects = outcome
            .side_effects()
            .expect("both fingerprints are captured");
        assert_eq!(paths(&effects), vec!["b.txt".to_owned()]);
    }

    #[test]
    fn side_effects_report_multiple_paths_sorted_and_deduplicated() {
        let dir = TempDir::new("se-multi");
        init_repo(dir.path());
        write_file(dir.path(), "a.txt", "a\n");
        write_file(dir.path(), "b.txt", "b\n");
        write_file(dir.path(), "c.txt", "c\n");
        commit(dir.path(), "init", &["a.txt", "b.txt", "c.txt"]);

        // One tracked modification (`b.txt`), one untracked creation
        // (`z_mod.txt`), another untracked creation (`z_new.txt`) and one
        // deletion (`c.txt`), in input order that is deliberately not sorted.
        let outcome = run_fingerprinted(
            dir.path(),
            &[
                "cp a.txt b.txt",
                "cp a.txt z_mod.txt",
                "touch z_new.txt",
                "rm c.txt",
            ],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        let effects = outcome
            .side_effects()
            .expect("both fingerprints are captured");
        assert_eq!(
            paths(&effects),
            vec![
                "b.txt".to_owned(),
                "c.txt".to_owned(),
                "z_mod.txt".to_owned(),
                "z_new.txt".to_owned(),
            ]
        );
    }

    #[test]
    fn ignored_files_are_not_side_effects() {
        let dir = TempDir::new("se-ignored");
        init_repo(dir.path());
        write_file(dir.path(), ".gitignore", "*.log\n");
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &[".gitignore", "module.py"]);

        // An ignored file never enters the worktree manifest, so it is not a
        // path side effect even though it was created by the run.
        let outcome = run_fingerprinted(
            dir.path(),
            &["touch debug.log"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        let effects = outcome
            .side_effects()
            .expect("both fingerprints are captured");
        assert!(
            effects.is_empty(),
            "an ignored file must not be reported as a side effect"
        );
    }

    #[test]
    fn side_effects_are_reported_for_a_nonzero_command_failure() {
        let dir = TempDir::new("se-nonzero");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let outcome = run_fingerprinted(
            dir.path(),
            &["touch ran.txt", "false", "touch never.txt"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(!outcome.succeeded());
        assert_eq!(outcome.status(), FingerprintedSequenceStatus::Failed);
        let effects = outcome
            .side_effects()
            .expect("both fingerprints are captured for a failed sequence");
        assert_eq!(paths(&effects), vec!["ran.txt".to_owned()]);
        assert!(
            !dir.path().join("never.txt").exists(),
            "commands after the first failure must not run"
        );
    }

    #[cfg(unix)]
    #[test]
    fn side_effects_are_reported_after_a_timeout() {
        let dir = TempDir::new("se-timeout");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let outcome = run_fingerprinted(
            dir.path(),
            &["touch ran.txt", "sleep 98765.4321"],
            Duration::from_millis(200),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(!outcome.succeeded());
        assert!(outcome.commands()[1].timed_out());
        let effects = outcome
            .side_effects()
            .expect("both fingerprints are captured for a timed-out sequence");
        assert_eq!(paths(&effects), vec!["ran.txt".to_owned()]);
    }

    #[test]
    fn empty_sequence_reports_no_side_effects() {
        let dir = TempDir::new("se-empty");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let outcome =
            run_fingerprinted(dir.path(), &[], Duration::from_secs(5), DEFAULT_TAIL_BYTES)
                .expect("an empty list is valid");
        assert!(outcome.succeeded());
        assert_eq!(outcome.before(), None);
        assert_eq!(outcome.after(), None);
        assert_eq!(
            outcome.side_effects(),
            None,
            "an empty command list must not report side effects"
        );
    }

    #[test]
    fn before_snapshot_failure_reports_no_side_effects() {
        let plain = TempDir::new("se-before-failed");
        let error = run_fingerprinted(
            plain.path(),
            &["touch marker.txt"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect_err("a non-repository cannot be fingerprinted");
        assert_eq!(error, FingerprintedSequenceError::BeforeSnapshot);
        assert!(
            !plain.path().join("marker.txt").exists(),
            "a failed before snapshot must not run any command"
        );
    }

    #[test]
    fn after_snapshot_failure_is_not_misclassified_as_side_effects() {
        let dir = TempDir::new("se-after-failed");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        // Deleting `.git` makes only the after snapshot fail while the command
        // succeeds. The unavailable after fingerprint stays an infrastructure
        // failure and must never be reported as a clean path result.
        let outcome = run_fingerprinted(
            dir.path(),
            &["rm -rf .git"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.commands()[0].succeeded());
        assert!(outcome.after_snapshot_failed());
        assert_eq!(outcome.after(), None);
        assert_eq!(outcome.side_effects(), None);
        assert_eq!(
            outcome.status(),
            FingerprintedSequenceStatus::GitFingerprintFailed
        );
        assert!(!outcome.succeeded());
    }

    #[test]
    fn side_effects_do_not_reveal_sensitive_inputs() {
        let dir = TempDir::new("se-redact");
        init_repo(dir.path());
        write_file(dir.path(), "SUPER_SECRET_OUTPUT_7a3f", "leaked\n");
        commit(dir.path(), "init", &["SUPER_SECRET_OUTPUT_7a3f"]);

        let outcome = run_fingerprinted(
            dir.path(),
            &["touch SUPER_SECRET_CREATED_5c1f"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .expect("the sequence should run");
        assert!(outcome.succeeded());
        let effects = outcome
            .side_effects()
            .expect("both fingerprints are captured");

        // The exact repository-relative path is available only through the
        // explicit accessor.
        assert_eq!(
            paths(&effects),
            vec!["SUPER_SECRET_CREATED_5c1f".to_owned()]
        );

        // `Debug`/`Display` render only the number of paths, never the path
        // itself, a fingerprint, command text, output or workspace path.
        assert_eq!(format!("{effects:?}"), "WorkspaceSideEffects { paths: 1 }");
        assert_eq!(format!("{effects}"), "side_effects=1");
        let rendered = format!("{effects:?} {effects}");
        assert!(!rendered.contains("SUPER_SECRET"));
        assert!(!rendered.contains("leaked"));
        assert!(!rendered.contains(&dir.path().display().to_string()));

        let before = outcome.before().expect("before fingerprint");
        assert!(!rendered.contains(&before.index_fingerprint().to_hex()));
        assert!(!rendered.contains(&before.worktree_fingerprint().to_hex()));
        assert!(!rendered.contains(before.head().expect("committed head").as_str()));

        // The outcome's own `Debug` renders only the count and status, never the
        // side-effect paths, output or fingerprints.
        let rendered = format!("{outcome:?}");
        assert!(!rendered.contains("SUPER_SECRET"));
        assert!(!rendered.contains("leaked"));
        assert!(!rendered.contains(&before.index_fingerprint().to_hex()));
        assert!(!rendered.contains(&before.worktree_fingerprint().to_hex()));
        assert!(!rendered.contains(before.head().expect("committed head").as_str()));
    }

    // -----------------------------------------------------------------------
    // Persist-once orchestration (task 5.6).
    // -----------------------------------------------------------------------

    fn persist_task_id() -> TaskId {
        TaskId::from_str("550e8400-e29b-41d4-a716-446655440000").expect("task id")
    }

    fn persist_project() -> ProjectId {
        ProjectId::from_str("persist-project").expect("project id")
    }

    fn open_persist_storage(tag: &str) -> (TempDir, StorageConnection) {
        let dir = TempDir::new(tag);
        let path = dir.path().join("state.sqlite");
        bridge_storage::initialize(&path).expect("initialize schema v6");
        let storage = bridge_storage::connect(&path).expect("connect");
        (dir, storage)
    }

    fn create_persist_round(storage: &mut StorageConnection, workspace: &Path) -> RoundRef {
        let task_id = persist_task_id();
        let project_id = persist_project();
        storage
            .create_task(CreateTaskInput {
                task_id,
                project_id: project_id.clone(),
                workspace: workspace.display().to_string(),
                task: "verify".to_owned(),
                request_id: "req-persist-1".to_owned(),
                payload_hash: "hash-persist-1".to_owned(),
                base_head: None,
                allowed_paths: vec![],
                test_commands: vec![],
                snapshot: None,
            })
            .expect("create task");
        RoundRef {
            task_id,
            project_id,
            round_number: 1,
        }
    }

    fn persist_round(
        storage: &mut StorageConnection,
        round: &RoundRef,
        workspace: &Path,
        commands: &[&str],
        timeout: Duration,
    ) -> Result<PersistedVerification, PersistVerificationError> {
        run_round_verification_persisted(
            storage,
            round.clone(),
            workspace,
            commands,
            timeout,
            DEFAULT_TAIL_BYTES,
        )
    }

    /// Reads the persisted verifier result back through the storage API.
    fn persisted_round_verification(
        storage: &mut StorageConnection,
        round: &RoundRef,
    ) -> bridge_domain::Verification {
        let outcome = storage
            .begin_verifier(round.clone())
            .expect("read verifier state");
        let VerifierUpdateOutcome::AlreadyDone(inner) = outcome else {
            panic!("the round must be verifier_state=done");
        };
        assert_eq!(inner.round.verifier_state, Some(VerifierState::Done));
        inner
            .round
            .verifier_json
            .expect("a done round carries a verification")
    }

    #[test]
    fn multi_repository_verifier_persists_qualified_effects_and_reuses_exact_result() {
        let (main, mut storage) = open_persist_storage("multi-main");
        let a = TempDir::new("multi-a");
        let b = TempDir::new("multi-b");
        for root in [main.path(), a.path(), b.path()] {
            init_repo(root);
        }
        let round = create_persist_round(&mut storage, main.path());
        let command = format!(
            "touch same.txt '{}/same.txt' '{}/same.txt'",
            a.path().display(),
            b.path().display()
        );
        let result = super::run_round_verification_persisted_with_repositories(
            &mut storage,
            round.clone(),
            main.path(),
            &[b.path(), a.path(), b.path()],
            &[&command],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .unwrap();
        let verification = result.verification();
        assert_eq!(verification.status, VerificationStatus::Passed);
        let repos = verification.repositories.as_ref().unwrap();
        assert_eq!(repos.len(), 2);
        assert!(repos[0].root < repos[1].root);
        for repo in repos {
            assert_eq!(repo.side_effects.as_ref().unwrap(), &["same.txt"]);
        }
        let effects = verification.side_effects.as_ref().unwrap();
        assert!(effects.contains(&"same.txt".to_owned()));
        assert!(effects.contains(&format!("{}/same.txt", a.path().display())));
        assert!(effects.contains(&format!("{}/same.txt", b.path().display())));
        assert_eq!(
            persisted_round_verification(&mut storage, &round),
            *verification
        );
        let reused = super::run_round_verification_persisted_with_repositories(
            &mut storage,
            round,
            main.path(),
            &[a.path(), b.path()],
            &["touch never-rerun"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .unwrap();
        assert!(reused.reused());
        assert_eq!(reused.verification(), verification);
        assert!(!main.path().join("never-rerun").exists());
        assert!(!format!("{result:?}").contains("same.txt"));
    }

    #[test]
    fn external_before_failure_prevents_all_commands() {
        let (main, mut storage) = open_persist_storage("multi-before");
        let external = TempDir::new("multi-not-git");
        init_repo(main.path());
        let round = create_persist_round(&mut storage, main.path());
        let result = super::run_round_verification_persisted_with_repositories(
            &mut storage,
            round,
            main.path(),
            &[external.path()],
            &["touch must-not-run"],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .unwrap();
        assert_eq!(result.verification().status, VerificationStatus::Error);
        assert!(result.verification().commands.is_none());
        assert!(!main.path().join("must-not-run").exists());
    }

    #[test]
    fn external_after_failure_keeps_main_and_preceding_repository_fingerprints() {
        let (main, mut storage) = open_persist_storage("multi-after");
        let a = TempDir::new("multi-after-a");
        let b = TempDir::new("multi-after-b");
        for root in [main.path(), a.path(), b.path()] {
            init_repo(root);
        }
        let mut roots = [a.path(), b.path()];
        roots.sort();
        let command = format!("rm -rf '{}/.git'", roots[1].display());
        let round = create_persist_round(&mut storage, main.path());
        let result = super::run_round_verification_persisted_with_repositories(
            &mut storage,
            round,
            main.path(),
            &roots,
            &[&command],
            Duration::from_secs(5),
            DEFAULT_TAIL_BYTES,
        )
        .unwrap();
        let v = result.verification();
        assert_eq!(v.status, VerificationStatus::Error);
        assert_eq!(v.reason.as_deref(), Some("git_fingerprint_failed"));
        assert!(v.before.is_some() && v.after.is_some());
        assert_eq!(v.repositories.as_ref().unwrap().len(), 1);
        assert!(v.side_effects.is_none());
        assert!(v.commands.as_ref().unwrap()[0].exit_code == Some(0));
    }

    #[test]
    fn empty_or_unsafe_multi_verification_does_not_probe_external_roots() {
        for (tag, commands, expected) in [
            ("multi-empty", vec![], VerificationStatus::Passed),
            ("multi-unsafe", vec!["git push"], VerificationStatus::Unsafe),
        ] {
            let (main, mut storage) = open_persist_storage(tag);
            let external = TempDir::new("multi-unavailable");
            let round = create_persist_round(&mut storage, main.path());
            let result = super::run_round_verification_persisted_with_repositories(
                &mut storage,
                round,
                main.path(),
                &[external.path()],
                &commands,
                Duration::from_secs(5),
                DEFAULT_TAIL_BYTES,
            )
            .unwrap();
            assert_eq!(result.verification().status, expected);
            assert!(result.verification().before.is_none());
            assert!(result.verification().repositories.is_none());
        }
    }

    #[test]
    fn first_run_persists_a_compact_passed_verification() {
        let (dir, mut storage) = open_persist_storage("persist-first");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["true"],
            Duration::from_secs(5),
        )
        .expect("the first run must persist");
        assert!(result.persisted());
        assert_eq!(result.outcome(), PersistedVerificationOutcome::Persisted);
        assert!(!result.reused());
        assert!(!result.replayed());

        let verification = result.verification();
        assert_eq!(verification.status, VerificationStatus::Passed);
        let commands = verification
            .commands
            .as_ref()
            .expect("commands are present");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "true");
        assert_eq!(commands[0].exit_code, Some(0));
        assert!(commands[0].duration.is_some());
        assert_eq!(commands[0].timed_out, None);
        assert_eq!(commands[0].output_tail, None);
        assert_eq!(commands[0].reason, None);
        // A clean run leaves no side effects, so the compact contract omits the
        // key instead of writing an empty list.
        assert_eq!(verification.side_effects, None);
        assert_eq!(verification.index, None);
        assert_eq!(verification.reason, None);
        let before = verification.before.as_ref().expect("before fingerprint");
        let after = verification.after.as_ref().expect("after fingerprint");
        assert_eq!(before, after);
        assert_eq!(
            verification.log,
            format!("verification/{}/round_1", persist_task_id())
        );

        // The exact persisted row carries the same compact verification.
        assert_eq!(
            persisted_round_verification(&mut storage, &round),
            *verification
        );
    }

    #[test]
    fn repository_without_commits_persists_an_empty_head() {
        let (dir, mut storage) = open_persist_storage("persist-no-commit");
        init_repo(dir.path());
        write_file(dir.path(), "pending.txt", "pending\n");
        let round = create_persist_round(&mut storage, dir.path());

        // A valid repository without commits has no HEAD; the compact persisted
        // contract models `head` as a required string, so it is empty.
        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["true"],
            Duration::from_secs(5),
        )
        .expect("the run persists");
        let verification = result.verification();
        assert_eq!(verification.status, VerificationStatus::Passed);
        let before = verification.before.as_ref().expect("before fingerprint");
        assert_eq!(before.head, "");
        assert_eq!(
            before,
            verification.after.as_ref().expect("after fingerprint")
        );
    }

    #[test]
    fn done_reuse_does_not_spawn_commands() {
        let (dir, mut storage) = open_persist_storage("persist-reuse");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        let first = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["true"],
            Duration::from_secs(5),
        )
        .expect("the first run must persist");
        assert!(first.persisted());

        // The second call must reuse the finished result and never run the
        // (side-effecting) command.
        let second = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["touch reuse-marker.txt"],
            Duration::from_secs(5),
        )
        .expect("a finished result must be reused");
        assert!(second.reused());
        assert_eq!(second.outcome(), PersistedVerificationOutcome::Reused);
        assert_eq!(second.verification(), first.verification());
        assert!(
            !dir.path().join("reuse-marker.txt").exists(),
            "a done result must not spawn any test command"
        );
    }

    #[test]
    fn identical_inputs_are_idempotent() {
        let (dir, mut storage) = open_persist_storage("persist-idempotent");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        let first = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["true"],
            Duration::from_secs(5),
        )
        .expect("the first run must persist");
        let second = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["true"],
            Duration::from_secs(5),
        )
        .expect("the identical call must be idempotent");
        assert_eq!(second.verification(), first.verification());
        assert!(second.reused() || second.replayed());
    }

    #[test]
    fn running_verifier_recovery_reruns_and_persists() {
        let (dir, mut storage) = open_persist_storage("persist-recovery");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        // Simulate an attempt that died mid-flight: the round is `running` but
        // has no persisted result, so recovery must run the verification.
        let started = storage.begin_verifier(round.clone()).expect("begin");
        assert!(matches!(started, VerifierUpdateOutcome::Started(_)));

        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["touch recovered.txt"],
            Duration::from_secs(5),
        )
        .expect("a running verifier must be re-run");
        assert!(result.persisted());
        assert_eq!(result.verification().status, VerificationStatus::Passed);
        assert!(
            dir.path().join("recovered.txt").is_file(),
            "a running verifier must actually run its commands"
        );
        assert_eq!(
            persisted_round_verification(&mut storage, &round).status,
            VerificationStatus::Passed
        );
    }

    #[test]
    fn conflicting_result_maps_to_a_typed_fail_closed_error() {
        // The conflict is produced by the storage `complete_verifier`; the
        // orchestration only maps it to its own typed category without
        // overwriting anything. bridge-storage proves the concurrent behavior.
        let error =
            super::map_storage_error(bridge_storage::RoundUpdateError::VerifierResultConflict);
        assert!(matches!(error, PersistVerificationError::Conflict));
        assert_eq!(error.as_str(), "verifier_result_conflict");
        assert_eq!(error.to_string(), "verifier_result_conflict");
        assert_eq!(
            format!("{error:?}"),
            "PersistVerificationError(\"verifier_result_conflict\")"
        );
    }

    #[test]
    fn missing_task_is_a_typed_storage_failure() {
        let (_dir, mut storage) = open_persist_storage("persist-missing");
        let round = RoundRef {
            task_id: TaskId::from_str("550e8400-e29b-41d4-a716-446655440099")
                .expect("missing task id"),
            project_id: persist_project(),
            round_number: 1,
        };
        let error = persist_round(
            &mut storage,
            &round,
            Path::new("/nonexistent-workspace-9f2c"),
            &["true"],
            Duration::from_secs(5),
        )
        .expect_err("a missing task must fail");
        assert!(matches!(error, PersistVerificationError::Storage(_)));
        assert_eq!(error.as_str(), "storage_failed");
    }

    #[test]
    fn verifier_git_failure_persists_the_error_variant() {
        let (dir, mut storage) = open_persist_storage("persist-git-error");
        // A plain directory is not a Git repository, so the before snapshot
        // fails; the orchestration persists the reference `error` variant
        // instead of failing.
        let round = create_persist_round(&mut storage, dir.path());

        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["touch must-not-run.txt"],
            Duration::from_secs(5),
        )
        .expect("a Git fingerprint failure is persisted as the error variant");
        assert!(result.persisted());
        let verification = result.verification();
        assert_eq!(verification.status, VerificationStatus::Error);
        assert_eq!(
            verification.reason.as_deref(),
            Some("git_fingerprint_failed")
        );
        assert_eq!(verification.commands, None);
        assert_eq!(verification.before, None);
        assert_eq!(verification.after, None);
        assert_eq!(verification.side_effects, None);
        assert_eq!(verification.index, None);
        assert!(
            !dir.path().join("must-not-run.txt").exists(),
            "a failed before snapshot must not run any command"
        );
    }

    #[test]
    fn after_snapshot_failure_persists_commands_and_before() {
        let (dir, mut storage) = open_persist_storage("persist-after-error");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        // The command succeeds but deletes `.git`, so only the after snapshot
        // fails. The reference keeps the run and the before fingerprint and
        // only overwrites the overall status with `error`.
        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["rm -rf .git"],
            Duration::from_secs(5),
        )
        .expect("an after-snapshot failure is persisted as the error variant");
        assert!(result.persisted());
        let verification = result.verification();
        assert_eq!(verification.status, VerificationStatus::Error);
        assert_eq!(
            verification.reason.as_deref(),
            Some("git_fingerprint_failed")
        );
        assert_eq!(verification.index, None);
        assert_eq!(verification.after, None);
        assert_eq!(verification.side_effects, None);

        // The run is not discarded: the executed command and the before
        // fingerprint stay recorded.
        let commands = verification
            .commands
            .as_ref()
            .expect("the executed command must be preserved");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "rm -rf .git");
        assert_eq!(commands[0].exit_code, Some(0));
        assert_eq!(commands[0].reason, None);
        assert!(
            verification.before.is_some(),
            "the before fingerprint must be preserved"
        );

        // The persisted row carries the same compact verification.
        assert_eq!(
            persisted_round_verification(&mut storage, &round),
            *verification
        );
    }

    #[test]
    fn spawn_failure_with_after_snapshot_failure_preserves_run_and_before() {
        let (dir, mut storage) = open_persist_storage("persist-after-spawn");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        // A successful command removes `.git`, the next command fails to spawn
        // and the after snapshot then fails too. The reference keeps every
        // executed entry, the `spawn_failed` entry and the before fingerprint.
        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &[
                "true",
                "rm -rf .git",
                "bridge-verifier-no-such-executable-2f7a",
            ],
            Duration::from_secs(5),
        )
        .expect("the after-snapshot failure is persisted");
        assert!(result.persisted());
        let verification = result.verification();
        assert_eq!(verification.status, VerificationStatus::Error);
        assert_eq!(
            verification.reason.as_deref(),
            Some("git_fingerprint_failed")
        );
        assert_eq!(verification.after, None);
        assert_eq!(verification.side_effects, None);
        assert!(
            verification.before.is_some(),
            "the before fingerprint must be preserved"
        );

        let commands = verification.commands.as_ref().expect("commands");
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].command, "true");
        assert_eq!(commands[0].exit_code, Some(0));
        assert_eq!(commands[1].command, "rm -rf .git");
        assert_eq!(commands[1].exit_code, Some(0));
        assert_eq!(
            commands[2].command,
            "bridge-verifier-no-such-executable-2f7a"
        );
        assert_eq!(commands[2].reason.as_deref(), Some("spawn_failed"));
        assert_eq!(commands[2].exit_code, None);
        assert_eq!(commands[2].duration, None);
    }

    #[test]
    fn rejected_command_persists_the_unsafe_variant() {
        let (dir, mut storage) = open_persist_storage("persist-unsafe");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["touch ran.txt", "git push origin main"],
            Duration::from_secs(5),
        )
        .expect("a rejected list is persisted as the unsafe variant");
        assert!(result.persisted());
        let verification = result.verification();
        assert_eq!(verification.status, VerificationStatus::Unsafe);
        assert_eq!(verification.index, Some(1));
        assert_eq!(verification.reason.as_deref(), Some("git_write_blocked"));
        assert_eq!(verification.commands, None);
        assert_eq!(verification.before, None);
        assert_eq!(verification.after, None);
        assert!(
            !dir.path().join("ran.txt").exists(),
            "no command may run when the whole list is rejected up front"
        );
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_command_persists_the_timed_out_status() {
        let (dir, mut storage) = open_persist_storage("persist-timeout");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["sleep 98765.4321"],
            Duration::from_millis(200),
        )
        .expect("a timeout is persisted");
        let verification = result.verification();
        assert_eq!(verification.status, VerificationStatus::TimedOut);
        let command = &verification.commands.as_ref().expect("commands")[0];
        assert_eq!(command.timed_out, Some(true));
        assert_eq!(command.exit_code, Some(-9));
        assert!(command.output_tail.is_some());
        assert!(verification.before.is_some());
        assert!(verification.after.is_some());
    }

    #[test]
    fn nonzero_command_persists_the_failed_status_with_output_tail() {
        let (dir, mut storage) = open_persist_storage("persist-failed");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["false"],
            Duration::from_secs(5),
        )
        .expect("a non-zero exit is persisted");
        let verification = result.verification();
        assert_eq!(verification.status, VerificationStatus::Failed);
        let command = &verification.commands.as_ref().expect("commands")[0];
        assert_eq!(command.exit_code, Some(1));
        assert_eq!(command.output_tail.as_deref(), Some(""));
        assert_eq!(command.timed_out, None);
    }

    #[test]
    fn spawn_failure_is_recorded_as_a_reason_entry() {
        let (dir, mut storage) = open_persist_storage("persist-spawn");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["true", "bridge-verifier-no-such-executable-2f7a"],
            Duration::from_secs(5),
        )
        .expect("a spawn failure is recorded in the verification");
        let verification = result.verification();
        assert_eq!(verification.status, VerificationStatus::Failed);
        let commands = verification.commands.as_ref().expect("commands");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].command, "true");
        assert_eq!(commands[0].exit_code, Some(0));
        assert_eq!(
            commands[1].command,
            "bridge-verifier-no-such-executable-2f7a"
        );
        assert_eq!(commands[1].reason.as_deref(), Some("spawn_failed"));
        assert_eq!(commands[1].exit_code, None);
        assert_eq!(commands[1].duration, None);
        assert_eq!(commands[1].output_tail, None);
    }

    #[test]
    fn side_effects_are_persisted_as_paths() {
        let (dir, mut storage) = open_persist_storage("persist-side-effects");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["touch created.txt"],
            Duration::from_secs(5),
        )
        .expect("side effects are persisted");
        assert_eq!(
            result.verification().side_effects.as_deref(),
            Some(["created.txt".to_owned()].as_slice())
        );
    }

    #[test]
    fn persist_errors_and_debug_do_not_reveal_sensitive_inputs() {
        let (dir, mut storage) = open_persist_storage("persist-redact");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);
        let round = create_persist_round(&mut storage, dir.path());

        // The outcome `Debug` renders only the outcome and status; command text,
        // output, fingerprints and paths stay behind the explicit accessors.
        let result = persist_round(
            &mut storage,
            &round,
            dir.path(),
            &["touch SUPER_SECRET_CREATED_5c1f"],
            Duration::from_secs(5),
        )
        .expect("the run persists");
        let rendered = format!("{result:?}");
        assert!(!rendered.contains("SUPER_SECRET"));
        assert!(!rendered.contains("touch"));
        assert!(!rendered.contains(&dir.path().display().to_string()));
        assert_eq!(
            rendered,
            "PersistedVerification { outcome: \"persisted\", status: \"passed\" }"
        );

        // A storage failure carries no id, path or SQL in `Debug`/`Display`.
        let missing = RoundRef {
            task_id: TaskId::from_str("550e8400-e29b-41d4-a716-446655440099")
                .expect("missing task id"),
            project_id: persist_project(),
            round_number: 7,
        };
        let error = persist_round(
            &mut storage,
            &missing,
            dir.path(),
            &["true"],
            Duration::from_secs(5),
        )
        .expect_err("a missing task must fail");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("550e8400"));
        assert!(!rendered.contains("persist-project"));
        assert!(!rendered.contains("SELECT"));
        assert!(!rendered.contains(&dir.path().display().to_string()));
        assert_eq!(
            rendered,
            "PersistVerificationError(\"storage_failed\") storage_failed"
        );
    }
}
