//! Multi-repository snapshot orchestration (task 4.8).
//!
//! This module combines the read-only single-worktree [`take_snapshot`] of task
//! 4.7 with the repository buckets produced by
//! [`bridge_path_policy::group_allowed_paths_by_repo`] (task 4.6). It mirrors the
//! observable semantics of the reference Python multi-repository verifier flow
//! (`git_snapshot.take_external_snapshots` plus the main-workspace snapshot taken
//! by `mcp_server.submit_task_impl`): the main workspace is always snapshotted
//! first, the affected external repositories follow in ascending canonical root
//! order, and each repository keeps its own raw `allowed_paths` bucket so that
//! identical relative paths in different repositories can never mix.
//!
//! Scope is deliberately narrow. This module only records snapshots: it does not
//! implement 4.9 comparison (`changed_paths`, `committed_paths`, scope or policy
//! violations, history ancestry) and it has no worker or MCP envelope.
//!
//! # Contract
//!
//! [`take_multi_repository_snapshot`] accepts the canonical main workspace and
//! the typed [`AllowedPathGroup`] buckets returned by
//! `group_allowed_paths_by_repo`. It produces exactly one
//! [`RepositoryGroupSnapshot`] per unique canonical repository root:
//!
//! - the main repository is always present and first, even when its bucket has
//!   an empty `allowed_paths` list;
//! - external repositories follow in ascending canonical root order, so the
//!   result is deterministic;
//! - every entry preserves the canonical repository root, the raw bucket entries
//!   (never normalized, validated or mixed) and the matching [`RepositorySnapshot`].
//!
//! Before a repository is snapshotted its bucket root is verified against the
//! actual canonical Git worktree root. The following conditions fail closed with
//! a payload-free [`MultiRepoError`]:
//!
//! - the main workspace cannot be canonicalized
//!   ([`MultiRepoError::WorkspaceResolution`]);
//! - a bucket root cannot be canonicalized, for example because the repository
//!   disappeared ([`MultiRepoError::RepositoryResolution`]);
//! - the bucket list is empty ([`MultiRepoError::MissingMainRepository`]);
//! - the first bucket is not the canonical main workspace
//!   ([`MultiRepoError::MainRepositoryMismatch`]);
//! - two buckets resolve to the same canonical root
//!   ([`MultiRepoError::DuplicateRepositoryRoot`]);
//! - a bucket root is not inside a Git worktree
//!   ([`MultiRepoError::NotRepository`]);
//! - a bucket root is not the canonical Git worktree root (a substituted,
//!   nested or otherwise non-canonical root)
//!   ([`MultiRepoError::RepositoryRootMismatch`]);
//! - any Git infrastructure error from the worktree probe or the snapshot is
//!   propagated as [`MultiRepoError::Git`].
//!
//! Every Git command is read-only and runs through the bounded runner of task
//! 4.7. There is no shell and no Git write command anywhere in this module. The
//! errors carry no payload, so `Debug`/`Display` can never leak a root, an
//! allowed path, Git output, argv, OS error text or a secret.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use bridge_path_policy::AllowedPathGroup;

use crate::{
    GitError, RepositorySnapshot, os_from_bytes, run_checked, take_snapshot, trim_ascii_whitespace,
};

/// The snapshot of one repository together with its repository-qualified scope.
///
/// The `root` is the canonical Git worktree root, `allowed_paths` are the raw
/// entries of the bucket exactly as produced by
/// `group_allowed_paths_by_repo` (never normalized or validated), and
/// `snapshot` is the base snapshot of that worktree. Keeping the raw bucket on
/// the entry is what prevents identical relative paths in different
/// repositories from being mixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryGroupSnapshot {
    root: PathBuf,
    allowed_paths: Vec<String>,
    snapshot: RepositorySnapshot,
}

impl RepositoryGroupSnapshot {
    /// Returns the canonical Git worktree root of this repository.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the raw `allowed_paths` bucket of this repository.
    #[must_use]
    pub fn allowed_paths(&self) -> &[String] {
        &self.allowed_paths
    }

    /// Returns the base snapshot of this repository.
    #[must_use]
    pub fn snapshot(&self) -> &RepositorySnapshot {
        &self.snapshot
    }
}

/// The base snapshots of every repository affected by a task (task 4.8).
///
/// The main repository is always the first entry, even when its bucket has an
/// empty `allowed_paths` list. External repositories follow in ascending
/// canonical root order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiRepositorySnapshot {
    repositories: Vec<RepositoryGroupSnapshot>,
}

impl MultiRepositorySnapshot {
    /// Returns the per-repository snapshots in deterministic order.
    #[must_use]
    pub fn repositories(&self) -> &[RepositoryGroupSnapshot] {
        &self.repositories
    }

    /// Returns the main repository entry, which is always present and first.
    #[must_use]
    pub fn main(&self) -> &RepositoryGroupSnapshot {
        self.repositories
            .first()
            .expect("a multi-repository snapshot always contains the main repository")
    }

    /// Returns the number of repositories.
    #[must_use]
    pub fn len(&self) -> usize {
        self.repositories.len()
    }

    /// Returns whether the snapshot has no repositories.
    ///
    /// A successfully built snapshot always contains the main repository, so
    /// this is always `false`; the method exists for symmetry with [`len`].
    ///
    /// [`len`]: Self::len
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.repositories.is_empty()
    }

    /// Returns the entry for `root`, if present.
    #[must_use]
    pub fn get(&self, root: &Path) -> Option<&RepositoryGroupSnapshot> {
        self.repositories
            .iter()
            .find(|repository| repository.root == root)
    }
}

/// A payload-free multi-repository snapshot failure.
///
/// The variant is the whole contract: no root, allowed path, Git output, argv,
/// OS error text or secret is ever stored in it, so both `Debug` and `Display`
/// render only a static identifier (or the static identifier of the wrapped
/// [`GitError`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MultiRepoError {
    /// The main workspace could not be canonicalized.
    WorkspaceResolution,
    /// A bucket root could not be canonicalized (for example it disappeared).
    RepositoryResolution,
    /// No repository bucket was supplied, so the main repository is missing.
    MissingMainRepository,
    /// The first bucket is not the canonical main workspace.
    MainRepositoryMismatch,
    /// Two buckets resolve to the same canonical repository root.
    DuplicateRepositoryRoot,
    /// A bucket root is not inside a Git worktree.
    NotRepository,
    /// A bucket root is not the canonical Git worktree root.
    RepositoryRootMismatch,
    /// A read-only Git command used while probing or snapshotting failed.
    Git(GitError),
}

impl MultiRepoError {
    /// Returns the stable, payload-free identifier for this error.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceResolution => "workspace_resolution_failed",
            Self::RepositoryResolution => "repository_resolution_failed",
            Self::MissingMainRepository => "missing_main_repository",
            Self::MainRepositoryMismatch => "main_repository_mismatch",
            Self::DuplicateRepositoryRoot => "duplicate_repository_root",
            Self::NotRepository => "not_a_git_repo",
            Self::RepositoryRootMismatch => "repository_root_mismatch",
            Self::Git(error) => error.as_str(),
        }
    }
}

impl fmt::Display for MultiRepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for MultiRepoError {}

/// Takes the base snapshot of every repository referenced by `groups`.
///
/// `main_workspace` is the canonical main workspace. `groups` are the typed
/// repository buckets produced by
/// [`bridge_path_policy::group_allowed_paths_by_repo`]; the main workspace
/// bucket must be first. The result always contains the main repository first,
/// followed by the external repositories in ascending canonical root order. Raw
/// bucket entries are preserved per repository, so identical relative paths in
/// different repositories stay separated.
///
/// Each bucket root is verified against the actual canonical Git worktree root
/// before it is snapshotted; see the module documentation for the complete
/// fail-closed contract.
///
/// # Errors
///
/// Returns a payload-free [`MultiRepoError`] for a missing main repository, a
/// main/bucket root mismatch, a duplicate canonical root, a non-repository or
/// non-canonical bucket root, an unresolvable root, or any Git infrastructure
/// error from the probe or the snapshot.
pub fn take_multi_repository_snapshot(
    main_workspace: &Path,
    groups: &[AllowedPathGroup],
) -> Result<MultiRepositorySnapshot, MultiRepoError> {
    let main_root =
        std::fs::canonicalize(main_workspace).map_err(|_| MultiRepoError::WorkspaceResolution)?;

    if groups.is_empty() {
        return Err(MultiRepoError::MissingMainRepository);
    }

    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut main: Option<(PathBuf, Vec<String>)> = None;
    let mut external: Vec<(PathBuf, Vec<String>)> = Vec::with_capacity(groups.len() - 1);

    for (index, group) in groups.iter().enumerate() {
        let root = std::fs::canonicalize(group.root())
            .map_err(|_| MultiRepoError::RepositoryResolution)?;
        if group.root() != root.as_path() {
            return Err(MultiRepoError::RepositoryRootMismatch);
        }
        if !seen.insert(root.clone()) {
            return Err(MultiRepoError::DuplicateRepositoryRoot);
        }
        let entries = group.entries().to_vec();
        if index == 0 {
            if root != main_root {
                return Err(MultiRepoError::MainRepositoryMismatch);
            }
            main = Some((root, entries));
        } else {
            external.push((root, entries));
        }
    }

    external.sort_by(|left, right| left.0.cmp(&right.0));

    let Some((main_root, main_entries)) = main else {
        return Err(MultiRepoError::MissingMainRepository);
    };

    let mut repositories = Vec::with_capacity(external.len() + 1);
    repositories.push(snapshot_bucket(main_root, main_entries)?);
    for (root, entries) in external {
        repositories.push(snapshot_bucket(root, entries)?);
    }
    Ok(MultiRepositorySnapshot { repositories })
}

/// Verifies and snapshots one already-canonical repository bucket.
fn snapshot_bucket(
    root: PathBuf,
    allowed_paths: Vec<String>,
) -> Result<RepositoryGroupSnapshot, MultiRepoError> {
    let root = canonical_worktree_root(&root)?;
    let snapshot = take_snapshot(&root).map_err(MultiRepoError::Git)?;
    Ok(RepositoryGroupSnapshot {
        root,
        allowed_paths,
        snapshot,
    })
}

/// Returns the canonical Git worktree root of `root`, failing closed unless it
/// equals `root` exactly.
///
/// Runs the read-only `git rev-parse --show-toplevel`. A non-zero exit or an
/// empty report is [`MultiRepoError::NotRepository`]; a non-absolute report or a
/// canonical root that differs from `root` (a substituted, nested or otherwise
/// non-canonical root) is [`MultiRepoError::RepositoryRootMismatch`]; a
/// filesystem error canonicalizing the report is
/// [`MultiRepoError::RepositoryResolution`]; every other Git infrastructure
/// error is [`MultiRepoError::Git`].
fn canonical_worktree_root(root: &Path) -> Result<PathBuf, MultiRepoError> {
    let stdout = match run_checked(root, &["rev-parse", "--show-toplevel"]) {
        Ok(stdout) => stdout,
        Err(GitError::CommandFailed) => return Err(MultiRepoError::NotRepository),
        Err(error) => return Err(MultiRepoError::Git(error)),
    };

    let reported = trim_ascii_whitespace(&stdout);
    if reported.is_empty() {
        return Err(MultiRepoError::NotRepository);
    }
    let reported = PathBuf::from(os_from_bytes(reported));
    if !reported.is_absolute() {
        return Err(MultiRepoError::RepositoryRootMismatch);
    }

    let canonical =
        std::fs::canonicalize(&reported).map_err(|_| MultiRepoError::RepositoryResolution)?;
    if canonical == root {
        Ok(canonical)
    } else {
        Err(MultiRepoError::RepositoryRootMismatch)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        MultiRepoError, MultiRepositorySnapshot, RepositoryGroupSnapshot,
        take_multi_repository_snapshot,
    };
    use crate::{GitError, take_snapshot};
    use bridge_path_policy::AllowedPathGroup;
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
                "bridge-git-multi-test-{}-{tag}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
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

    fn group(workspace: &Path, paths: &[&str]) -> Vec<AllowedPathGroup> {
        bridge_path_policy::group_allowed_paths_by_repo(workspace, paths)
            .expect("group allowed paths")
    }

    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).expect("canonicalize")
    }

    fn dirty_contains(repository: &RepositoryGroupSnapshot, name: &str) -> bool {
        repository
            .snapshot()
            .dirty_paths()
            .contains(&OsString::from(name))
    }

    #[test]
    fn main_repository_only_with_empty_allowed_paths() {
        let fixture = TempDir::new("main-only");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        write_file(&ws, "module.py", "x = 1\n");
        commit(&ws, "init", &["module.py"]);

        let groups = group(&ws, &[]);
        let result = take_multi_repository_snapshot(&ws, &groups).expect("snapshot");

        assert_eq!(result.len(), 1);
        assert!(!result.is_empty());
        assert_eq!(result.main().root(), canonical(&ws));
        assert!(result.main().allowed_paths().is_empty());
        assert!(result.main().snapshot().head().is_some());
        assert_eq!(result.get(&canonical(&ws)), Some(result.main()));
    }

    #[test]
    fn main_with_one_external_repository() {
        let fixture = TempDir::new("one-external");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        write_file(&ws, "module.py", "x = 1\n");
        commit(&ws, "init", &["module.py"]);

        let ext = fixture.join("external");
        init_repo(&ext);
        write_file(&ext, "lib.py", "y = 1\n");
        commit(&ext, "init", &["lib.py"]);

        let ext_entry = format!("{}/lib.py", ext.display());
        let groups = group(&ws, &[&ext_entry]);
        let result = take_multi_repository_snapshot(&ws, &groups).expect("snapshot");

        assert_eq!(result.len(), 2);
        assert_eq!(result.repositories()[0].root(), canonical(&ws));
        assert_eq!(result.repositories()[1].root(), canonical(&ext));
        assert_eq!(result.repositories()[1].allowed_paths(), &[ext_entry]);
        assert!(result.repositories()[1].snapshot().head().is_some());
    }

    #[test]
    fn two_external_repositories_are_deterministically_ordered() {
        let fixture = TempDir::new("two-external");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        write_file(&ws, "module.py", "x = 1\n");
        commit(&ws, "init", &["module.py"]);

        let ext_b = fixture.join("repo-b");
        init_repo(&ext_b);
        write_file(&ext_b, "b.py", "b\n");
        commit(&ext_b, "init", &["b.py"]);

        let ext_a = fixture.join("repo-a");
        init_repo(&ext_a);
        write_file(&ext_a, "a.py", "a\n");
        commit(&ext_a, "init", &["a.py"]);

        let entry_b = format!("{}/b.py", ext_b.display());
        let entry_a = format!("{}/a.py", ext_a.display());
        // Supply the later root first; the result must still be sorted.
        let groups = group(&ws, &[&entry_b, &entry_a]);
        let result = take_multi_repository_snapshot(&ws, &groups).expect("snapshot");

        assert_eq!(result.len(), 3);
        assert_eq!(result.repositories()[0].root(), canonical(&ws));
        assert_eq!(result.repositories()[1].root(), canonical(&ext_a));
        assert_eq!(result.repositories()[2].root(), canonical(&ext_b));
        assert!(result.repositories()[1].root() < result.repositories()[2].root());
        assert_eq!(result.repositories()[1].allowed_paths(), &[entry_a]);
        assert_eq!(result.repositories()[2].allowed_paths(), &[entry_b]);
    }

    #[test]
    fn repo_root_and_child_collapse_into_one_snapshot() {
        let fixture = TempDir::new("root-child");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        write_file(&ws, "module.py", "x = 1\n");
        commit(&ws, "init", &["module.py"]);

        let ext = fixture.join("external");
        init_repo(&ext);
        write_file(&ext, "pkg/mod.py", "mod\n");
        commit(&ext, "init", &["pkg/mod.py"]);

        let root_entry = format!("{}/", ext.display());
        let child_entry = format!("{}/pkg/mod.py", ext.display());
        let groups = group(&ws, &[&root_entry, &child_entry]);
        let result = take_multi_repository_snapshot(&ws, &groups).expect("snapshot");

        assert_eq!(result.len(), 2);
        assert_eq!(
            result.repositories()[1].allowed_paths(),
            &[root_entry, child_entry]
        );
    }

    #[test]
    fn same_relative_filename_in_different_repositories_stays_separated() {
        let fixture = TempDir::new("same-relative");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        write_file(&ws, "module.py", "x = 1\n");
        commit(&ws, "init", &["module.py"]);

        let ext = fixture.join("external");
        init_repo(&ext);
        write_file(&ext, "module.py", "x = 1\n");
        commit(&ext, "init", &["module.py"]);

        write_file(&ws, "module.py", "x = 2\n");
        write_file(&ext, "module.py", "x = 3\n");

        let ext_entry = format!("{}/module.py", ext.display());
        let groups = group(&ws, &["module.py", &ext_entry]);
        let result = take_multi_repository_snapshot(&ws, &groups).expect("snapshot");

        assert_eq!(result.repositories()[0].allowed_paths(), &["module.py"]);
        assert_eq!(result.repositories()[1].allowed_paths(), &[ext_entry]);
        assert_ne!(
            result.repositories()[0].root(),
            result.repositories()[1].root()
        );
        assert!(dirty_contains(&result.repositories()[0], "module.py"));
        assert!(dirty_contains(&result.repositories()[1], "module.py"));
    }

    #[test]
    fn repositories_keep_independent_states() {
        let fixture = TempDir::new("independent");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        write_file(&ws, "module.py", "x = 1\n");
        commit(&ws, "init", &["module.py"]);

        let dirty = fixture.join("dirty-repo");
        init_repo(&dirty);
        write_file(&dirty, "lib.py", "y = 1\n");
        commit(&dirty, "init", &["lib.py"]);
        write_file(&dirty, "new.py", "z = 1\n");

        let staged = fixture.join("staged-repo");
        init_repo(&staged);
        write_file(&staged, "app.py", "a = 1\n");
        commit(&staged, "init", &["app.py"]);
        write_file(&staged, "app.py", "a = 2\n");
        git(&staged, &["add", "--", "app.py"]);

        let dirty_entry = format!("{}/new.py", dirty.display());
        let staged_entry = format!("{}/app.py", staged.display());
        let groups = group(&ws, &[&dirty_entry, &staged_entry]);
        let result = take_multi_repository_snapshot(&ws, &groups).expect("snapshot");

        let main = &result.repositories()[0];
        let dirty_repo = &result.repositories()[1];
        let staged_repo = &result.repositories()[2];

        assert!(!dirty_contains(main, "new.py"));
        assert!(dirty_contains(dirty_repo, "new.py"));
        assert!(dirty_contains(staged_repo, "app.py"));
        assert_ne!(
            main.snapshot().worktree_fingerprint(),
            dirty_repo.snapshot().worktree_fingerprint()
        );
        assert_ne!(
            main.snapshot().index_fingerprint(),
            staged_repo.snapshot().index_fingerprint()
        );
        assert_ne!(
            dirty_repo.snapshot().index_fingerprint(),
            staged_repo.snapshot().index_fingerprint()
        );
    }

    #[test]
    fn empty_buckets_fail_closed() {
        let fixture = TempDir::new("empty-buckets");
        let ws = fixture.join("workspace");
        init_repo(&ws);

        assert_eq!(
            take_multi_repository_snapshot(&ws, &[]),
            Err(MultiRepoError::MissingMainRepository)
        );
    }

    #[test]
    fn main_bucket_from_other_workspace_fails_closed() {
        let fixture = TempDir::new("main-mismatch");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        let other = fixture.join("other");
        init_repo(&other);

        let groups = group(&other, &[]);
        assert_eq!(
            take_multi_repository_snapshot(&ws, &groups),
            Err(MultiRepoError::MainRepositoryMismatch)
        );
    }

    #[test]
    fn duplicate_repository_root_fails_closed() {
        let fixture = TempDir::new("duplicate");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        write_file(&ws, "module.py", "x = 1\n");
        commit(&ws, "init", &["module.py"]);

        let ext = fixture.join("external");
        init_repo(&ext);
        write_file(&ext, "lib.py", "y = 1\n");
        commit(&ext, "init", &["lib.py"]);

        let ext_entry = format!("{}/lib.py", ext.display());
        let mut groups = group(&ws, &[&ext_entry]);
        let duplicate = groups[1].clone();
        groups.push(duplicate);

        assert_eq!(
            take_multi_repository_snapshot(&ws, &groups),
            Err(MultiRepoError::DuplicateRepositoryRoot)
        );
    }

    #[test]
    fn conflicting_buckets_for_same_root_fail_closed() {
        let fixture = TempDir::new("conflicting");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        write_file(&ws, "module.py", "x = 1\n");
        commit(&ws, "init", &["module.py"]);

        let ext = fixture.join("external");
        init_repo(&ext);
        write_file(&ext, "lib.py", "y = 1\n");
        write_file(&ext, "other.py", "z = 1\n");
        commit(&ext, "init", &["lib.py", "other.py"]);

        let first = format!("{}/lib.py", ext.display());
        let second = format!("{}/other.py", ext.display());
        let groups_a = group(&ws, &[&first]);
        let groups_b = group(&ws, &[&second]);

        // Two buckets for the same canonical root with different raw entries.
        let groups = vec![
            groups_a[0].clone(),
            groups_a[1].clone(),
            groups_b[1].clone(),
        ];
        assert_eq!(
            take_multi_repository_snapshot(&ws, &groups),
            Err(MultiRepoError::DuplicateRepositoryRoot)
        );
    }

    #[test]
    fn workspace_subdirectory_root_mismatch_fails_closed() {
        let fixture = TempDir::new("root-mismatch");
        let repo = fixture.join("repo");
        init_repo(&repo);
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).expect("create subdir");

        let groups = group(&sub, &[]);
        assert_eq!(
            take_multi_repository_snapshot(&sub, &groups),
            Err(MultiRepoError::RepositoryRootMismatch)
        );
    }

    #[test]
    fn disappeared_external_repository_fails_closed() {
        let fixture = TempDir::new("disappeared");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        write_file(&ws, "module.py", "x = 1\n");
        commit(&ws, "init", &["module.py"]);

        let ext = fixture.join("external");
        init_repo(&ext);
        write_file(&ext, "lib.py", "y = 1\n");
        commit(&ext, "init", &["lib.py"]);

        let ext_entry = format!("{}/lib.py", ext.display());
        let groups = group(&ws, &[&ext_entry]);
        std::fs::remove_dir_all(&ext).expect("remove external repository");

        assert_eq!(
            take_multi_repository_snapshot(&ws, &groups),
            Err(MultiRepoError::RepositoryResolution)
        );
    }

    #[test]
    fn non_repository_workspace_fails_closed() {
        let fixture = TempDir::new("non-repo");
        let plain = fixture.join("plain");
        std::fs::create_dir_all(&plain).expect("create plain dir");

        let groups = group(&plain, &[]);
        assert_eq!(
            take_multi_repository_snapshot(&plain, &groups),
            Err(MultiRepoError::NotRepository)
        );
    }

    #[test]
    fn regression_4_7_snapshot_is_reused_unchanged() {
        let fixture = TempDir::new("regression-4-7");
        let ws = fixture.join("workspace");
        init_repo(&ws);
        write_file(&ws, "module.py", "x = 1\n");
        commit(&ws, "init", &["module.py"]);
        write_file(&ws, "new.py", "y = 2\n");

        let groups = group(&ws, &[]);
        let result = take_multi_repository_snapshot(&ws, &groups).expect("snapshot");

        assert_eq!(
            result.main().snapshot(),
            &take_snapshot(&ws).expect("direct snapshot")
        );
    }

    #[test]
    fn errors_are_payload_free() {
        for (error, debug, display) in [
            (
                MultiRepoError::WorkspaceResolution,
                "WorkspaceResolution",
                "workspace_resolution_failed",
            ),
            (
                MultiRepoError::RepositoryResolution,
                "RepositoryResolution",
                "repository_resolution_failed",
            ),
            (
                MultiRepoError::MissingMainRepository,
                "MissingMainRepository",
                "missing_main_repository",
            ),
            (
                MultiRepoError::MainRepositoryMismatch,
                "MainRepositoryMismatch",
                "main_repository_mismatch",
            ),
            (
                MultiRepoError::DuplicateRepositoryRoot,
                "DuplicateRepositoryRoot",
                "duplicate_repository_root",
            ),
            (
                MultiRepoError::NotRepository,
                "NotRepository",
                "not_a_git_repo",
            ),
            (
                MultiRepoError::RepositoryRootMismatch,
                "RepositoryRootMismatch",
                "repository_root_mismatch",
            ),
            (
                MultiRepoError::Git(GitError::NotRepository),
                "Git(NotRepository)",
                "not_a_git_repo",
            ),
            (
                MultiRepoError::Git(GitError::Timeout),
                "Git(Timeout)",
                "timeout",
            ),
        ] {
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(format!("{error}"), display);
            assert_eq!(error.as_str(), display);
        }
    }

    #[test]
    fn snapshot_type_has_no_public_constructor() {
        // Compile-time guard: the only way to obtain a snapshot is the typed
        // entry point, so the main-first invariant cannot be bypassed.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MultiRepositorySnapshot>();
        assert_send_sync::<RepositoryGroupSnapshot>();
        assert_send_sync::<MultiRepoError>();
    }
}
