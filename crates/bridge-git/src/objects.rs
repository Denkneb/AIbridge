//! Bounded read-only access to exact commit blobs for delivery preflight.
use crate::{GitError, run_checked_os};
use std::{ffi::OsStr, path::Path};
#[derive(Clone, PartialEq, Eq)]
pub struct CommitObject {
    pub mode: u32,
    pub data: Vec<u8>,
}
/// Reads one literal path from a commit, never using a revision/path expression.
pub fn commit_object(
    repo: &Path,
    commit: &str,
    path: &str,
) -> Result<Option<CommitObject>, GitError> {
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(GitError::MalformedOutput);
    }
    let tree = run_checked_os(
        repo,
        &[
            OsStr::new("ls-tree"),
            OsStr::new("-rz"),
            OsStr::new("--full-tree"),
            OsStr::new(commit),
        ],
    )?;
    for row in tree.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = row
            .iter()
            .position(|b| *b == b'\t')
            .ok_or(GitError::MalformedOutput)?;
        if &row[tab + 1..] != path.as_bytes() {
            continue;
        }
        let fields = std::str::from_utf8(&row[..tab])
            .map_err(|_| GitError::MalformedOutput)?
            .split(' ')
            .collect::<Vec<_>>();
        if fields.len() != 3 || fields[1] != "blob" {
            return Err(GitError::MalformedOutput);
        }
        let mode = u32::from_str_radix(fields[0], 8).map_err(|_| GitError::MalformedOutput)?;
        if !matches!(mode, 0o100644 | 0o100755 | 0o120000) {
            return Err(GitError::MalformedOutput);
        }
        let data = run_checked_os(
            repo,
            &[
                OsStr::new("cat-file"),
                OsStr::new("blob"),
                OsStr::new(fields[2]),
            ],
        )?;
        return Ok(Some(CommitObject { mode, data }));
    }
    Ok(None)
}
/// Fixed-argv ancestry check; false means main history diverged or was rewritten.
pub fn commit_descends_from(repo: &Path, base: &str, current: &str) -> Result<bool, GitError> {
    for oid in [base, current] {
        if !matches!(oid.len(), 40 | 64)
            || !oid
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(GitError::MalformedOutput);
        }
    }
    let output = crate::runner::run_bounded(
        OsStr::new("git"),
        repo,
        &[
            OsStr::new("merge-base"),
            OsStr::new("--is-ancestor"),
            OsStr::new(base),
            OsStr::new(current),
        ],
        std::time::Duration::from_secs(30),
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GitError::MalformedOutput),
    }
}
