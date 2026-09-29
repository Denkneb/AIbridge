//! Snapshot comparison and repository-policy violations (task 4.9).

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::worktree::surrogateescape_key;
use crate::{
    CommitId, GitError, MultiRepositorySnapshot, RepositorySnapshot, head, index_fingerprint,
    is_repository, os_from_bytes, run_checked_os, worktree_manifest,
};

/// A stable Git-policy violation reported while comparing a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitPolicyViolation {
    HistoryRewritten,
    HeadChanged,
    IndexChanged,
    NotRepository,
    ExternalRepositoryMissing,
}

impl GitPolicyViolation {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HistoryRewritten => "history_rewritten",
            Self::HeadChanged => "head_changed",
            Self::IndexChanged => "index_changed",
            Self::NotRepository => "not_a_git_repo",
            Self::ExternalRepositoryMissing => "external_repo_missing",
        }
    }
}

/// Comparison result for one repository. All paths are repository-relative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryComparison {
    root: PathBuf,
    baseline_dirty_paths: Vec<OsString>,
    changed_paths: Vec<OsString>,
    committed_paths: Vec<OsString>,
    scope_violations: Vec<OsString>,
    git_policy_violations: Vec<GitPolicyViolation>,
    head_before: Option<CommitId>,
    head_after: Option<CommitId>,
}

impl RepositoryComparison {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
    #[must_use]
    pub fn baseline_dirty_paths(&self) -> &[OsString] {
        &self.baseline_dirty_paths
    }
    #[must_use]
    pub fn changed_paths(&self) -> &[OsString] {
        &self.changed_paths
    }
    #[must_use]
    pub fn committed_paths(&self) -> &[OsString] {
        &self.committed_paths
    }
    #[must_use]
    pub fn scope_violations(&self) -> &[OsString] {
        &self.scope_violations
    }
    #[must_use]
    pub fn git_policy_violations(&self) -> &[GitPolicyViolation] {
        &self.git_policy_violations
    }
    #[must_use]
    pub fn head_before(&self) -> Option<&CommitId> {
        self.head_before.as_ref()
    }
    #[must_use]
    pub fn head_after(&self) -> Option<&CommitId> {
        self.head_after.as_ref()
    }
}

/// Deterministically ordered comparison of all repositories in a baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiRepositoryComparison {
    repositories: Vec<RepositoryComparison>,
}

impl MultiRepositoryComparison {
    #[must_use]
    pub fn repositories(&self) -> &[RepositoryComparison] {
        &self.repositories
    }
    #[must_use]
    pub fn main(&self) -> &RepositoryComparison {
        &self.repositories[0]
    }
}

/// Compares one repository with a baseline snapshot.
///
/// `external` selects external-repository missing semantics and qualifies scope
/// candidates with the canonical repository root before matching absolute
/// allowed paths. Returned paths remain repository-relative.
pub fn compare_repository_snapshot(
    root: &Path,
    baseline: &RepositorySnapshot,
    allowed_paths: &[String],
    allow_commit: bool,
    external: bool,
) -> Result<RepositoryComparison, GitError> {
    let baseline_dirty_paths = baseline.dirty_paths().to_vec();
    if !root.is_dir() || !is_repository(root)? {
        return Ok(RepositoryComparison {
            root: root.to_path_buf(),
            baseline_dirty_paths,
            changed_paths: Vec::new(),
            committed_paths: Vec::new(),
            scope_violations: Vec::new(),
            git_policy_violations: vec![if external {
                GitPolicyViolation::ExternalRepositoryMissing
            } else {
                GitPolicyViolation::NotRepository
            }],
            head_before: baseline.head().cloned(),
            head_after: None,
        });
    }

    let current_head = head(root)?;
    let changed_paths = changed_paths(root, baseline)?;
    let committed_paths = committed_paths(root, baseline.head(), current_head.as_ref())?;
    let mut scoped = changed_paths.clone();
    scoped.extend(committed_paths.iter().cloned());
    sort_dedup(&mut scoped);
    let scope_violations = scoped
        .into_iter()
        .filter(|path| !is_allowed(root, path, allowed_paths, external))
        .collect();

    let mut git_policy_violations = Vec::new();
    if !history_descends_from(root, baseline.head(), current_head.as_ref())? {
        git_policy_violations.push(GitPolicyViolation::HistoryRewritten);
    }
    if !allow_commit {
        if current_head.as_ref() != baseline.head() {
            git_policy_violations.push(GitPolicyViolation::HeadChanged);
        }
        if &index_fingerprint(root)? != baseline.index_fingerprint() {
            git_policy_violations.push(GitPolicyViolation::IndexChanged);
        }
    }

    Ok(RepositoryComparison {
        root: root.to_path_buf(),
        baseline_dirty_paths,
        changed_paths,
        committed_paths,
        scope_violations,
        git_policy_violations,
        head_before: baseline.head().cloned(),
        head_after: current_head,
    })
}

/// Compares every repository in a multi-repository baseline in baseline order.
pub fn compare_multi_repository_snapshot(
    baseline: &MultiRepositorySnapshot,
    allow_commit: bool,
) -> Result<MultiRepositoryComparison, GitError> {
    let mut repositories = Vec::with_capacity(baseline.len());
    for (index, repository) in baseline.repositories().iter().enumerate() {
        repositories.push(compare_repository_snapshot(
            repository.root(),
            repository.snapshot(),
            repository.allowed_paths(),
            allow_commit,
            index != 0,
        )?);
    }
    Ok(MultiRepositoryComparison { repositories })
}

fn changed_paths(root: &Path, baseline: &RepositorySnapshot) -> Result<Vec<OsString>, GitError> {
    let current = worktree_manifest(root)?;
    let mut paths = Vec::new();
    for entry in current.entries() {
        if baseline.manifest().digest(entry.path()) != Some(entry.digest()) {
            paths.push(entry.path().to_os_string());
        }
    }
    for entry in baseline.manifest().entries() {
        if current.digest(entry.path()).is_none() {
            paths.push(entry.path().to_os_string());
        }
    }
    sort_dedup(&mut paths);
    Ok(paths)
}

fn committed_paths(
    root: &Path,
    base: Option<&CommitId>,
    current: Option<&CommitId>,
) -> Result<Vec<OsString>, GitError> {
    let Some(current) = current else {
        return Ok(Vec::new());
    };
    if Some(current) == base {
        return Ok(Vec::new());
    }
    let output = if let Some(base) = base {
        run_checked_os(
            root,
            &[
                OsStr::new("diff"),
                OsStr::new("--name-only"),
                OsStr::new("-z"),
                OsStr::new("--no-ext-diff"),
                OsStr::new(base.as_str()),
                OsStr::new(current.as_str()),
            ],
        )?
    } else {
        run_checked_os(
            root,
            &[
                OsStr::new("diff-tree"),
                OsStr::new("--root"),
                OsStr::new("--no-commit-id"),
                OsStr::new("--name-only"),
                OsStr::new("-z"),
                OsStr::new("-r"),
                OsStr::new(current.as_str()),
            ],
        )?
    };
    parse_null_paths(&output)
}

fn history_descends_from(
    root: &Path,
    base: Option<&CommitId>,
    current: Option<&CommitId>,
) -> Result<bool, GitError> {
    let (Some(base), Some(current)) = (base, current) else {
        return Ok(true);
    };
    if base == current {
        return Ok(true);
    }
    let args = [
        OsStr::new("merge-base"),
        OsStr::new("--is-ancestor"),
        OsStr::new(base.as_str()),
        OsStr::new(current.as_str()),
    ];
    let output = crate::runner::run_bounded(
        OsStr::new(super::GIT_PROGRAM),
        root,
        &args,
        super::GIT_TIMEOUT,
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GitError::CommandFailed),
    }
}

fn parse_null_paths(bytes: &[u8]) -> Result<Vec<OsString>, GitError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let body = bytes.strip_suffix(b"\0").ok_or(GitError::MalformedOutput)?;
    let mut paths = Vec::new();
    for field in body.split(|byte| *byte == 0) {
        if field.is_empty() {
            return Err(GitError::MalformedOutput);
        }
        paths.push(os_from_bytes(field));
    }
    sort_dedup(&mut paths);
    Ok(paths)
}

fn sort_dedup(paths: &mut Vec<OsString>) {
    paths.sort_by_key(|path| surrogateescape_key(path));
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

#[cfg(unix)]
fn candidate_bytes(root: &Path, path: &OsStr, external: bool) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    if !external {
        return path.as_bytes().to_vec();
    }
    let mut candidate = root.as_os_str().as_bytes().to_vec();
    candidate.push(b'/');
    candidate.extend_from_slice(path.as_bytes());
    candidate
}

#[cfg(not(unix))]
fn candidate_bytes(root: &Path, path: &OsStr, external: bool) -> Vec<u8> {
    if external {
        root.join(path).to_string_lossy().as_bytes().to_vec()
    } else {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

fn is_allowed(root: &Path, path: &OsStr, allowed_paths: &[String], external: bool) -> bool {
    let candidate = candidate_bytes(root, path, external);
    allowed_paths.iter().any(|allowed| {
        let bytes = allowed.as_bytes();
        if let Some(directory) = bytes.strip_suffix(b"/") {
            candidate.len() > directory.len() && candidate.starts_with(bytes)
        } else {
            candidate == bytes
        }
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        GitPolicyViolation, compare_multi_repository_snapshot, compare_repository_snapshot,
    };
    use crate::{GitError, take_multi_repository_snapshot, take_snapshot};
    use bridge_path_policy::group_allowed_paths_by_repo;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bridge-git-comparison-{}-{tag}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
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
        assert!(output.status.success(), "git {args:?} failed");
    }

    fn init(dir: &Path) {
        std::fs::create_dir_all(dir).expect("create repo");
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "tests@example.invalid"]);
        git(dir, &["config", "user.name", "Bridge Tests"]);
        write(dir, "tracked.txt", "base\n");
        git(dir, &["add", "tracked.txt"]);
        git(dir, &["commit", "-q", "-m", "base"]);
    }

    fn write(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, content).expect("write fixture");
    }

    fn names(paths: &[OsString]) -> Vec<String> {
        paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn clean_repository_has_no_changes_or_violations() {
        let dir = TempDir::new("clean");
        init(dir.path());
        let baseline = take_snapshot(dir.path()).expect("baseline");
        let result = compare_repository_snapshot(
            dir.path(),
            &baseline,
            &["tracked.txt".into()],
            false,
            false,
        )
        .expect("comparison");

        assert!(result.changed_paths().is_empty());
        assert!(result.committed_paths().is_empty());
        assert!(result.scope_violations().is_empty());
        assert!(result.git_policy_violations().is_empty());
        assert_eq!(result.head_before(), result.head_after());
    }

    #[test]
    fn worktree_changes_are_compared_to_exact_file_and_directory_scopes() {
        let dir = TempDir::new("scope");
        init(dir.path());
        let baseline = take_snapshot(dir.path()).expect("baseline");
        write(dir.path(), "allowed/in.txt", "allowed\n");
        write(dir.path(), "outside.txt", "outside\n");

        let result =
            compare_repository_snapshot(dir.path(), &baseline, &["allowed/".into()], false, false)
                .expect("comparison");

        assert_eq!(
            names(result.changed_paths()),
            ["allowed/in.txt", "outside.txt"]
        );
        assert_eq!(names(result.scope_violations()), ["outside.txt"]);
        assert!(result.committed_paths().is_empty());
    }

    #[test]
    fn commit_reports_committed_paths_and_head_and_index_policy() {
        let dir = TempDir::new("commit");
        init(dir.path());
        let baseline = take_snapshot(dir.path()).expect("baseline");
        write(dir.path(), "tracked.txt", "changed\n");
        git(dir.path(), &["add", "tracked.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "change"]);

        let result = compare_repository_snapshot(
            dir.path(),
            &baseline,
            &["tracked.txt".into()],
            false,
            false,
        )
        .expect("comparison");

        assert_eq!(names(result.changed_paths()), ["tracked.txt"]);
        assert_eq!(names(result.committed_paths()), ["tracked.txt"]);
        assert_eq!(
            result.git_policy_violations(),
            [
                GitPolicyViolation::HeadChanged,
                GitPolicyViolation::IndexChanged
            ]
        );
    }

    #[test]
    fn allow_commit_suppresses_head_and_index_but_not_history_rewrite() {
        let dir = TempDir::new("rewrite");
        init(dir.path());
        let baseline = take_snapshot(dir.path()).expect("baseline");
        write(dir.path(), "tracked.txt", "amended\n");
        git(dir.path(), &["add", "tracked.txt"]);
        git(dir.path(), &["commit", "-q", "--amend", "-m", "amended"]);

        let result = compare_repository_snapshot(
            dir.path(),
            &baseline,
            &["tracked.txt".into()],
            true,
            false,
        )
        .expect("comparison");
        assert_eq!(
            result.git_policy_violations(),
            [GitPolicyViolation::HistoryRewritten]
        );
    }

    #[test]
    fn missing_external_repository_is_a_typed_policy_violation() {
        let dir = TempDir::new("missing");
        init(dir.path());
        let baseline = take_snapshot(dir.path()).expect("baseline");
        std::fs::remove_dir_all(dir.path()).expect("remove repository");

        let result = compare_repository_snapshot(dir.path(), &baseline, &[], false, true)
            .expect("comparison");
        assert_eq!(
            result.git_policy_violations(),
            [GitPolicyViolation::ExternalRepositoryMissing]
        );
        assert_eq!(result.head_after(), None);
    }

    #[test]
    fn multi_repository_comparison_keeps_roots_and_scopes_independent() {
        let parent = TempDir::new("multi");
        let main = parent.path().join("main");
        let external = parent.path().join("external");
        init(&main);
        init(&external);
        let main = std::fs::canonicalize(main).expect("canonical main");
        let external = std::fs::canonicalize(external).expect("canonical external");
        let allowed = [
            "main.txt".to_owned(),
            format!("{}/external.txt", external.display()),
        ];
        let allowed_refs: Vec<&str> = allowed.iter().map(String::as_str).collect();
        let groups = group_allowed_paths_by_repo(&main, &allowed_refs).expect("groups");
        let baseline = take_multi_repository_snapshot(&main, &groups).expect("baseline");
        write(&main, "main.txt", "main\n");
        write(&external, "external.txt", "external\n");

        let result = compare_multi_repository_snapshot(&baseline, false).expect("comparison");
        assert_eq!(result.repositories().len(), 2);
        assert_eq!(result.repositories()[0].root(), main);
        assert_eq!(result.repositories()[1].root(), external);
        assert_eq!(
            names(result.repositories()[0].changed_paths()),
            ["main.txt"]
        );
        assert_eq!(
            names(result.repositories()[1].changed_paths()),
            ["external.txt"]
        );
        assert!(result.repositories()[0].scope_violations().is_empty());
        assert!(result.repositories()[1].scope_violations().is_empty());
    }

    #[test]
    fn comparison_errors_remain_payload_free() {
        assert_eq!(format!("{:?}", GitError::CommandFailed), "CommandFailed");
        assert_eq!(GitError::CommandFailed.to_string(), "command_failed");
    }
}
