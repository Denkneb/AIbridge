//! Local branch metadata and explicit, non-forcing switches; no fetch or shell.
use crate::{GitError, check_repository, head, run_checked, run_git, status_porcelain};
use std::path::{Path, PathBuf};

pub struct Branch {
    pub reference: String,
    pub name: String,
    pub remote: bool,
}
pub struct BranchState {
    pub current: Option<String>,
    pub head: Option<String>,
    pub branches: Vec<Branch>,
}

/// Returns the worktree root, including when the configured workspace is nested.
pub fn repository_root(workspace: &Path) -> Result<PathBuf, GitError> {
    let bytes = run_checked(workspace, &["rev-parse", "--show-toplevel"])?;
    let text = std::str::from_utf8(&bytes).map_err(|_| GitError::MalformedOutput)?;
    PathBuf::from(text.strip_suffix('\n').ok_or(GitError::MalformedOutput)?)
        .canonicalize()
        .map_err(|_| GitError::Io)
}

/// Lists local and already known remote branches; skips remote HEAD aliases.
pub fn branches(workspace: &Path) -> Result<BranchState, GitError> {
    check_repository(workspace)?;
    let output = run_git(workspace, &["symbolic-ref", "--quiet", "HEAD"])?;
    let current = if output.status.success() {
        Some(
            std::str::from_utf8(&output.stdout)
                .map_err(|_| GitError::MalformedOutput)?
                .trim_end_matches('\n')
                .to_owned(),
        )
    } else if output.status.code() == Some(1) {
        None
    } else {
        return Err(GitError::CommandFailed);
    };
    let output = run_checked(
        workspace,
        &[
            "for-each-ref",
            "--sort=refname",
            "--format=%(refname)%00%(symref)",
            "refs/heads/",
            "refs/remotes/",
        ],
    )?;
    let mut branches = Vec::new();
    for line in std::str::from_utf8(&output)
        .map_err(|_| GitError::MalformedOutput)?
        .lines()
    {
        let (reference, symbolic) = line.split_once('\0').ok_or(GitError::MalformedOutput)?;
        if !symbolic.is_empty() {
            continue;
        }
        let (name, remote) = if let Some(name) = reference.strip_prefix("refs/heads/") {
            (name, false)
        } else if let Some(name) = reference.strip_prefix("refs/remotes/") {
            (name, true)
        } else {
            return Err(GitError::MalformedOutput);
        };
        branches.push(Branch {
            reference: reference.into(),
            name: name.into(),
            remote,
        });
    }
    Ok(BranchState {
        current,
        head: head(workspace)?.map(|h| h.as_str().to_owned()),
        branches,
    })
}

/// Refuses an in-progress merge, rebase, cherry-pick, revert or bisect.
pub fn operation_in_progress(workspace: &Path) -> Result<bool, GitError> {
    for name in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "BISECT_LOG",
    ] {
        let bytes = run_checked(workspace, &["rev-parse", "--git-path", name])?;
        let path = PathBuf::from(
            std::str::from_utf8(&bytes)
                .map_err(|_| GitError::MalformedOutput)?
                .trim_end_matches('\n'),
        );
        let path = if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        };
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(GitError::Io),
        }
    }
    Ok(false)
}

/// Switches only to an existing listed reference, preserving all dirty files.
/// Remote selection creates the corresponding local tracking branch.
pub fn switch_branch(workspace: &Path, reference: &str) -> Result<(), GitError> {
    let state = branches(workspace)?;
    let target = state
        .branches
        .iter()
        .find(|b| b.reference == reference)
        .ok_or(GitError::CommandFailed)?;
    if !status_porcelain(workspace)?.is_empty() || operation_in_progress(workspace)? {
        return Err(GitError::CommandFailed);
    }
    let mut args = vec![
        "-c",
        "core.hooksPath=/dev/null",
        "switch",
        "--no-guess",
        "--no-overwrite-ignore",
    ];
    if target.remote {
        args.extend(["--track", "--", target.reference.as_str()]);
    } else {
        args.extend(["--", target.name.as_str()]);
    }
    run_checked(workspace, &args)?;
    Ok(())
}
