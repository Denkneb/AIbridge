//! Minimal lexical path-policy primitives for workspace-relative
//! `allowed_paths` (task 4.4).
//!
//! This crate reproduces only the workspace-relative, lexical validation and
//! normalization branch of the reference Python
//! `git_snapshot.validate_allowed_paths`
//! (`/home/denis/Python/agent_bridge/src/agent_bridge/git_snapshot.py`). It
//! never touches the filesystem: entries are classified and normalized purely
//! from their text, so a missing file or directory behaves exactly like an
//! existing one.
//!
//! Deliberately out of scope for this crate (later tasks):
//!
//! - symlink resolution and confinement (4.5);
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
//! Errors ([`PathPolicyReason`]) carry no payload, so a rejected entry never
//! leaks the original path or any secrets through `Debug`/`Display`.

use std::error::Error;
use std::fmt;

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
