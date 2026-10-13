//! Named worktrees for standalone consoles, separate from detached task checkouts.
use crate::{GitError, branches, run_checked, run_checked_os, transfer};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

pub struct ConsoleWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub locked: bool,
    pub prunable: bool,
}

/// NUL-delimited output preserves spaces, quotes and newlines in paths.
pub fn list(workspace: &Path) -> Result<Vec<ConsoleWorktree>, GitError> {
    let bytes = run_checked(workspace, &["worktree", "list", "--porcelain", "-z"])?;
    let mut result = Vec::new();
    let mut current: Option<ConsoleWorktree> = None;
    for field in bytes.split(|byte| *byte == 0) {
        let field = std::str::from_utf8(field).map_err(|_| GitError::MalformedOutput)?;
        if let Some(path) = field.strip_prefix("worktree ") {
            if let Some(previous) = current.take() {
                result.push(previous);
            }
            current = Some(ConsoleWorktree {
                path: path.into(),
                branch: None,
                locked: false,
                prunable: false,
            });
        } else if let Some(record) = current.as_mut() {
            if let Some(branch) = field.strip_prefix("branch ") {
                record.branch = Some(branch.into());
            }
            if field == "locked" || field.starts_with("locked ") {
                record.locked = true;
            }
            if field == "prunable" || field.starts_with("prunable ") {
                record.prunable = true;
            }
        }
    }
    if let Some(record) = current {
        result.push(record);
    }
    Ok(result)
}

/// Does not require a clean source checkout or switch its branch.
pub fn create(
    workspace: &Path,
    destination: &Path,
    branch: &str,
    base: Option<&str>,
) -> Result<(), GitError> {
    transfer::valid_name(workspace, branch)?;
    let state = branches::branches(workspace)?;
    let reference = format!("refs/heads/{branch}");
    if list(workspace)?
        .iter()
        .any(|record| record.branch.as_deref() == Some(&reference))
        || destination.exists()
    {
        return Err(GitError::CommandFailed);
    }
    let mut args = vec![
        OsStr::new("-c"),
        OsStr::new("core.hooksPath=/dev/null"),
        OsStr::new("worktree"),
        OsStr::new("add"),
    ];
    let commit;
    if let Some(base) = base {
        if state
            .branches
            .iter()
            .any(|item| item.reference == reference)
            || (base != "HEAD" && !state.branches.iter().any(|item| item.reference == base))
        {
            return Err(GitError::CommandFailed);
        }
        commit = String::from_utf8(run_checked(
            workspace,
            &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
        )?)
        .map_err(|_| GitError::MalformedOutput)?;
        args.extend([
            OsStr::new("--no-track"),
            OsStr::new("-b"),
            OsStr::new(branch),
            OsStr::new("--"),
            destination.as_os_str(),
            OsStr::new(commit.trim()),
        ]);
    } else {
        if !state
            .branches
            .iter()
            .any(|item| item.reference == reference && !item.remote)
        {
            return Err(GitError::CommandFailed);
        }
        args.extend([
            OsStr::new("--"),
            destination.as_os_str(),
            OsStr::new(branch),
        ]);
    }
    run_checked_os(workspace, &args)?;
    Ok(())
}
