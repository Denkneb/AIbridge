//! Isolated read-only Git baseline snapshot layer (task 4.7a).
//!
//! This crate reproduces the read-only baseline primitives of the reference
//! Python `git_snapshot` module
//! (`/home/denis/Python/agent_bridge/src/agent_bridge/git_snapshot.py`):
//! [`is_repository`] / [`check_repository`] (`is_repo`), [`head`],
//! [`status_porcelain`], [`status_paths`], [`dirty_paths`],
//! [`index_fingerprint`] and a minimal [`take_snapshot`]. It is intentionally
//! narrow: this task (4.7a) covers the *base* snapshot of a single worktree
//! only.
//!
//! Deliberately out of scope for this crate (later tasks):
//!
//! - the worktree manifest, file hashing, executable-bit/symlink identity and
//!   the `worktree_fingerprint` (4.7b);
//! - `changed_paths` / `committed_paths` and history ancestry (4.9);
//! - scope/policy violations, external or multi-repository snapshots and the
//!   worker/MCP envelope (4.8, 4.9, 5.x).
//!
//! Every Git command is read-only and runs through a bounded runner: a fixed
//! `git` executable, closed standard input, discarded standard error, raw
//! standard-output bytes, and a wall-clock timeout after which the child is
//! killed and reaped. There is no shell, no string interpolation and no Git
//! write command anywhere in the production code.
//!
//! Fail-closed policy:
//!
//! - a worktree check that reports `false` or exits non-zero yields the typed
//!   [`GitError::NotRepository`];
//! - spawn, wait, timeout, an unreadable stdout pipe and malformed output all
//!   yield a typed infrastructure error ([`GitError::Spawn`],
//!   [`GitError::Wait`], [`GitError::Timeout`], [`GitError::Io`],
//!   [`GitError::MalformedOutput`]);
//! - a failing `git status` / `git ls-files` yields
//!   [`GitError::CommandFailed`].
//!
//! On Unix, repository-relative paths are carried as [`OsString`] bytes and are
//! never decoded to a lossy `String`, so a non-UTF-8 path survives a snapshot
//! unchanged. A commit id is an opaque ASCII string validated against the exact
//! expected form (40 or 64 lowercase hexadecimal digits) without any lossy
//! decode.
//!
//! [`GitError`] carries no payload, so `Display`/`Debug` can never leak the
//! workspace, argv, stdout/stderr, Git configuration, OS error text or secrets.

mod runner;
mod sha256;

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::Path;
use std::time::Duration;

/// Maximum wall-clock time allowed for one Git command.
///
/// Mirrors the reference `GIT_TIMEOUT`; a command that outlives it is killed
/// and reaped and reported as [`GitError::Timeout`].
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// The fixed production Git executable.
const GIT_PROGRAM: &str = "git";

/// A payload-free Git failure.
///
/// The variant is the whole contract: no workspace, argument vector,
/// stdout/stderr, Git configuration, OS error text or secret is ever stored in
/// it, so both `Debug` and `Display` render only a static identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GitError {
    /// The workspace is not a Git worktree (`git rev-parse
    /// --is-inside-work-tree` reported `false` or exited non-zero).
    NotRepository,
    /// A read-only Git subcommand exited non-zero.
    CommandFailed,
    /// Git output did not match the exact expected form.
    MalformedOutput,
    /// The command outlived its wall-clock timeout; the child was killed and
    /// reaped.
    Timeout,
    /// The Git process could not be started.
    Spawn,
    /// Waiting for the Git process failed; the child was killed and reaped.
    Wait,
    /// The stdout pipe was unavailable or could not be read.
    Io,
}

impl GitError {
    /// Returns the stable, payload-free identifier for this error.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRepository => "not_a_git_repo",
            Self::CommandFailed => "command_failed",
            Self::MalformedOutput => "malformed_output",
            Self::Timeout => "timeout",
            Self::Spawn => "spawn_failed",
            Self::Wait => "wait_failed",
            Self::Io => "io_failed",
        }
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for GitError {}

/// An opaque, validated Git commit id.
///
/// The value is the full 40- or 64-character lowercase hexadecimal object name
/// reported by `git rev-parse HEAD`; it is never decoded lossily and never
/// parsed further.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommitId(String);

impl CommitId {
    /// Returns the commit id as an ASCII string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The SHA-256 fingerprint of a Git index.
///
/// Computed exactly like the reference `git_snapshot.index_fingerprint`:
/// `sha256(git ls-files --stage -z || 0x00 || git ls-files -v -z)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct IndexFingerprint([u8; 32]);

impl IndexFingerprint {
    /// Returns the raw 32-byte digest.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns the digest as lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(&self) -> String {
        sha256::to_hex(&self.0)
    }
}

impl fmt::Debug for IndexFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IndexFingerprint({})", self.to_hex())
    }
}

impl fmt::Display for IndexFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// The base snapshot of a single Git worktree (task 4.7a).
///
/// It records exactly the HEAD commit, the raw `git status --porcelain=v1 -z`
/// bytes, the deduplicated and sorted dirty paths, and the index fingerprint.
/// There is deliberately no worktree fingerprint or manifest: those belong to
/// task 4.7b.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySnapshot {
    head: Option<CommitId>,
    status: Vec<u8>,
    dirty_paths: Vec<OsString>,
    index_fingerprint: IndexFingerprint,
}

impl RepositorySnapshot {
    /// Returns the HEAD commit, or `None` for a repository without commits.
    #[must_use]
    pub fn head(&self) -> Option<&CommitId> {
        self.head.as_ref()
    }

    /// Returns the raw `git status --porcelain=v1 -z` bytes.
    #[must_use]
    pub fn status(&self) -> &[u8] {
        &self.status
    }

    /// Returns the sorted, deduplicated dirty paths.
    #[must_use]
    pub fn dirty_paths(&self) -> &[OsString] {
        &self.dirty_paths
    }

    /// Returns the index fingerprint.
    #[must_use]
    pub fn index_fingerprint(&self) -> &IndexFingerprint {
        &self.index_fingerprint
    }
}

/// Runs one read-only Git command with the production timeout.
fn run_git(workspace: &Path, args: &[&str]) -> Result<runner::GitOutput, GitError> {
    let owned: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    runner::run_bounded(OsStr::new(GIT_PROGRAM), workspace, &owned, GIT_TIMEOUT)
}

/// Runs one read-only Git command and fails closed on a non-zero exit.
fn run_checked(workspace: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let output = run_git(workspace, args)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitError::CommandFailed)
    }
}

/// Strips ASCII whitespace from both ends of `bytes`, like Python `bytes.strip()`.
fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let is_whitespace = |byte: u8| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && is_whitespace(bytes[start]) {
        start += 1;
    }
    while end > start && is_whitespace(bytes[end - 1]) {
        end -= 1;
    }
    &bytes[start..end]
}

/// Converts raw Unix path bytes into an [`OsString`] without lossy decoding.
#[cfg(unix)]
fn os_from_bytes(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes.to_vec())
}

/// Converts raw path bytes into an [`OsString`] on non-Unix platforms.
#[cfg(not(unix))]
fn os_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Validates a `git rev-parse HEAD` response as an opaque commit id.
///
/// The trimmed output must be exactly 40 or 64 lowercase hexadecimal digits;
/// anything else fails closed as [`GitError::MalformedOutput`]. No lossy decode
/// is performed.
fn parse_commit_id(stdout: &[u8]) -> Result<CommitId, GitError> {
    let trimmed = trim_ascii_whitespace(stdout);
    if trimmed.len() != 40 && trimmed.len() != 64 {
        return Err(GitError::MalformedOutput);
    }
    if !trimmed
        .iter()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(GitError::MalformedOutput);
    }
    let text = std::str::from_utf8(trimmed).map_err(|_| GitError::MalformedOutput)?;
    Ok(CommitId(text.to_owned()))
}

/// Returns whether `workspace` is inside a Git worktree.
///
/// Equivalent to `git rev-parse --is-inside-work-tree`: a non-zero exit yields
/// `Ok(false)` and the exact `true`/`false` output is required. Spawn, wait,
/// timeout, I/O and any other (malformed) output fail closed as an
/// infrastructure [`GitError`].
///
/// # Errors
///
/// Returns [`GitError::Spawn`], [`GitError::Wait`], [`GitError::Timeout`],
/// [`GitError::Io`] or [`GitError::MalformedOutput`]. The error carries no
/// payload.
pub fn is_repository(workspace: &Path) -> Result<bool, GitError> {
    let output = run_git(workspace, &["rev-parse", "--is-inside-work-tree"])?;
    if !output.status.success() {
        return Ok(false);
    }
    match trim_ascii_whitespace(&output.stdout) {
        b"true" => Ok(true),
        b"false" => Ok(false),
        _ => Err(GitError::MalformedOutput),
    }
}

/// Fails closed unless `workspace` is inside a Git worktree.
///
/// A `false` or non-zero worktree check is reported as the typed
/// [`GitError::NotRepository`].
///
/// # Errors
///
/// Returns [`GitError::NotRepository`] for a non-repository and the same
/// infrastructure errors as [`is_repository`].
pub fn check_repository(workspace: &Path) -> Result<(), GitError> {
    if is_repository(workspace)? {
        Ok(())
    } else {
        Err(GitError::NotRepository)
    }
}

/// Returns the HEAD commit of `workspace`, if any.
///
/// Runs `git rev-parse HEAD`. A non-zero exit (a valid repository without a
/// commit) yields `Ok(None)`; on success the output must be an exact opaque
/// commit id ([`CommitId`]).
///
/// # Errors
///
/// Returns the bounded-runner infrastructure errors and
/// [`GitError::MalformedOutput`] for an unexpected commit id form. The error
/// carries no payload.
pub fn head(workspace: &Path) -> Result<Option<CommitId>, GitError> {
    let output = run_git(workspace, &["rev-parse", "HEAD"])?;
    if !output.status.success() {
        return Ok(None);
    }
    parse_commit_id(&output.stdout).map(Some)
}

/// Returns the raw `git status --porcelain=v1 -z` bytes.
///
/// # Errors
///
/// Returns the bounded-runner infrastructure errors and
/// [`GitError::CommandFailed`] for a non-zero exit.
pub fn status_porcelain(workspace: &Path) -> Result<Vec<u8>, GitError> {
    run_checked(workspace, &["status", "--porcelain=v1", "-z"])
}

/// Extracts both sides of every path from porcelain v1 `-z` output.
///
/// Ordinary entries contribute their path; a rename or copy entry contributes
/// both the destination and the source. The result is sorted and
/// deduplicated. Paths are kept as raw Unix [`OsString`] bytes and never
/// decoded lossily.
///
/// Only the canonical framing is accepted: an empty byte slice is the clean
/// status, otherwise the stream must be a sequence of NUL-terminated records
/// ending in a terminal NUL. A missing terminal NUL, an embedded empty field,
/// an entry shorter than four bytes, a missing separator, or a rename/copy
/// without its single non-empty second path all fail closed as
/// [`GitError::MalformedOutput`].
///
/// # Errors
///
/// Returns [`GitError::MalformedOutput`] for malformed porcelain bytes.
pub fn status_paths(status: &[u8]) -> Result<Vec<OsString>, GitError> {
    if status.is_empty() {
        return Ok(Vec::new());
    }
    let Some(body) = status.strip_suffix(b"\0") else {
        return Err(GitError::MalformedOutput);
    };
    let mut paths = std::collections::BTreeSet::new();
    let mut fields = body.split(|&byte| byte == 0);
    while let Some(entry) = fields.next() {
        if entry.len() < 4 || entry[2] != b' ' {
            return Err(GitError::MalformedOutput);
        }
        paths.insert(os_from_bytes(&entry[3..]));
        let code = &entry[..2];
        if code.contains(&b'R') || code.contains(&b'C') {
            let second = fields.next().ok_or(GitError::MalformedOutput)?;
            if second.is_empty() {
                return Err(GitError::MalformedOutput);
            }
            paths.insert(os_from_bytes(second));
        }
    }
    Ok(paths.into_iter().collect())
}

/// Returns the sorted, deduplicated dirty paths of `workspace`.
///
/// # Errors
///
/// Returns the same errors as [`status_porcelain`] and [`status_paths`].
pub fn dirty_paths(workspace: &Path) -> Result<Vec<OsString>, GitError> {
    let status = status_porcelain(workspace)?;
    status_paths(&status)
}

/// Returns the SHA-256 fingerprint of the Git index.
///
/// The digest is taken over the exact bytes of `git ls-files --stage -z`, a NUL
/// separator, then `git ls-files -v -z`, matching the reference
/// `git_snapshot.index_fingerprint`.
///
/// # Errors
///
/// Returns the bounded-runner infrastructure errors and
/// [`GitError::CommandFailed`] when either listing exits non-zero.
pub fn index_fingerprint(workspace: &Path) -> Result<IndexFingerprint, GitError> {
    let staged = run_checked(workspace, &["ls-files", "--stage", "-z"])?;
    let flags = run_checked(workspace, &["ls-files", "-v", "-z"])?;
    let mut material = Vec::with_capacity(staged.len() + 1 + flags.len());
    material.extend_from_slice(&staged);
    material.push(0);
    material.extend_from_slice(&flags);
    Ok(IndexFingerprint(sha256::digest(&material)))
}

/// Takes the base snapshot of a single Git worktree.
///
/// The worktree check runs first, so a plain directory is reported as
/// [`GitError::NotRepository`] rather than as a failing subcommand. The
/// snapshot records HEAD, the raw status bytes, the dirty paths and the index
/// fingerprint.
///
/// # Errors
///
/// Returns [`GitError::NotRepository`] for a non-repository and the errors of
/// [`head`], [`status_porcelain`], [`status_paths`] and
/// [`index_fingerprint`] otherwise.
pub fn take_snapshot(workspace: &Path) -> Result<RepositorySnapshot, GitError> {
    check_repository(workspace)?;
    let status = status_porcelain(workspace)?;
    let head = head(workspace)?;
    let dirty_paths = status_paths(&status)?;
    let index_fingerprint = index_fingerprint(workspace)?;
    Ok(RepositorySnapshot {
        head,
        status,
        dirty_paths,
        index_fingerprint,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        GitError, check_repository, dirty_paths, index_fingerprint, is_repository, parse_commit_id,
        status_paths, take_snapshot,
    };
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bridge-git-test-{}-{tag}-{sequence}",
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
                "user.email=bridge-git@example.invalid",
                "-c",
                "user.name=bridge-git",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
    }

    fn os(name: &str) -> OsString {
        OsString::from(name)
    }

    #[test]
    fn clean_repository_snapshot() {
        let dir = TempDir::new("clean");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        assert_eq!(check_repository(dir.path()), Ok(()));
        assert_eq!(is_repository(dir.path()), Ok(true));

        let snapshot = take_snapshot(dir.path()).expect("snapshot");
        assert!(snapshot.head().is_some());
        assert!(snapshot.status().is_empty());
        assert!(snapshot.dirty_paths().is_empty());
        assert_eq!(snapshot.index_fingerprint().to_hex().len(), 64);
    }

    #[test]
    fn repository_without_commit_has_no_head() {
        let dir = TempDir::new("no-commit");
        init_repo(dir.path());

        let snapshot = take_snapshot(dir.path()).expect("snapshot");
        assert_eq!(snapshot.head(), None);
        assert!(snapshot.dirty_paths().is_empty());
    }

    #[test]
    fn dirty_tracked_and_untracked_paths() {
        let dir = TempDir::new("dirty");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        write_file(dir.path(), "module.py", "x = 2\n");
        write_file(dir.path(), "new.py", "y = 1\n");

        assert_eq!(
            dirty_paths(dir.path()).expect("dirty"),
            vec![os("module.py"), os("new.py")]
        );
    }

    #[test]
    fn staged_change_moves_index_fingerprint() {
        let dir = TempDir::new("staged");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let before = index_fingerprint(dir.path()).expect("fingerprint");
        write_file(dir.path(), "module.py", "x = 2\n");
        git(dir.path(), &["add", "--", "module.py"]);
        let after = index_fingerprint(dir.path()).expect("fingerprint");

        assert_ne!(before, after);
        assert_eq!(
            dirty_paths(dir.path()).expect("dirty"),
            vec![os("module.py")]
        );
    }

    #[test]
    fn intent_to_add_changes_index_fingerprint() {
        let dir = TempDir::new("intent");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let before = index_fingerprint(dir.path()).expect("fingerprint");
        write_file(dir.path(), "new.py", "y = 1\n");
        git(dir.path(), &["add", "-N", "--", "new.py"]);
        let after = index_fingerprint(dir.path()).expect("fingerprint");

        assert_ne!(before, after);
    }

    #[test]
    fn index_fingerprint_is_stable_without_changes() {
        let dir = TempDir::new("stable");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let first = index_fingerprint(dir.path()).expect("fingerprint");
        let second = index_fingerprint(dir.path()).expect("fingerprint");
        assert_eq!(first, second);
    }

    #[test]
    fn real_rename_reports_both_sides() {
        let dir = TempDir::new("rename");
        init_repo(dir.path());
        write_file(dir.path(), "old.py", "same content\n");
        commit(dir.path(), "init", &["old.py"]);
        git(dir.path(), &["mv", "old.py", "new.py"]);

        assert_eq!(
            dirty_paths(dir.path()).expect("dirty"),
            vec![os("new.py"), os("old.py")]
        );
    }

    #[test]
    fn porcelain_parser_handles_rename_and_copy_both_sides() {
        assert_eq!(
            status_paths(b"R  new.py\0old.py\0").expect("rename"),
            vec![os("new.py"), os("old.py")]
        );
        assert_eq!(
            status_paths(b"C  copy.py\0source.py\0").expect("copy"),
            vec![os("copy.py"), os("source.py")]
        );
    }

    #[test]
    fn porcelain_paths_are_sorted_and_deduplicated() {
        let input = b" M b.py\0 M a.py\0 M a.py\0?? c.py\0";
        assert_eq!(
            status_paths(input).expect("parse"),
            vec![os("a.py"), os("b.py"), os("c.py")]
        );
    }

    #[test]
    fn malformed_porcelain_fails_closed() {
        for input in [&b"??\0"[..], b"ABpath\0", b"R  new.py\0", b"R  new.py\0\0"] {
            assert_eq!(
                status_paths(input),
                Err(GitError::MalformedOutput),
                "{input:?}"
            );
        }
    }

    #[test]
    fn porcelain_requires_canonical_framing() {
        // Clean status is exactly the empty slice.
        assert_eq!(status_paths(b""), Ok(Vec::new()));

        for input in [
            // Non-empty stream without the mandatory terminal NUL.
            &b" M a.py"[..],
            b"?? a.py",
            b"R  new.py\0old.py",
            // Unexpected empty field before the terminal NUL.
            b" M a.py\0\0 M b.py\0",
            b"\0",
            b" M a.py\0\0",
            // A rename/copy second path may not be empty.
            b"R  new.py\0\0",
        ] {
            assert_eq!(
                status_paths(input),
                Err(GitError::MalformedOutput),
                "{input:?}"
            );
        }
    }

    #[test]
    fn non_utf8_porcelain_path_is_lossless() {
        use std::os::unix::ffi::OsStrExt;

        let raw = [0xff, 0xfe, b'.', b'p', b'y'];
        let mut input = Vec::new();
        input.extend_from_slice(b"?? ");
        input.extend_from_slice(&raw);
        input.push(0);

        let paths = status_paths(&input).expect("parse");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].as_os_str().as_bytes(), &raw);
    }

    #[test]
    fn snapshot_keeps_non_utf8_dirty_path() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let dir = TempDir::new("non-utf8");
        init_repo(dir.path());
        write_file(dir.path(), "module.py", "x = 1\n");
        commit(dir.path(), "init", &["module.py"]);

        let name = OsString::from_vec(vec![0xff, b'x']);
        std::fs::write(dir.path().join(&name), b"data").expect("write non-utf8 file");

        let dirty = dirty_paths(dir.path()).expect("dirty");
        assert!(
            dirty
                .iter()
                .any(|path| path.as_os_str().as_bytes() == name.as_os_str().as_bytes())
        );
    }

    #[test]
    fn plain_directory_is_not_a_repository() {
        let dir = TempDir::new("plain");
        assert_eq!(check_repository(dir.path()), Err(GitError::NotRepository));
        assert_eq!(is_repository(dir.path()), Ok(false));
        assert_eq!(take_snapshot(dir.path()), Err(GitError::NotRepository));
    }

    #[test]
    fn commit_id_form_is_strict() {
        let valid = b"0123456789abcdef0123456789abcdef01234567\n";
        assert_eq!(parse_commit_id(valid).expect("valid").as_str().len(), 40);
        let long = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
        assert_eq!(parse_commit_id(long).expect("valid").as_str().len(), 64);

        for input in [
            &b"deadbeef\n"[..],
            b"\n",
            b"0123456789ABCDEF0123456789ABCDEF01234567\n",
            &[0xff; 40],
        ] {
            assert_eq!(
                parse_commit_id(input),
                Err(GitError::MalformedOutput),
                "{input:?}"
            );
        }
    }

    #[test]
    fn errors_are_payload_free() {
        for (error, debug, display) in [
            (GitError::NotRepository, "NotRepository", "not_a_git_repo"),
            (GitError::CommandFailed, "CommandFailed", "command_failed"),
            (
                GitError::MalformedOutput,
                "MalformedOutput",
                "malformed_output",
            ),
            (GitError::Timeout, "Timeout", "timeout"),
            (GitError::Spawn, "Spawn", "spawn_failed"),
            (GitError::Wait, "Wait", "wait_failed"),
            (GitError::Io, "Io", "io_failed"),
        ] {
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(format!("{error}"), display);
        }
    }
}
