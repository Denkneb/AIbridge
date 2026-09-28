//! Minimal path-policy primitives for workspace-relative `allowed_paths`
//! (tasks 4.4 and 4.5).
//!
//! This crate reproduces the workspace-relative branch of the reference Python
//! `git_snapshot.validate_allowed_paths`
//! (`/home/denis/Python/agent_bridge/src/agent_bridge/git_snapshot.py`). It
//! offers two layers:
//!
//! - a purely lexical layer ([`validate_allowed_paths`],
//!   [`validate_allowed_path_entries`]) that never touches the filesystem, so a
//!   missing file or directory behaves exactly like an existing one (4.4);
//! - a filesystem-aware layer ([`validate_workspace_allowed_paths`],
//!   [`validate_workspace_allowed_path_entries`]) that canonicalizes the
//!   workspace and resolves existing symlink components plus the missing tail
//!   like Python `Path.resolve(strict=False)`, rejecting any entry whose
//!   resolved target escapes the canonical workspace (4.5).
//!
//! Deliberately out of scope for this crate (later tasks):
//!
//! - trusted external directories, absolute external paths and Git repository
//!   discovery/grouping (4.6);
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
//! Absolute entries are rejected lexically with
//! [`PathPolicyReason::AbsolutePath`] (the corpus `absolute_workspace_path`
//! category): the workspace-relative policy requires a relative path. The
//! trusted-external-root branch that can accept some absolute paths belongs to
//! task 4.6 and is intentionally absent here.
//!
//! The filesystem-aware layer applies the lexical rules first and then, for
//! each surviving relative entry, resolves the canonical workspace joined with
//! the lexically normalized entry. Existing symlink components and the missing
//! tail are handled like Python `Path.resolve(strict=False)`: existing links
//! are followed, `..` in a link target is folded, and a component that does not
//! exist terminates resolution while the remaining components are joined
//! lexically. A resolved target that is not the canonical workspace or a
//! descendant of it is rejected as
//! [`PathPolicyReason::WorkspaceEscape`] (`workspace_escape`), covering both
//! existing and dangling symlink escapes. An accepted entry keeps its
//! normalized original workspace-relative scope (never the resolved target)
//! with the file/directory trailing slash preserved.
//!
//! As a documented fail-closed extension over Python, symlink loops and
//! filesystem/canonicalization failures (workspace canonicalization, non-
//! `NotFound` metadata errors such as a non-directory component, unreadable
//! links) fail closed as [`PathPolicyReason::SymlinkLoop`] or
//! [`PathPolicyReason::ResolutionFailure`] instead of silently widening scope.
//!
//! Errors ([`PathPolicyReason`]) carry no payload, so a rejected entry never
//! leaks the original path, workspace, symlink target or OS error text through
//! `Debug`/`Display`.

use std::collections::VecDeque;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};

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

/// Validates and normalizes one non-empty string entry.
///
/// The reference order is preserved: empty string, backslash, `..`, then the
/// empty/dot reduction and finally the absolute check. Normalization drops `.`
/// and empty components and appends a trailing `/` when the raw entry ended
/// with one, so file and directory scopes stay distinguishable.
fn classify_entry(raw: &str) -> Result<String, PathPolicyReason> {
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
    if raw.starts_with('/') {
        return Err(PathPolicyReason::AbsolutePath);
    }
    if is_dir {
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

/// Resolves one lexically normalized relative entry and checks confinement.
///
/// The walk mirrors Python `Path.resolve(strict=False)`: existing symlink
/// components are followed (absolute targets restart at the filesystem root,
/// relative targets resolve against the link's parent, and `..`/`.` inside a
/// target are folded), a missing component ends resolution while the remaining
/// components are joined lexically, and the final path must equal the canonical
/// workspace or be a descendant of it. No path, target or OS error text is
/// carried in the returned error.
fn confine_to_workspace(workspace: &Path, relative: &str) -> Result<(), PathPolicyReason> {
    let mut resolved = workspace.to_path_buf();
    let mut queue: VecDeque<OsString> = relative
        .split('/')
        .filter(|part| !part.is_empty())
        .map(OsString::from)
        .collect();
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
