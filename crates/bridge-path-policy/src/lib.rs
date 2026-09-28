//! Minimal path-policy primitives for `allowed_paths` (tasks 4.4-4.6).
//!
//! This crate reproduces the path-resolution branches of the reference Python
//! `git_snapshot.validate_allowed_paths` and
//! `git_snapshot.group_allowed_paths_by_repo`
//! (`/home/denis/Python/agent_bridge/src/agent_bridge/git_snapshot.py`). It
//! offers three layers:
//!
//! - a purely lexical layer ([`validate_allowed_paths`],
//!   [`validate_allowed_path_entries`]) that never touches the filesystem, so a
//!   missing file or directory behaves exactly like an existing one (4.4);
//! - a filesystem-aware workspace layer ([`validate_workspace_allowed_paths`],
//!   [`validate_workspace_allowed_path_entries`]) that canonicalizes the
//!   workspace and resolves existing symlink components plus the missing tail
//!   like Python `Path.resolve(strict=False)`, rejecting any entry whose
//!   resolved target escapes the canonical workspace (4.5);
//! - a filesystem-aware trusted-root layer
//!   ([`validate_allowed_paths_with_trusted_roots`],
//!   [`validate_allowed_path_entries_with_trusted_roots`]) that accepts
//!   absolute entries only inside a canonical trusted external root and inside a
//!   Git repository whose canonical root is itself inside a trusted root, plus
//!   the narrow [`group_allowed_paths_by_repo`] grouping API (4.6).
//!
//! Deliberately out of scope for this crate (later tasks):
//!
//! - scope matching, snapshots, status/index/HEAD comparison (4.7-4.9).
//!
//! The lexical rules, in reference order, are:
//!
//! - an empty `allowed_paths` list is valid and normalizes to an empty scope;
//! - every entry must be a non-empty string
//!   ([`PathPolicyReason::InvalidAllowedPathsEntry`]);
//! - an entry containing a backslash is rejected
//!   ([`PathPolicyReason::BackslashInPath`]);
//! - an entry with a `..` component is rejected
//!   ([`PathPolicyReason::ParentTraversal`]);
//! - an entry that reduces to the empty string or `.` is rejected
//!   ([`PathPolicyReason::EmptyOrDotPath`]);
//! - an absolute entry is rejected
//!   ([`PathPolicyReason::AbsolutePath`]);
//! - any remaining relative entry is normalized by dropping `.` and empty
//!   components, and the file/directory distinction is preserved with a
//!   trailing `/` for directory scopes.
//!
//! The check order mirrors the reference: a bare root slash (`/`, `//`) is
//! reported as [`PathPolicyReason::EmptyOrDotPath`] before the absolute check,
//! and the `..` scan runs before normalization, so `a/../b` is rejected even
//! though it would normalize back inside the workspace.
//!
//! The lexical APIs reject every absolute entry with
//! [`PathPolicyReason::AbsolutePath`] (the corpus `absolute_workspace_path`
//! category): the workspace-relative policy requires a relative path. The
//! trusted-root layer accepts some absolute entries instead.
//!
//! The filesystem-aware layers apply the lexical rules first and then resolve
//! the lexically normalized entry. Existing symlink components and the missing
//! tail are handled like Python `Path.resolve(strict=False)`: existing links
//! are followed, `..` in a link target is folded, and a component that does not
//! exist terminates resolution while the remaining components are joined
//! lexically. A resolved relative target that is not the canonical workspace or
//! a descendant of it is rejected as
//! [`PathPolicyReason::WorkspaceEscape`] (`workspace_escape`), covering both
//! existing and dangling symlink escapes. An accepted relative entry keeps its
//! normalized original workspace-relative scope (never the resolved target)
//! with the file/directory trailing slash preserved.
//!
//! For an absolute entry the trusted-root layer resolves the path and rejects
//! it, in reference order, when it lands inside the canonical main workspace
//! ([`PathPolicyReason::AbsolutePath`], checked before any trusted-root lookup),
//! when it resolves outside every canonical trusted root
//! ([`PathPolicyReason::OutsideTrustedRoots`]), when it does not belong to any
//! Git repository ([`PathPolicyReason::NotGitRepository`]) or when the canonical
//! root of the containing repository lies outside every trusted root
//! ([`PathPolicyReason::ExternalRepoRootOutsideTrusted`]). A nested trusted
//! subdirectory inside a higher repository never widens trust. An accepted
//! absolute entry is normalized to its canonical absolute path, preserving the
//! file/directory distinction.
//!
//! As a documented fail-closed extension over Python, symlink loops and
//! filesystem/canonicalization failures (workspace or trusted-root
//! canonicalization, non-`NotFound` metadata errors such as a non-directory
//! component, unreadable links, a failing or timed-out Git probe) fail closed
//! as [`PathPolicyReason::SymlinkLoop`] or
//! [`PathPolicyReason::ResolutionFailure`] instead of silently widening scope.
//! The Git probe is bounded: a probe that outlives its timeout is killed and
//! reaped, and its stdout is read as raw Unix path bytes (never decoded
//! lossily), so a repository root with a non-UTF-8 component is still found.
//!
//! Errors ([`PathPolicyReason`]) carry no payload, so a rejected entry never
//! leaks the original path, workspace, trusted roots, symlink target, Git
//! output or OS error text through `Debug`/`Display`.

use std::collections::VecDeque;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Returns the package name as a trivial smoke-check helper.
#[must_use]
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

/// Why a workspace-relative `allowed_paths` entry was rejected.
///
/// The reason carries no payload, so a rejected entry never leaks the original
/// path or any secrets through `Debug`/`Display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PathPolicyReason {
    /// The entry is not a string or is an empty string. Mirrors the reference
    /// category `invalid_allowed_paths_entry`.
    InvalidAllowedPathsEntry,
    /// The entry contains a backslash. Mirrors the reference category
    /// `backslash_in_path`.
    BackslashInPath,
    /// The entry contains a `..` component. Mirrors the reference category
    /// `parent_traversal`.
    ParentTraversal,
    /// The entry reduces to the empty string or `.`, including a bare root
    /// slash. Mirrors the reference category `empty_or_dot_path`.
    EmptyOrDotPath,
    /// The entry is absolute and must be expressed as a relative path. Mirrors
    /// the reference category `absolute_workspace_path`.
    AbsolutePath,
    /// A relative entry resolves outside the canonical workspace through an
    /// existing or dangling symlink. Mirrors the reference category
    /// `workspace_escape`.
    WorkspaceEscape,
    /// Symlink resolution did not terminate within the supported depth, which
    /// is a symlink loop. Fail-closed extension over the reference.
    SymlinkLoop,
    /// The workspace could not be canonicalized or a path component could not
    /// be inspected. Fail-closed extension over the reference.
    ResolutionFailure,
    /// An absolute entry resolves outside every canonical trusted external
    /// root, including when the escape happens through a symlink. Mirrors the
    /// reference category `outside_trusted_roots`.
    OutsideTrustedRoots,
    /// An absolute entry does not belong to any Git repository. Mirrors the
    /// reference category `not_git_repository`.
    NotGitRepository,
    /// An absolute entry resolves inside a trusted root, but the canonical root
    /// of the containing Git repository lies outside every trusted root.
    /// Mirrors the reference category `external_repo_root_outside_trusted`.
    ExternalRepoRootOutsideTrusted,
}

impl PathPolicyReason {
    /// Returns the stable reason identifier, matching the reference
    /// `reason_category` strings for the categories implemented here.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidAllowedPathsEntry => "invalid_allowed_paths_entry",
            Self::BackslashInPath => "backslash_in_path",
            Self::ParentTraversal => "parent_traversal",
            Self::EmptyOrDotPath => "empty_or_dot_path",
            Self::AbsolutePath => "absolute_workspace_path",
            Self::WorkspaceEscape => "workspace_escape",
            Self::SymlinkLoop => "symlink_loop",
            Self::ResolutionFailure => "path_resolution_failure",
            Self::OutsideTrustedRoots => "outside_trusted_roots",
            Self::NotGitRepository => "not_git_repository",
            Self::ExternalRepoRootOutsideTrusted => "external_repo_root_outside_trusted",
        }
    }
}

impl fmt::Display for PathPolicyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for PathPolicyReason {}

/// One raw `allowed_paths` entry at the JSON/harness boundary.
///
/// [`AllowedPathEntry::NonText`] models a non-string JSON value (number, null,
/// boolean, array, object) that a boundary decoder can encounter. The typed
/// string API [`validate_allowed_paths`] cannot express such a value; the
/// boundary entry point [`validate_allowed_path_entries`] can, and rejects it
/// fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AllowedPathEntry<'a> {
    /// A string entry.
    Text(&'a str),
    /// A non-string entry; rejected as
    /// [`PathPolicyReason::InvalidAllowedPathsEntry`].
    NonText,
}

impl<'a> From<&'a str> for AllowedPathEntry<'a> {
    fn from(value: &'a str) -> Self {
        Self::Text(value)
    }
}

/// One lexically checked entry before the absolute/relative decision.
struct LexicalEntry {
    /// The entry with `.` and empty components removed, no trailing slash.
    normalized: String,
    /// Whether the raw entry ended with `/` (directory scope).
    is_dir: bool,
    /// Whether the raw entry started with `/`.
    is_absolute: bool,
}

/// Applies the shared reference lexical checks and normalization.
///
/// The reference order is preserved: empty string, backslash, `..`, then the
/// empty/dot reduction. The absolute/relative split happens in the callers so
/// the workspace-relative policy can reject absolute entries while the
/// trusted-root policy can resolve them.
fn classify_lexical(raw: &str) -> Result<LexicalEntry, PathPolicyReason> {
    if raw.is_empty() {
        return Err(PathPolicyReason::InvalidAllowedPathsEntry);
    }
    if raw.contains('\\') {
        return Err(PathPolicyReason::BackslashInPath);
    }

    let is_dir = raw.ends_with('/');
    let mut normalized = String::new();
    for part in raw.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(PathPolicyReason::ParentTraversal);
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(part);
    }

    if normalized.is_empty() {
        return Err(PathPolicyReason::EmptyOrDotPath);
    }
    Ok(LexicalEntry {
        normalized,
        is_dir,
        is_absolute: raw.starts_with('/'),
    })
}

/// Validates and normalizes one non-empty workspace-relative string entry.
///
/// Normalization drops `.` and empty components and appends a trailing `/` when
/// the raw entry ended with one, so file and directory scopes stay
/// distinguishable. Absolute entries are rejected as
/// [`PathPolicyReason::AbsolutePath`].
fn classify_entry(raw: &str) -> Result<String, PathPolicyReason> {
    let lexical = classify_lexical(raw)?;
    if lexical.is_absolute {
        return Err(PathPolicyReason::AbsolutePath);
    }
    let mut normalized = lexical.normalized;
    if lexical.is_dir {
        normalized.push('/');
    }
    Ok(normalized)
}

/// Validates and normalizes a list of workspace-relative `allowed_paths`
/// string entries.
///
/// An empty slice is valid and returns an empty vector. Each returned scope is
/// normalized deterministically: `.` and empty components are removed and a
/// trailing `/` marks a directory scope, preserving the file/directory
/// distinction. The function never touches the filesystem, so missing paths
/// are handled lexically.
///
/// # Errors
///
/// Returns the first [`PathPolicyReason`] encountered, without the offending
/// path. See [`classify_entry`] for the reference check order.
pub fn validate_allowed_paths(paths: &[&str]) -> Result<Vec<String>, PathPolicyReason> {
    paths.iter().map(|path| classify_entry(path)).collect()
}

/// Validates and normalizes raw `allowed_paths` entries at the JSON/harness
/// boundary, where an entry may be a non-string value.
///
/// [`AllowedPathEntry::Text`] behaves exactly like [`validate_allowed_paths`];
/// [`AllowedPathEntry::NonText`] fails closed as
/// [`PathPolicyReason::InvalidAllowedPathsEntry`].
///
/// # Errors
///
/// Returns the first [`PathPolicyReason`] encountered, without the offending
/// entry.
pub fn validate_allowed_path_entries(
    entries: &[AllowedPathEntry<'_>],
) -> Result<Vec<String>, PathPolicyReason> {
    entries
        .iter()
        .map(|entry| match entry {
            AllowedPathEntry::Text(path) => classify_entry(path),
            AllowedPathEntry::NonText => Err(PathPolicyReason::InvalidAllowedPathsEntry),
        })
        .collect()
}

/// Maximum number of symlinks resolved before a path is treated as a loop.
///
/// This mirrors the classic `SYMLOOP_MAX` fail-closed bound; a genuine loop can
/// never terminate, so the bound converts it into a typed error instead of an
/// unbounded walk.
const MAX_SYMLINK_DEPTH: usize = 40;

/// Canonicalizes the workspace root, failing closed on any filesystem error.
fn canonical_workspace(workspace: &Path) -> Result<PathBuf, PathPolicyReason> {
    std::fs::canonicalize(workspace).map_err(|_| PathPolicyReason::ResolutionFailure)
}

/// Resolves a queue of path components starting from an already resolved base.
///
/// The walk mirrors Python `Path.resolve(strict=False)`: existing symlink
/// components are followed (absolute targets restart at the filesystem root,
/// relative targets resolve against the link's parent, and `..`/`.` inside a
/// target are folded) and a missing component ends resolution while the
/// remaining components are joined lexically. No path, target or OS error text
/// is carried in the returned error.
fn resolve_components(
    mut resolved: PathBuf,
    mut queue: VecDeque<OsString>,
) -> Result<PathBuf, PathPolicyReason> {
    let mut links = 0usize;

    while let Some(part) = queue.pop_front() {
        let part = part.as_os_str();
        if part == OsStr::new(".") {
            continue;
        }
        if part == OsStr::new("..") {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(part);
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                links += 1;
                if links > MAX_SYMLINK_DEPTH {
                    return Err(PathPolicyReason::SymlinkLoop);
                }
                let target = std::fs::read_link(&candidate)
                    .map_err(|_| PathPolicyReason::ResolutionFailure)?;
                let mut target_parts: Vec<OsString> = Vec::new();
                for component in target.components() {
                    match component {
                        Component::Normal(name) => target_parts.push(name.to_os_string()),
                        Component::ParentDir => target_parts.push(OsString::from("..")),
                        Component::CurDir => {}
                        Component::RootDir => {}
                        Component::Prefix(_) => {
                            return Err(PathPolicyReason::ResolutionFailure);
                        }
                    }
                }
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                for target_part in target_parts.into_iter().rev() {
                    queue.push_front(target_part);
                }
            }
            Ok(_) => resolved.push(part),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                resolved.push(part);
                for remaining in queue.drain(..) {
                    let remaining = remaining.as_os_str();
                    if remaining == OsStr::new("..") {
                        resolved.pop();
                    } else if remaining != OsStr::new(".") {
                        resolved.push(remaining);
                    }
                }
                break;
            }
            Err(_) => return Err(PathPolicyReason::ResolutionFailure),
        }
    }

    Ok(resolved)
}

/// Splits a lexically normalized entry into path components for resolution.
fn components_of(normalized: &str) -> VecDeque<OsString> {
    normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .map(OsString::from)
        .collect()
}

/// Resolves one lexically normalized relative entry and checks confinement.
///
/// The final resolved path must equal the canonical workspace or be a
/// descendant of it; anything else is a symlink escape. No path, target or OS
/// error text is carried in the returned error.
fn confine_to_workspace(workspace: &Path, relative: &str) -> Result<(), PathPolicyReason> {
    let resolved = resolve_components(workspace.to_path_buf(), components_of(relative))?;
    if resolved == workspace || resolved.starts_with(workspace) {
        Ok(())
    } else {
        Err(PathPolicyReason::WorkspaceEscape)
    }
}

/// Validates and normalizes one workspace-relative entry with symlink
/// confinement against the canonical workspace.
fn classify_workspace_entry(workspace: &Path, raw: &str) -> Result<String, PathPolicyReason> {
    let normalized = classify_entry(raw)?;
    let stripped = normalized.strip_suffix('/').unwrap_or(&normalized);
    confine_to_workspace(workspace, stripped)?;
    Ok(normalized)
}

/// Validates and normalizes workspace-relative `allowed_paths` entries with
/// filesystem-aware symlink confinement (task 4.5).
///
/// The workspace is canonicalized safely and every entry is first checked with
/// the lexical rules of [`validate_allowed_paths`]. Each surviving relative
/// entry is then resolved against the canonical workspace following existing
/// symlink components and the missing tail like Python
/// `Path.resolve(strict=False)`; an entry whose resolved target leaves the
/// workspace is rejected as [`PathPolicyReason::WorkspaceEscape`]. Accepted
/// entries keep their normalized original workspace-relative scope, never the
/// resolved target, and preserve the trailing-slash file/directory
/// distinction.
///
/// An empty slice returns an empty scope without touching the filesystem.
///
/// # Errors
///
/// Returns the first [`PathPolicyReason`] encountered, without the offending
/// path, workspace, symlink target or OS error text. A missing or
/// non-canonicalizable workspace, an unreadable/non-directory component and a
/// symlink loop fail closed as [`PathPolicyReason::ResolutionFailure`] or
/// [`PathPolicyReason::SymlinkLoop`].
pub fn validate_workspace_allowed_paths(
    workspace: &Path,
    paths: &[&str],
) -> Result<Vec<String>, PathPolicyReason> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let workspace = canonical_workspace(workspace)?;
    paths
        .iter()
        .map(|path| classify_workspace_entry(&workspace, path))
        .collect()
}

/// Boundary variant of [`validate_workspace_allowed_paths`] that also accepts
/// non-string entries, rejecting them fail closed as
/// [`PathPolicyReason::InvalidAllowedPathsEntry`].
///
/// # Errors
///
/// Returns the first [`PathPolicyReason`] encountered, without the offending
/// entry, workspace, symlink target or OS error text.
pub fn validate_workspace_allowed_path_entries(
    workspace: &Path,
    entries: &[AllowedPathEntry<'_>],
) -> Result<Vec<String>, PathPolicyReason> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let workspace = canonical_workspace(workspace)?;
    entries
        .iter()
        .map(|entry| match entry {
            AllowedPathEntry::Text(path) => classify_workspace_entry(&workspace, path),
            AllowedPathEntry::NonText => Err(PathPolicyReason::InvalidAllowedPathsEntry),
        })
        .collect()
}

/// Whether a validated scope is a single file or a directory subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathScope {
    /// A file scope: only the exact path matches.
    File,
    /// A directory scope: the path and its strict descendants match.
    Directory,
}

impl PathScope {
    fn from_is_dir(is_dir: bool) -> Self {
        if is_dir { Self::Directory } else { Self::File }
    }
}

/// One validated `allowed_paths` entry (task 4.6).
///
/// Workspace-relative entries stay relative and are never rewritten to their
/// resolved target; absolute entries are canonical absolute paths inside a
/// trusted external root. The [`PathScope`] keeps the file/directory
/// distinction that a trailing slash expresses at the JSON boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValidatedAllowedPath {
    /// A normalized workspace-relative entry without a trailing slash.
    WorkspaceRelative {
        /// The normalized relative path.
        path: String,
        /// File or directory scope.
        scope: PathScope,
    },
    /// A canonical absolute entry inside a trusted external Git repository.
    External {
        /// The canonical absolute path.
        path: PathBuf,
        /// File or directory scope.
        scope: PathScope,
    },
}

impl ValidatedAllowedPath {
    /// Returns the file/directory scope of the entry.
    #[must_use]
    pub fn scope(&self) -> PathScope {
        match self {
            Self::WorkspaceRelative { scope, .. } | Self::External { scope, .. } => *scope,
        }
    }

    /// Returns `true` for canonical absolute external entries.
    #[must_use]
    pub fn is_external(&self) -> bool {
        matches!(self, Self::External { .. })
    }

    /// Returns the normalized path without a trailing slash.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::WorkspaceRelative { path, .. } => Path::new(path),
            Self::External { path, .. } => path,
        }
    }

    /// Renders the normalized scope string, adding a trailing `/` for a
    /// directory scope.
    #[must_use]
    pub fn to_scope_string(&self) -> String {
        let mut text = self.path().to_string_lossy().into_owned();
        if self.scope() == PathScope::Directory {
            text.push('/');
        }
        text
    }
}

/// Canonicalizes the trusted external roots, failing closed on any error and
/// collapsing duplicates while preserving order.
fn canonical_trusted_roots(roots: &[&Path]) -> Result<Vec<PathBuf>, PathPolicyReason> {
    let mut canonical: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        let resolved =
            std::fs::canonicalize(root).map_err(|_| PathPolicyReason::ResolutionFailure)?;
        if !canonical.contains(&resolved) {
            canonical.push(resolved);
        }
    }
    Ok(canonical)
}

/// Maximum wall-clock time allowed for one Git repository probe.
///
/// Mirrors the reference `GIT_TIMEOUT`; a probe that outlives it is an
/// infrastructure failure, not evidence that the path is outside a repository,
/// so callers fail closed.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval for the bounded Git probe lifecycle.
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Runs one bounded `git rev-parse --show-toplevel` probe.
///
/// `program` is normally `git` but is injectable so tests can exercise the
/// process lifecycle. On success the raw stdout bytes are returned; a non-zero
/// exit yields `Ok(None)` (the path is not in a repository); a spawn error, a
/// wait error or a timeout yields [`PathPolicyReason::ResolutionFailure`]. On
/// timeout the child is killed and reaped, so no probe process is left behind.
/// No Git output, path or OS error text is carried in the returned error.
fn run_bounded_git_probe(
    program: &OsStr,
    probe: &Path,
    timeout: Duration,
) -> Result<Option<Vec<u8>>, PathPolicyReason> {
    let mut child = std::process::Command::new(program)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(probe)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| PathPolicyReason::ResolutionFailure)?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                if let Some(mut pipe) = child.stdout.take() {
                    pipe.read_to_end(&mut stdout)
                        .map_err(|_| PathPolicyReason::ResolutionFailure)?;
                }
                if status.success() {
                    return Ok(Some(stdout));
                }
                return Ok(None);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(PathPolicyReason::ResolutionFailure);
                }
                std::thread::sleep(GIT_POLL_INTERVAL);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(PathPolicyReason::ResolutionFailure);
            }
        }
    }
}

/// Converts raw `git rev-parse --show-toplevel` bytes into a path.
///
/// On Unix the bytes are interpreted directly as an `OsString` (the reference
/// decodes with UTF-8 `surrogateescape`), so a valid repository root with a
/// non-UTF-8 component is not misclassified as [`PathPolicyReason::NotGitRepository`].
/// Only a trailing ASCII line ending is removed; the bytes are never decoded
/// lossily or strictly. The result is never carried in an error.
fn git_stdout_path(bytes: &[u8]) -> PathBuf {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    let bytes = &bytes[..end];
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Finds the canonical root of the Git worktree containing `path`, if any.
///
/// Mirrors the reference `repo_root_for`: the nearest existing directory at or
/// above `path` is probed with `git rev-parse --show-toplevel`, the reported
/// root is canonicalized, and it must be `path` or an ancestor of it. A missing
/// leaf therefore uses its nearest existing ancestor. A Git failure or
/// non-absolute/invalid output yields `Ok(None)` (reported as
/// [`PathPolicyReason::NotGitRepository`]); an inability to launch the Git probe
/// or a probe that exceeds [`GIT_TIMEOUT`] fails closed as
/// [`PathPolicyReason::ResolutionFailure`] with the child killed and reaped.
/// The probe output is read as raw bytes (never lossily decoded), and neither
/// Git output nor OS error text is carried in the returned error.
fn git_repository_root(path: &Path) -> Result<Option<PathBuf>, PathPolicyReason> {
    git_repository_root_with_timeout(path, GIT_TIMEOUT)
}

/// [`git_repository_root`] with an explicit probe timeout, so the bound can be
/// kept small in tests without changing the production default.
fn git_repository_root_with_timeout(
    path: &Path,
    timeout: Duration,
) -> Result<Option<PathBuf>, PathPolicyReason> {
    let mut probe = path.to_path_buf();
    while !probe.is_dir() {
        match probe.parent() {
            Some(parent) if parent != probe => probe = parent.to_path_buf(),
            _ => return Ok(None),
        }
    }

    let Some(stdout) = run_bounded_git_probe(OsStr::new("git"), &probe, timeout)? else {
        return Ok(None);
    };
    let reported = git_stdout_path(&stdout);
    if reported.as_os_str().is_empty() || !reported.is_absolute() {
        return Ok(None);
    }
    let Ok(root) = std::fs::canonicalize(reported) else {
        return Ok(None);
    };
    if path.starts_with(&root) {
        Ok(Some(root))
    } else {
        Ok(None)
    }
}

/// Validates and normalizes one absolute entry against the canonical trusted
/// roots, in reference order.
fn classify_external_entry(
    workspace: &Path,
    trusted: &[PathBuf],
    lexical: &LexicalEntry,
) -> Result<ValidatedAllowedPath, PathPolicyReason> {
    let resolved = resolve_components(PathBuf::from("/"), components_of(&lexical.normalized))?;
    if resolved == workspace || resolved.starts_with(workspace) {
        return Err(PathPolicyReason::AbsolutePath);
    }
    if !trusted.iter().any(|root| resolved.starts_with(root)) {
        return Err(PathPolicyReason::OutsideTrustedRoots);
    }
    let repo = git_repository_root(&resolved)?.ok_or(PathPolicyReason::NotGitRepository)?;
    if !trusted.iter().any(|root| repo.starts_with(root)) {
        return Err(PathPolicyReason::ExternalRepoRootOutsideTrusted);
    }
    Ok(ValidatedAllowedPath::External {
        path: resolved,
        scope: PathScope::from_is_dir(lexical.is_dir),
    })
}

/// Validates one entry for the trusted-root layer, dispatching on absoluteness.
fn classify_validated_entry(
    workspace: &Path,
    trusted: &[PathBuf],
    raw: &str,
) -> Result<ValidatedAllowedPath, PathPolicyReason> {
    let lexical = classify_lexical(raw)?;
    if lexical.is_absolute {
        return classify_external_entry(workspace, trusted, &lexical);
    }
    confine_to_workspace(workspace, &lexical.normalized)?;
    Ok(ValidatedAllowedPath::WorkspaceRelative {
        path: lexical.normalized,
        scope: PathScope::from_is_dir(lexical.is_dir),
    })
}

/// Validates and normalizes `allowed_paths` with filesystem-aware workspace
/// confinement and trusted external Git repository resolution (task 4.6).
///
/// Relative entries behave exactly like [`validate_workspace_allowed_paths`]:
/// they are normalized, resolved against the canonical workspace and rejected
/// as [`PathPolicyReason::WorkspaceEscape`] when they escape it. Absolute
/// entries are resolved like Python `Path.resolve(strict=False)` and rejected,
/// in reference order, as [`PathPolicyReason::AbsolutePath`] when they land
/// inside the canonical main workspace (checked before any trusted-root
/// lookup), as [`PathPolicyReason::OutsideTrustedRoots`] when they resolve
/// outside every canonical trusted root, as
/// [`PathPolicyReason::NotGitRepository`] when they belong to no Git repository
/// and as [`PathPolicyReason::ExternalRepoRootOutsideTrusted`] when the
/// canonical root of the containing repository lies outside every trusted root.
/// A trusted root nested inside a higher repository never widens trust.
///
/// An empty slice returns an empty scope without touching the filesystem.
///
/// # Errors
///
/// Returns the first [`PathPolicyReason`] encountered, without the offending
/// path, workspace, trusted roots, symlink target, Git output or OS error text.
pub fn validate_allowed_paths_with_trusted_roots(
    workspace: &Path,
    trusted_roots: &[&Path],
    paths: &[&str],
) -> Result<Vec<ValidatedAllowedPath>, PathPolicyReason> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let workspace = canonical_workspace(workspace)?;
    let trusted = canonical_trusted_roots(trusted_roots)?;
    paths
        .iter()
        .map(|path| classify_validated_entry(&workspace, &trusted, path))
        .collect()
}

/// Boundary variant of [`validate_allowed_paths_with_trusted_roots`] that also
/// accepts non-string entries, rejecting them fail closed as
/// [`PathPolicyReason::InvalidAllowedPathsEntry`].
///
/// # Errors
///
/// Returns the first [`PathPolicyReason`] encountered, without the offending
/// entry, workspace, trusted roots, symlink target, Git output or OS error text.
pub fn validate_allowed_path_entries_with_trusted_roots(
    workspace: &Path,
    trusted_roots: &[&Path],
    entries: &[AllowedPathEntry<'_>],
) -> Result<Vec<ValidatedAllowedPath>, PathPolicyReason> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let workspace = canonical_workspace(workspace)?;
    let trusted = canonical_trusted_roots(trusted_roots)?;
    entries
        .iter()
        .map(|entry| match entry {
            AllowedPathEntry::Text(path) => classify_validated_entry(&workspace, &trusted, path),
            AllowedPathEntry::NonText => Err(PathPolicyReason::InvalidAllowedPathsEntry),
        })
        .collect()
}

/// One repository bucket produced by [`group_allowed_paths_by_repo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedPathGroup {
    root: PathBuf,
    entries: Vec<String>,
}

impl AllowedPathGroup {
    /// Returns the canonical repository root of the bucket.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the raw entries assigned to the bucket, in input order and
    /// without normalization or validation.
    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }
}

/// Groups `allowed_paths` by canonical containing repository root (task 4.6).
///
/// Relative entries are assigned to the canonical main workspace root exactly
/// as supplied, with no normalization or validation. Absolute entries are
/// resolved like Python `Path.resolve(strict=False)` (a missing leaf uses its
/// nearest existing ancestor) and assigned to the canonical root of the
/// containing Git repository. Trusted roots are not consulted. The main
/// workspace bucket is always present and comes first; external buckets follow
/// in ascending root order, so the result is deterministic.
///
/// # Errors
///
/// Returns [`PathPolicyReason::ResolutionFailure`] when the workspace or a
/// trusted-independent path cannot be resolved, and
/// [`PathPolicyReason::NotGitRepository`] when an absolute entry belongs to no
/// Git repository. The error never carries a path, Git output or OS error text.
pub fn group_allowed_paths_by_repo(
    workspace: &Path,
    paths: &[&str],
) -> Result<Vec<AllowedPathGroup>, PathPolicyReason> {
    let workspace = canonical_workspace(workspace)?;
    let mut workspace_entries: Vec<String> = Vec::new();
    let mut external: Vec<(PathBuf, Vec<String>)> = Vec::new();

    for &raw in paths {
        if !raw.starts_with('/') {
            workspace_entries.push(raw.to_owned());
            continue;
        }
        let resolved = resolve_components(PathBuf::from("/"), components_of(raw))?;
        let repo = git_repository_root(&resolved)?.ok_or(PathPolicyReason::NotGitRepository)?;
        match external.iter_mut().find(|(root, _)| *root == repo) {
            Some((_, entries)) => entries.push(raw.to_owned()),
            None => external.push((repo, vec![raw.to_owned()])),
        }
    }

    external.sort_by(|left, right| left.0.cmp(&right.0));
    let mut groups = Vec::with_capacity(external.len() + 1);
    groups.push(AllowedPathGroup {
        root: workspace,
        entries: workspace_entries,
    });
    groups.extend(
        external
            .into_iter()
            .map(|(root, entries)| AllowedPathGroup { root, entries }),
    );
    Ok(groups)
}

#[cfg(all(test, unix))]
mod workspace_tests {
    use super::{
        AllowedPathEntry, PathPolicyReason, validate_workspace_allowed_path_entries,
        validate_workspace_allowed_paths,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bridge-path-policy-test-{}-{tag}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn missing_workspace(tag: &str) -> PathBuf {
        let dir = TempDir::new(tag);
        let path = dir.path().to_path_buf();
        drop(dir);
        path
    }

    fn mkdir(path: &Path) {
        std::fs::create_dir_all(path).expect("create directory");
    }

    fn mkfile(path: &Path) {
        if let Some(parent) = path.parent() {
            mkdir(parent);
        }
        std::fs::write(path, b"content").expect("write file");
    }

    fn link(target: &Path, link_path: &Path) {
        std::os::unix::fs::symlink(target, link_path).expect("create symlink");
    }

    #[test]
    fn empty_list_is_allowed_without_filesystem() {
        let missing = missing_workspace("empty-list");
        assert_eq!(
            validate_workspace_allowed_paths(&missing, &[]),
            Ok(Vec::new())
        );
        assert_eq!(
            validate_workspace_allowed_path_entries(&missing, &[]),
            Ok(Vec::new())
        );
    }

    /// Task 4.5 subset of `docs/fixtures/path-policy-cases.json`: the four
    /// `validate_allowed_paths` relative symlink/dangling branches. Absolute
    /// external paths and trusted roots (4.6) and scope/snapshot operations
    /// (4.7-4.9) are out of scope.
    #[test]
    fn reference_corpus_cases_4_5() {
        let fixture = TempDir::new("corpus-4-5");
        let ws = fixture.join("workspace");
        let outside = fixture.join("outside");
        mkdir(&ws);
        mkdir(&outside);

        mkdir(&ws.join("src"));
        link(&ws.join("src"), &ws.join("link-in"));
        link(&outside, &ws.join("escape"));
        link(&ws.join("missing-a"), &ws.join("dangling"));
        link(&outside.join("missing-b"), &ws.join("dangling-out"));

        let allow: [(&str, &str, &str); 2] = [
            (
                "validate-relative-symlink-inside-scope",
                "link-in/file.py",
                "link-in/file.py",
            ),
            (
                "validate-dangling-symlink-inside-scope",
                "dangling",
                "dangling",
            ),
        ];
        for (id, input, expected) in allow {
            assert_eq!(
                validate_workspace_allowed_paths(&ws, &[input]),
                Ok(vec![expected.to_owned()]),
                "{id} should be allowed"
            );
        }

        let deny: [(&str, &str); 2] = [
            ("validate-relative-symlink-escape", "escape/"),
            ("validate-relative-dangling-symlink-outside", "dangling-out"),
        ];
        for (id, input) in deny {
            let reason = validate_workspace_allowed_paths(&ws, &[input])
                .expect_err("entry should be denied");
            assert_eq!(reason.as_str(), "workspace_escape", "{id}");
        }
    }

    #[test]
    fn final_and_intermediate_symlinks_stay_confined() {
        let fixture = TempDir::new("intermediate");
        let ws = fixture.join("workspace");
        mkdir(&ws.join("real/sub"));
        mkfile(&ws.join("real/sub/file.py"));
        link(&ws.join("real"), &ws.join("link"));
        link(&ws.join("real/sub/file.py"), &ws.join("link-file.py"));

        assert_eq!(
            validate_workspace_allowed_paths(&ws, &["link/sub/file.py"]),
            Ok(vec!["link/sub/file.py".to_owned()])
        );
        assert_eq!(
            validate_workspace_allowed_paths(&ws, &["link-file.py"]),
            Ok(vec!["link-file.py".to_owned()])
        );
        assert_eq!(
            validate_workspace_allowed_paths(&ws, &["link/"]),
            Ok(vec!["link/".to_owned()])
        );
    }

    #[test]
    fn symlink_target_parent_components_are_folded() {
        let fixture = TempDir::new("parent-target");
        let ws = fixture.join("workspace");
        mkdir(&ws.join("real"));
        mkdir(&ws.join("sub"));
        link(Path::new("../real"), &ws.join("sub/inner"));
        link(Path::new("../../outside"), &ws.join("sub/up"));

        assert_eq!(
            validate_workspace_allowed_paths(&ws, &["sub/inner/file.py"]),
            Ok(vec!["sub/inner/file.py".to_owned()])
        );
        let reason = validate_workspace_allowed_paths(&ws, &["sub/up/file.py"])
            .expect_err("escape through a '..' target should be denied");
        assert_eq!(reason.as_str(), "workspace_escape");
    }

    #[test]
    fn symlink_loops_fail_closed() {
        let fixture = TempDir::new("loop");
        let ws = fixture.join("workspace");
        mkdir(&ws);
        link(Path::new("loop"), &ws.join("loop"));
        link(&ws.join("loop1"), &ws.join("loop2"));
        link(&ws.join("loop2"), &ws.join("loop1"));

        for input in ["loop", "loop/child", "loop1", "loop2/x"] {
            let reason = validate_workspace_allowed_paths(&ws, &[input])
                .expect_err("symlink loop should be denied");
            assert_eq!(reason, PathPolicyReason::SymlinkLoop, "{input}");
        }
    }

    #[test]
    fn workspace_canonicalization_failure_fails_closed() {
        let missing = missing_workspace("missing-workspace");
        let reason = validate_workspace_allowed_paths(&missing, &["module.py"])
            .expect_err("missing workspace should be denied");
        assert_eq!(reason, PathPolicyReason::ResolutionFailure);
    }

    #[test]
    fn non_directory_component_fails_closed() {
        let fixture = TempDir::new("nondir");
        let ws = fixture.join("workspace");
        mkdir(&ws);
        mkfile(&ws.join("file.txt"));

        let reason = validate_workspace_allowed_paths(&ws, &["file.txt/child"])
            .expect_err("non-directory component should be denied");
        assert_eq!(reason, PathPolicyReason::ResolutionFailure);
    }

    #[test]
    fn lexical_rejections_are_preserved_by_workspace_api() {
        let fixture = TempDir::new("lexical");
        let ws = fixture.join("workspace");
        mkdir(&ws);

        for (input, expected) in [
            ("", PathPolicyReason::InvalidAllowedPathsEntry),
            ("src\\module.py", PathPolicyReason::BackslashInPath),
            ("../outside.py", PathPolicyReason::ParentTraversal),
            (".", PathPolicyReason::EmptyOrDotPath),
            ("/etc/passwd", PathPolicyReason::AbsolutePath),
        ] {
            let reason = validate_workspace_allowed_paths(&ws, &[input])
                .expect_err("entry should be denied");
            assert_eq!(reason, expected, "{input:?}");
        }
    }

    #[test]
    fn workspace_boundary_rejects_non_text_and_preserves_scopes() {
        let fixture = TempDir::new("boundary");
        let ws = fixture.join("workspace");
        mkdir(&ws.join("src"));

        assert_eq!(
            validate_workspace_allowed_path_entries(
                &ws,
                &[AllowedPathEntry::Text("src/"), AllowedPathEntry::NonText],
            ),
            Err(PathPolicyReason::InvalidAllowedPathsEntry)
        );
        assert_eq!(
            validate_workspace_allowed_path_entries(&ws, &[AllowedPathEntry::Text("src/")]),
            Ok(vec!["src/".to_owned()])
        );
    }

    #[test]
    fn errors_do_not_reveal_paths_targets_or_os_text() {
        let fixture = TempDir::new("super-secret-root");
        let ws = fixture.join("workspace");
        mkdir(&ws);
        link(&fixture.join("secret-target"), &ws.join("escape"));
        link(Path::new("loop"), &ws.join("loop"));

        let escape = validate_workspace_allowed_paths(&ws, &["escape"])
            .expect_err("escape should be denied");
        assert_eq!(escape, PathPolicyReason::WorkspaceEscape);
        assert_eq!(format!("{escape:?}"), "WorkspaceEscape");
        assert_eq!(format!("{escape}"), "workspace_escape");

        let looped =
            validate_workspace_allowed_paths(&ws, &["loop"]).expect_err("loop should be denied");
        assert_eq!(looped, PathPolicyReason::SymlinkLoop);
        assert_eq!(format!("{looped:?}"), "SymlinkLoop");
        assert_eq!(format!("{looped}"), "symlink_loop");

        let failure = validate_workspace_allowed_paths(&fixture.join("missing"), &["module.py"])
            .expect_err("missing workspace should be denied");
        assert_eq!(failure, PathPolicyReason::ResolutionFailure);
        assert_eq!(format!("{failure:?}"), "ResolutionFailure");
        assert_eq!(format!("{failure}"), "path_resolution_failure");

        let workspace_text = fixture.path().display().to_string();
        for rendered in [
            format!("{escape:?} {escape}"),
            format!("{looped:?} {looped}"),
            format!("{failure:?} {failure}"),
        ] {
            assert!(!rendered.contains("secret"), "{rendered}");
            assert!(!rendered.contains(&workspace_text), "{rendered}");
        }
    }
}

#[cfg(all(test, unix))]
mod external_tests {
    use super::{
        AllowedPathEntry, AllowedPathGroup, PathPolicyReason, PathScope, ValidatedAllowedPath,
        git_repository_root, group_allowed_paths_by_repo, run_bounded_git_probe,
        validate_allowed_path_entries_with_trusted_roots,
        validate_allowed_paths_with_trusted_roots, validate_workspace_allowed_paths,
    };
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bridge-path-policy-ext-{}-{tag}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create fixture dir");
            let path = std::fs::canonicalize(&path).expect("canonicalize fixture");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn mkdir(path: &Path) {
        std::fs::create_dir_all(path).expect("create directory");
    }

    fn mkfile(path: &Path) {
        if let Some(parent) = path.parent() {
            mkdir(parent);
        }
        std::fs::write(path, b"content").expect("write file");
    }

    fn link(target: &Path, link_path: &Path) {
        std::os::unix::fs::symlink(target, link_path).expect("create symlink");
    }

    /// Initializes a real Git worktree, matching the fixture `git_repo=true`
    /// setup so repository discovery is exercised through Git.
    fn init_repo(path: &Path) {
        mkdir(path);
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(path)
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
    }

    fn rel(path: &str, scope: PathScope) -> ValidatedAllowedPath {
        ValidatedAllowedPath::WorkspaceRelative {
            path: path.to_owned(),
            scope,
        }
    }

    fn ext(path: PathBuf, scope: PathScope) -> ValidatedAllowedPath {
        ValidatedAllowedPath::External { path, scope }
    }

    fn group_for<'a>(groups: &'a [AllowedPathGroup], root: &Path) -> &'a AllowedPathGroup {
        groups
            .iter()
            .find(|group| group.root() == root)
            .expect("group must be present")
    }

    fn as_paths<'a>(roots: &[&'a Path]) -> Vec<&'a Path> {
        roots.to_vec()
    }

    /// Materializes the trusted-external layout shared by the 4.6 fixtures.
    struct Layout {
        fixture: Fixture,
        workspace: PathBuf,
        trusted_dir: PathBuf,
        trusted_repo: PathBuf,
        trusted_repo_other: PathBuf,
        trusted_plain: PathBuf,
        outside_repo: PathBuf,
        trusted_subdir: PathBuf,
    }

    impl Layout {
        fn new(tag: &str) -> Self {
            let fixture = Fixture::new(tag);
            let workspace = fixture.join("workspace");
            init_repo(&workspace);
            let trusted_dir = fixture.join("trusted");
            mkdir(&trusted_dir);
            let trusted_repo = trusted_dir.join("repo");
            init_repo(&trusted_repo);
            let trusted_repo_other = trusted_dir.join("repo-other");
            init_repo(&trusted_repo_other);
            let trusted_plain = trusted_dir.join("plain");
            mkdir(&trusted_plain);
            mkfile(&trusted_plain.join("note.txt"));
            let outside_repo = fixture.join("outside-repo");
            init_repo(&outside_repo);
            let parent_repo = fixture.join("parent-repo");
            init_repo(&parent_repo);
            let trusted_subdir = parent_repo.join("trusted-subdir");
            mkdir(&trusted_subdir);
            mkfile(&trusted_subdir.join("lib.py"));
            let escape_link = trusted_dir.join("escape");
            link(&outside_repo, &escape_link);
            mkdir(&trusted_repo.join("pkg"));
            mkfile(&trusted_repo.join("pkg/mod.py"));
            mkfile(&trusted_repo.join("lib.py"));
            mkfile(&trusted_repo.join("a.py"));
            mkfile(&trusted_repo_other.join("b.py"));
            Self {
                fixture,
                workspace,
                trusted_dir,
                trusted_repo,
                trusted_repo_other,
                trusted_plain,
                outside_repo,
                trusted_subdir,
            }
        }

        fn root(&self) -> &Path {
            self.fixture.path()
        }
    }

    /// Task 4.6 `validate_allowed_paths` corpus cases from
    /// `docs/fixtures/path-policy-cases.json`.
    #[test]
    fn reference_corpus_cases_4_6_validate() {
        let layout = Layout::new("validate-corpus");
        let ws = layout.workspace.as_path();

        let trusted = [layout.trusted_dir.as_path()];

        // validate-absolute-trusted-repo-directory
        assert_eq!(
            validate_allowed_paths_with_trusted_roots(
                ws,
                &trusted,
                &[&format!("{}/", layout.trusted_repo.display())],
            ),
            Ok(vec![ext(layout.trusted_repo.clone(), PathScope::Directory)])
        );
        // validate-absolute-trusted-repo-file
        assert_eq!(
            validate_allowed_paths_with_trusted_roots(
                ws,
                &trusted,
                &[&format!("{}/lib.py", layout.trusted_repo.display())],
            ),
            Ok(vec![ext(
                layout.trusted_repo.join("lib.py"),
                PathScope::File
            )])
        );

        let deny: [(&str, Vec<String>, Vec<PathBuf>, &str); 6] = [
            (
                "validate-absolute-outside-trusted",
                vec![format!("{}/lib.py", layout.outside_repo.display())],
                vec![layout.trusted_dir.clone()],
                "outside_trusted_roots",
            ),
            (
                "validate-absolute-repo-root-outside-trusted",
                vec![format!("{}/lib.py", layout.trusted_subdir.display())],
                vec![layout.trusted_subdir.clone()],
                "external_repo_root_outside_trusted",
            ),
            (
                "validate-absolute-symlink-escape-outside-trusted",
                vec![format!(
                    "{}/lib.py",
                    layout.trusted_dir.join("escape").display()
                )],
                vec![layout.trusted_dir.clone()],
                "outside_trusted_roots",
            ),
            (
                "validate-absolute-trusted-not-git-repository",
                vec![format!("{}/note.txt", layout.trusted_plain.display())],
                vec![layout.trusted_dir.clone()],
                "not_git_repository",
            ),
            (
                "validate-absolute-workspace-directory",
                vec![format!("{}/", layout.workspace.display())],
                vec![layout.trusted_dir.clone()],
                "absolute_workspace_path",
            ),
            (
                "validate-absolute-workspace-file",
                vec![format!("{}/module.py", layout.workspace.display())],
                Vec::new(),
                "absolute_workspace_path",
            ),
        ];
        for (id, inputs, roots, category) in deny {
            let refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
            let root_refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
            let reason = validate_allowed_paths_with_trusted_roots(ws, &root_refs, &refs)
                .expect_err("entry should be denied");
            assert_eq!(reason.as_str(), category, "{id}");
        }
    }

    /// Task 4.6 `group_allowed_paths_by_repo` corpus cases from
    /// `docs/fixtures/path-policy-cases.json`.
    #[test]
    fn reference_corpus_cases_4_6_group() {
        let layout = Layout::new("group-corpus");
        let ws = layout.workspace.as_path();

        // group-empty-list
        let groups = group_allowed_paths_by_repo(ws, &[]).expect("empty list groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].root(), ws);
        assert!(groups[0].entries().is_empty());

        // group-external-not-git-repository
        let note = format!("{}/note.txt", layout.trusted_plain.display());
        let reason = group_allowed_paths_by_repo(ws, &[&note])
            .expect_err("non-repository path must be denied");
        assert_eq!(reason.as_str(), "not_git_repository");

        // group-external-repo-root-and-child
        let repo_dir = format!("{}/", layout.trusted_repo.display());
        let repo_child = format!("{}/pkg/mod.py", layout.trusted_repo.display());
        let groups = group_allowed_paths_by_repo(ws, &[&repo_dir, &repo_child])
            .expect("external entries group");
        assert_eq!(groups[0].root(), ws);
        assert!(groups[0].entries().is_empty());
        let bucket = group_for(&groups, &layout.trusted_repo);
        assert_eq!(bucket.entries(), &[repo_dir.clone(), repo_child.clone()]);

        // group-relative-and-external
        let lib = format!("{}/lib.py", layout.trusted_repo.display());
        let groups = group_allowed_paths_by_repo(ws, &["module.py", "src/", &lib])
            .expect("mixed entries group");
        assert_eq!(groups[0].root(), ws);
        assert_eq!(groups[0].entries(), &["module.py", "src/"]);
        assert_eq!(group_for(&groups, &layout.trusted_repo).entries(), &[lib]);

        // group-two-external-repos
        let a = format!("{}/a.py", layout.trusted_repo.display());
        let b = format!("{}/b.py", layout.trusted_repo_other.display());
        let groups = group_allowed_paths_by_repo(ws, &[&a, &b]).expect("two repos group");
        assert_eq!(groups[0].root(), ws);
        assert!(groups[0].entries().is_empty());
        assert_eq!(group_for(&groups, &layout.trusted_repo).entries(), &[a]);
        assert_eq!(
            group_for(&groups, &layout.trusted_repo_other).entries(),
            &[b]
        );
    }

    /// A missing leaf resolves lexically and maps to the nearest existing
    /// ancestor's repository.
    #[test]
    fn missing_leaf_uses_existing_ancestor() {
        let layout = Layout::new("missing-leaf");
        let ws = layout.workspace.as_path();
        let trusted = as_paths(&[layout.trusted_dir.as_path()]);

        let missing = format!("{}/missing/deep.py", layout.trusted_repo.display());
        assert_eq!(
            validate_allowed_paths_with_trusted_roots(ws, &trusted, &[&missing]),
            Ok(vec![ext(
                layout.trusted_repo.join("missing/deep.py"),
                PathScope::File
            )])
        );

        let groups =
            group_allowed_paths_by_repo(ws, &[&missing]).expect("missing leaf still groups");
        assert_eq!(
            group_for(&groups, &layout.trusted_repo).entries(),
            &[missing]
        );
    }

    /// Both the repository root itself and a child inside it are accepted.
    #[test]
    fn repo_root_and_child_are_accepted() {
        let layout = Layout::new("root-child");
        let ws = layout.workspace.as_path();
        let trusted = as_paths(&[layout.trusted_dir.as_path()]);

        let root = layout.trusted_repo.display().to_string();
        let root_dir = format!("{root}/");
        assert_eq!(
            validate_allowed_paths_with_trusted_roots(ws, &trusted, &[&root]),
            Ok(vec![ext(layout.trusted_repo.clone(), PathScope::File)])
        );
        assert_eq!(
            validate_allowed_paths_with_trusted_roots(ws, &trusted, &[&root_dir]),
            Ok(vec![ext(layout.trusted_repo.clone(), PathScope::Directory)])
        );
    }

    /// A trusted root nested inside a higher repository does not widen trust.
    #[test]
    fn nested_trusted_root_does_not_widen_trust() {
        let layout = Layout::new("nested-trust");
        let ws = layout.workspace.as_path();
        let trusted = as_paths(&[layout.trusted_subdir.as_path()]);
        let input = format!("{}/lib.py", layout.trusted_subdir.display());
        let reason = validate_allowed_paths_with_trusted_roots(ws, &trusted, &[&input])
            .expect_err("nested trusted root must not widen trust");
        assert_eq!(reason, PathPolicyReason::ExternalRepoRootOutsideTrusted);
    }

    /// A symlink inside a trusted root that escapes to an outside repository is
    /// rejected before repository discovery.
    #[test]
    fn trusted_symlink_escape_is_rejected() {
        let layout = Layout::new("trusted-escape");
        let ws = layout.workspace.as_path();
        let trusted = as_paths(&[layout.trusted_dir.as_path()]);
        let input = format!("{}/lib.py", layout.trusted_dir.join("escape").display());
        let reason = validate_allowed_paths_with_trusted_roots(ws, &trusted, &[&input])
            .expect_err("symlink escape must be denied");
        assert_eq!(reason, PathPolicyReason::OutsideTrustedRoots);
    }

    /// Grouping never normalizes or validates raw entries.
    #[test]
    fn grouping_preserves_raw_entries() {
        let layout = Layout::new("group-raw");
        let ws = layout.workspace.as_path();
        let raw = ["a//b", "./c", "d/"];
        let groups = group_allowed_paths_by_repo(ws, &raw).expect("relative entries group");
        assert_eq!(groups[0].root(), ws);
        assert_eq!(groups[0].entries(), &raw);
    }

    /// Relative entries keep the workspace-layer semantics and normalized
    /// scopes; absolute entries become canonical external scopes.
    #[test]
    fn relative_entries_match_workspace_api() {
        let layout = Layout::new("relative-regression");
        let ws = layout.workspace.as_path();
        mkdir(&ws.join("src"));
        let trusted = as_paths(&[layout.trusted_dir.as_path()]);

        let inputs = ["module.py", "src/", "missing/deep.py"];
        let expected = validate_workspace_allowed_paths(ws, &inputs).expect("relative entries");
        let validated = validate_allowed_paths_with_trusted_roots(ws, &trusted, &inputs)
            .expect("relative entries through trusted API");
        let rendered: Vec<String> = validated
            .iter()
            .map(ValidatedAllowedPath::to_scope_string)
            .collect();
        assert_eq!(rendered, expected);
        assert!(validated.iter().all(|entry| !entry.is_external()));
        assert_eq!(validated[0].scope(), PathScope::File);
        assert_eq!(validated[1].scope(), PathScope::Directory);
        assert_eq!(validated[0].path(), Path::new("module.py"));
    }

    /// Non-absolute lexical rejections and workspace escapes are preserved.
    #[test]
    fn lexical_and_escape_rejections_are_preserved() {
        let layout = Layout::new("lexical-regression");
        let ws = layout.workspace.as_path();
        let trusted = as_paths(&[layout.trusted_dir.as_path()]);
        link(&layout.outside_repo, &ws.join("escape"));

        for (input, expected) in [
            ("", PathPolicyReason::InvalidAllowedPathsEntry),
            ("src\\module.py", PathPolicyReason::BackslashInPath),
            ("../outside.py", PathPolicyReason::ParentTraversal),
            (".", PathPolicyReason::EmptyOrDotPath),
        ] {
            let reason = validate_allowed_paths_with_trusted_roots(ws, &trusted, &[input])
                .expect_err("entry should be denied");
            assert_eq!(reason, expected, "{input:?}");
        }

        let reason = validate_allowed_paths_with_trusted_roots(ws, &trusted, &["escape"])
            .expect_err("relative symlink escape should be denied");
        assert_eq!(reason, PathPolicyReason::WorkspaceEscape);
    }

    /// The boundary variant rejects non-string entries fail closed.
    #[test]
    fn boundary_rejects_non_text() {
        let layout = Layout::new("boundary");
        let ws = layout.workspace.as_path();
        let trusted = as_paths(&[layout.trusted_dir.as_path()]);
        mkdir(&ws.join("src"));

        assert_eq!(
            validate_allowed_path_entries_with_trusted_roots(
                ws,
                &trusted,
                &[AllowedPathEntry::Text("src/"), AllowedPathEntry::NonText],
            ),
            Err(PathPolicyReason::InvalidAllowedPathsEntry)
        );
        assert_eq!(
            validate_allowed_path_entries_with_trusted_roots(
                ws,
                &trusted,
                &[AllowedPathEntry::Text("src/")],
            ),
            Ok(vec![rel("src", PathScope::Directory)])
        );
        assert_eq!(
            validate_allowed_path_entries_with_trusted_roots(ws, &trusted, &[]),
            Ok(Vec::new())
        );
    }

    /// A missing trusted root fails closed instead of silently widening scope.
    #[test]
    fn missing_trusted_root_fails_closed() {
        let layout = Layout::new("missing-trusted-root");
        let ws = layout.workspace.as_path();
        let input = format!("{}/lib.py", layout.trusted_repo.display());
        let reason = validate_allowed_paths_with_trusted_roots(
            ws,
            &[&layout.root().join("does-not-exist")],
            &[&input],
        )
        .expect_err("missing trusted root must be denied");
        assert_eq!(reason, PathPolicyReason::ResolutionFailure);
    }

    /// Reason categories are stable and error rendering leaks nothing.
    #[test]
    fn errors_do_not_reveal_paths_roots_or_git_output() {
        let layout = Layout::new("super-secret");
        let ws = layout.workspace.as_path();

        let outside = format!("{}/lib.py", layout.outside_repo.display());
        let plain = format!("{}/note.txt", layout.trusted_plain.display());
        let nested = format!("{}/lib.py", layout.trusted_subdir.display());
        let cases: [(&str, &str, Vec<&Path>); 3] = [
            (
                "outside_trusted_roots",
                &outside,
                vec![layout.trusted_dir.as_path()],
            ),
            (
                "not_git_repository",
                &plain,
                vec![layout.trusted_dir.as_path()],
            ),
            (
                "external_repo_root_outside_trusted",
                &nested,
                vec![layout.trusted_subdir.as_path()],
            ),
        ];
        for (category, input, roots) in cases {
            let reason = validate_allowed_paths_with_trusted_roots(ws, &roots, &[input])
                .expect_err("entry should be denied");
            assert_eq!(reason.as_str(), category);
            assert_eq!(reason.to_string(), category);
            let rendered = format!("{reason:?} {reason}");
            assert!(!rendered.contains("secret"), "{rendered}");
            assert!(!rendered.contains(input), "{rendered}");
            assert!(!rendered.contains(&ws.display().to_string()), "{rendered}");
        }

        for (reason, display, debug) in [
            (
                PathPolicyReason::OutsideTrustedRoots,
                "outside_trusted_roots",
                "OutsideTrustedRoots",
            ),
            (
                PathPolicyReason::NotGitRepository,
                "not_git_repository",
                "NotGitRepository",
            ),
            (
                PathPolicyReason::ExternalRepoRootOutsideTrusted,
                "external_repo_root_outside_trusted",
                "ExternalRepoRootOutsideTrusted",
            ),
        ] {
            assert_eq!(reason.as_str(), display);
            assert_eq!(reason.to_string(), display);
            assert_eq!(format!("{reason:?}"), debug);
        }
    }

    /// Git is only probed for absolute entries; a relative-only call on a
    /// workspace works without any trusted root.
    #[test]
    fn relative_only_call_needs_no_trusted_roots() {
        let layout = Layout::new("relative-only");
        let ws = layout.workspace.as_path();
        assert_eq!(
            validate_allowed_paths_with_trusted_roots(ws, &[], &["module.py"]),
            Ok(vec![rel("module.py", PathScope::File)])
        );
    }

    /// A probe that never exits is killed and reaped within the bound, so a
    /// hung or replaced `git` cannot block validation forever.
    #[test]
    fn hanging_git_probe_is_killed_within_bound() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("hanging-git");
        let probe_dir = fixture.join("probe");
        mkdir(&probe_dir);

        let pid_file = fixture.join("git.pid");
        let fake_git = fixture.join("fake-git");
        std::fs::write(
            &fake_git,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .expect("write fake git");
        let mut permissions = std::fs::metadata(&fake_git)
            .expect("stat fake git")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_git, permissions).expect("chmod fake git");

        let timeout = Duration::from_millis(500);
        let started = Instant::now();
        let result = run_bounded_git_probe(fake_git.as_os_str(), &probe_dir, timeout);
        let elapsed = started.elapsed();

        assert_eq!(result, Err(PathPolicyReason::ResolutionFailure));
        assert!(
            elapsed < timeout + Duration::from_secs(3),
            "probe took {elapsed:?}"
        );

        let pid_text = std::fs::read_to_string(&pid_file).expect("fake git records its pid");
        let pid: u32 = pid_text.trim().parse().expect("pid parses");
        let proc_path = Path::new("/proc").join(pid.to_string());
        let mut gone = false;
        for _ in 0..100 {
            if !proc_path.exists() {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            gone,
            "probe process {pid} should have been killed and reaped"
        );
    }

    /// A repository root with a non-UTF-8 component is discovered instead of
    /// being misclassified as a non-repository.
    #[test]
    fn non_utf8_git_repo_root_is_discovered() {
        use std::os::unix::ffi::OsStrExt;

        let fixture = Fixture::new("non-utf8");
        let repo = fixture.path().join(OsStr::from_bytes(b"repo-\xff\xfe"));
        init_repo(&repo);
        mkfile(&repo.join("lib.py"));

        let root = git_repository_root(&repo)
            .expect("probe runs")
            .expect("repository root is discovered");
        assert_eq!(
            root,
            std::fs::canonicalize(&repo).expect("canonicalize repo")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AllowedPathEntry, PathPolicyReason, crate_name, validate_allowed_path_entries,
        validate_allowed_paths,
    };

    fn allowed(paths: &[&str]) -> Vec<String> {
        validate_allowed_paths(paths).expect("paths should be allowed")
    }

    fn denied(path: &str) -> PathPolicyReason {
        validate_allowed_paths(&[path]).expect_err("path should be denied")
    }

    #[test]
    fn crate_is_wired() {
        assert_eq!(crate_name(), "bridge-path-policy");
    }

    #[test]
    fn empty_list_is_allowed() {
        assert_eq!(allowed(&[]), Vec::<String>::new());
        assert_eq!(validate_allowed_path_entries(&[]), Ok(Vec::<String>::new()));
    }

    #[test]
    fn relative_file_and_directory_scopes_are_preserved() {
        assert_eq!(allowed(&["module.py"]), vec!["module.py".to_owned()]);
        assert_eq!(allowed(&["src/"]), vec!["src/".to_owned()]);
        assert_eq!(
            allowed(&["module.py", "src/"]),
            vec!["module.py".to_owned(), "src/".to_owned()]
        );
    }

    #[test]
    fn file_and_directory_distinction_is_preserved() {
        assert_eq!(allowed(&["src"]), vec!["src".to_owned()]);
        assert_eq!(allowed(&["src/"]), vec!["src/".to_owned()]);
        assert_ne!(allowed(&["src"]), allowed(&["src/"]));
    }

    #[test]
    fn missing_paths_are_handled_lexically() {
        assert_eq!(allowed(&["missing/dir/"]), vec!["missing/dir/".to_owned()]);
        assert_eq!(
            allowed(&["missing/deep.py"]),
            vec!["missing/deep.py".to_owned()]
        );
    }

    #[test]
    fn normalization_is_deterministic() {
        assert_eq!(
            allowed(&["a//b", "./c", "d/./", "./e/"]),
            vec![
                "a/b".to_owned(),
                "c".to_owned(),
                "d/".to_owned(),
                "e/".to_owned(),
            ]
        );
    }

    #[test]
    fn empty_entry_is_rejected() {
        assert_eq!(denied(""), PathPolicyReason::InvalidAllowedPathsEntry);
    }

    #[test]
    fn non_string_entry_is_rejected_at_the_boundary() {
        assert_eq!(
            validate_allowed_path_entries(&[AllowedPathEntry::NonText]),
            Err(PathPolicyReason::InvalidAllowedPathsEntry)
        );
        assert_eq!(
            validate_allowed_path_entries(&[
                AllowedPathEntry::Text("module.py"),
                AllowedPathEntry::NonText,
            ]),
            Err(PathPolicyReason::InvalidAllowedPathsEntry)
        );
        assert_eq!(
            validate_allowed_path_entries(&[
                AllowedPathEntry::Text("module.py"),
                AllowedPathEntry::Text("src/"),
            ]),
            Ok(vec!["module.py".to_owned(), "src/".to_owned()])
        );
    }

    #[test]
    fn backslash_entries_are_rejected() {
        for path in ["src\\module.py", "\\module.py", "src/module\\name.py"] {
            assert_eq!(denied(path), PathPolicyReason::BackslashInPath);
        }
    }

    #[test]
    fn parent_traversal_is_rejected_before_normalization() {
        for path in ["../outside.py", "a/../b", "..", "../", "a/..", "a/../"] {
            assert_eq!(denied(path), PathPolicyReason::ParentTraversal);
        }
    }

    #[test]
    fn empty_or_dot_paths_are_rejected() {
        for path in [".", "./", "/", "//", "///", ".//"] {
            assert_eq!(denied(path), PathPolicyReason::EmptyOrDotPath);
        }
    }

    #[test]
    fn absolute_paths_are_rejected() {
        for path in ["/etc/passwd", "/tmp/workspace/module.py", "//a", "///a"] {
            assert_eq!(denied(path), PathPolicyReason::AbsolutePath);
        }
    }

    #[test]
    fn reason_strings_match_reference_categories() {
        for (reason, expected) in [
            (
                PathPolicyReason::InvalidAllowedPathsEntry,
                "invalid_allowed_paths_entry",
            ),
            (PathPolicyReason::BackslashInPath, "backslash_in_path"),
            (PathPolicyReason::ParentTraversal, "parent_traversal"),
            (PathPolicyReason::EmptyOrDotPath, "empty_or_dot_path"),
            (PathPolicyReason::AbsolutePath, "absolute_workspace_path"),
            (PathPolicyReason::WorkspaceEscape, "workspace_escape"),
            (PathPolicyReason::SymlinkLoop, "symlink_loop"),
            (
                PathPolicyReason::ResolutionFailure,
                "path_resolution_failure",
            ),
            (
                PathPolicyReason::OutsideTrustedRoots,
                "outside_trusted_roots",
            ),
            (PathPolicyReason::NotGitRepository, "not_git_repository"),
            (
                PathPolicyReason::ExternalRepoRootOutsideTrusted,
                "external_repo_root_outside_trusted",
            ),
        ] {
            assert_eq!(reason.as_str(), expected);
            assert_eq!(reason.to_string(), expected);
        }
    }

    #[test]
    fn errors_do_not_reveal_paths_or_secrets() {
        for path in [
            "super-secret-dir/../secret.txt",
            "secret\\name.py",
            "",
            "/etc/super-secret-passwd",
            ".",
        ] {
            let reason = validate_allowed_paths(&[path]).expect_err("path should be denied");
            let rendered = format!("{reason:?} {reason}");
            assert!(!rendered.contains("secret"), "{rendered}");
            assert!(!rendered.contains("passwd"), "{rendered}");
            if !path.is_empty() {
                assert!(!rendered.contains(path), "{rendered}");
            }
        }
    }

    /// Task 4.4 subset of `docs/fixtures/path-policy-cases.json`: the
    /// workspace-relative lexical branch of `validate_allowed_paths` plus the
    /// absolute-workspace rejection that the workspace-relative policy owns.
    /// Symlink confinement (4.5), trusted external roots and absolute external
    /// paths (4.6), and scope/snapshot operations (4.7-4.9) are out of scope.
    #[test]
    fn reference_corpus_cases_4_4() {
        let allow: [(&str, &[&str], &[&str]); 5] = [
            ("validate-empty-list", &[], &[]),
            ("validate-relative-file", &["module.py"], &["module.py"]),
            ("validate-relative-directory", &["src/"], &["src/"]),
            (
                "validate-relative-missing-file",
                &["missing/deep.py"],
                &["missing/deep.py"],
            ),
            (
                "validate-relative-missing-directory",
                &["missing/dir/"],
                &["missing/dir/"],
            ),
        ];
        for (id, input, expected) in allow {
            let expected: Vec<String> = expected.iter().map(|path| (*path).to_owned()).collect();
            assert_eq!(
                validate_allowed_paths(input),
                Ok(expected),
                "{id} should be allowed"
            );
        }

        let deny: [(&str, &[&str], &str); 6] = [
            ("validate-empty-entry", &[""], "invalid_allowed_paths_entry"),
            (
                "validate-backslash-entry",
                &["src\\module.py", "\\module.py"],
                "backslash_in_path",
            ),
            (
                "validate-parent-traversal",
                &["../outside.py", "a/../b"],
                "parent_traversal",
            ),
            (
                "validate-dot-path",
                &[".", "./", "/", "//"],
                "empty_or_dot_path",
            ),
            (
                "validate-absolute-workspace-file",
                &["/tmp/workspace/module.py"],
                "absolute_workspace_path",
            ),
            (
                "validate-absolute-workspace-directory",
                &["/tmp/workspace/"],
                "absolute_workspace_path",
            ),
        ];
        for (id, inputs, category) in deny {
            for input in inputs.iter().copied() {
                let reason = validate_allowed_paths(&[input]).expect_err("entry should be denied");
                assert_eq!(reason.as_str(), category, "{id} for {input:?}");
            }
        }

        // `validate-entry-non-string` (variants 42/null/true) is only
        // representable at the boundary API, which rejects every non-string
        // entry fail closed.
        assert_eq!(
            validate_allowed_path_entries(&[AllowedPathEntry::NonText]),
            Err(PathPolicyReason::InvalidAllowedPathsEntry)
        );
    }
}
