//! Read-only, bounded text review. Temporary inputs never touch a checkout.
use crate::{GitError, objects::CommitObject, run_checked_os, runner::run_bounded};
use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::Duration,
};
pub const FILE_LIMIT: usize = 256 * 1024;
pub const PATCH_LIMIT: usize = 512 * 1024;
pub fn file_digest(mode: u32, data: &[u8]) -> String {
    let mut hash = crate::sha256::Sha256::new();
    hash.update(if mode == 0o120000 {
        b"link\0"
    } else if mode == 0o100755 {
        b"exec\0"
    } else {
        b"file\0"
    });
    hash.update(data);
    crate::sha256::to_hex(&hash.finalize())
}

/// An outer None is an absent path; an inner None is an oversized blob.
pub fn commit_file(
    repo: &Path,
    commit: &str,
    path: &str,
) -> Result<Option<Option<CommitObject>>, GitError> {
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(GitError::MalformedOutput);
    }
    let args = [
        "--literal-pathspecs",
        "ls-tree",
        "-z",
        "--full-tree",
        commit,
        "--",
        path,
    ];
    let tree = run_checked_os(repo, &args.map(OsStr::new))?;
    if tree.is_empty() {
        return Ok(None);
    }
    let row = tree.strip_suffix(b"\0").ok_or(GitError::MalformedOutput)?;
    let (meta, name) = row.split_at(
        row.iter()
            .position(|b| *b == b'\t')
            .ok_or(GitError::MalformedOutput)?,
    );
    if &name[1..] != path.as_bytes() {
        return Err(GitError::MalformedOutput);
    }
    let fields = std::str::from_utf8(meta)
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
    let size = run_checked_os(
        repo,
        &[
            OsStr::new("cat-file"),
            OsStr::new("-s"),
            OsStr::new(fields[2]),
        ],
    )?;
    let size = std::str::from_utf8(&size)
        .map_err(|_| GitError::MalformedOutput)?
        .trim()
        .parse::<usize>()
        .map_err(|_| GitError::MalformedOutput)?;
    if size > FILE_LIMIT {
        return Ok(Some(None));
    }
    let data = run_checked_os(
        repo,
        &[
            OsStr::new("cat-file"),
            OsStr::new("blob"),
            OsStr::new(fields[2]),
        ],
    )?;
    if data.len() != size {
        return Err(GitError::MalformedOutput);
    }
    Ok(Some(Some(CommitObject { mode, data })))
}
/// Read the stage-zero blob without following filesystem links or running filters.
pub fn index_file(repo: &Path, path: &str) -> Result<Option<Option<CommitObject>>, GitError> {
    let bytes = run_checked_os(
        repo,
        &[
            OsStr::new("--literal-pathspecs"),
            OsStr::new("ls-files"),
            OsStr::new("--stage"),
            OsStr::new("-z"),
            OsStr::new("--"),
            OsStr::new(path),
        ],
    )?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let row = bytes.strip_suffix(b"\0").ok_or(GitError::MalformedOutput)?;
    let split = row
        .iter()
        .position(|b| *b == b'\t')
        .ok_or(GitError::MalformedOutput)?;
    if &row[split + 1..] != path.as_bytes() {
        return Err(GitError::MalformedOutput);
    }
    let fields = std::str::from_utf8(&row[..split])
        .map_err(|_| GitError::MalformedOutput)?
        .split(' ')
        .collect::<Vec<_>>();
    if fields.len() != 3 || fields[2] != "0" {
        return Err(GitError::MalformedOutput);
    }
    let mode = u32::from_str_radix(fields[0], 8).map_err(|_| GitError::MalformedOutput)?;
    if !matches!(mode, 0o100644 | 0o100755 | 0o120000) {
        return Err(GitError::MalformedOutput);
    }
    let size = run_checked_os(
        repo,
        &[
            OsStr::new("cat-file"),
            OsStr::new("-s"),
            OsStr::new(fields[1]),
        ],
    )?;
    let size = std::str::from_utf8(&size)
        .map_err(|_| GitError::MalformedOutput)?
        .trim()
        .parse::<usize>()
        .map_err(|_| GitError::MalformedOutput)?;
    if size > FILE_LIMIT {
        return Ok(Some(None));
    }
    let data = run_checked_os(
        repo,
        &[
            OsStr::new("cat-file"),
            OsStr::new("blob"),
            OsStr::new(fields[1]),
        ],
    )?;
    if data.len() != size {
        return Err(GitError::MalformedOutput);
    }
    Ok(Some(Some(CommitObject { mode, data })))
}

/// Fixed read-only comparisons; rename detection is disabled so paths stay independent.
pub fn project_changes(
    repo: &Path,
    scope: &str,
    head: Option<&str>,
) -> Result<Vec<(String, String)>, GitError> {
    use std::collections::BTreeMap;
    if !matches!(scope, "all" | "staged" | "unstaged") {
        return Err(GitError::MalformedOutput);
    }
    let mut files = BTreeMap::new();
    let comparisons: Vec<&str> = match scope {
        "all" if head.is_none() => vec!["staged", "unstaged"],
        _ => vec![scope],
    };
    for comparison in comparisons {
        let mut args = vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-status",
            "-z",
        ];
        if comparison == "staged" {
            args.push("--cached");
        }
        if comparison != "unstaged"
            && let Some(head) = head
        {
            args.push(head);
        }
        args.push("--");
        let bytes = run_checked_os(repo, &args.iter().map(OsStr::new).collect::<Vec<_>>())?;
        let fields = bytes.split(|b| *b == 0).collect::<Vec<_>>();
        if fields.last() != Some(&&b""[..]) || fields.len() % 2 != 1 {
            return Err(GitError::MalformedOutput);
        }
        for pair in fields[..fields.len() - 1].as_chunks::<2>().0 {
            let status = match pair[0] {
                b"A" => "added",
                b"D" => "deleted",
                b"M" | b"T" | b"U" => "modified",
                _ => return Err(GitError::MalformedOutput),
            };
            let path = std::str::from_utf8(pair[1]).map_err(|_| GitError::MalformedOutput)?;
            files.insert(path.to_owned(), status.to_owned());
        }
    }
    if scope != "staged" {
        let bytes = run_checked_os(
            repo,
            &[
                OsStr::new("ls-files"),
                OsStr::new("--others"),
                OsStr::new("--exclude-standard"),
                OsStr::new("-z"),
            ],
        )?;
        for path in bytes.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            files.insert(
                std::str::from_utf8(path)
                    .map_err(|_| GitError::MalformedOutput)?
                    .to_owned(),
                "added".to_owned(),
            );
        }
    }
    if scope == "all" && head.is_none() {
        files.retain(|path, _| fs::symlink_metadata(repo.join(path)).is_ok());
        for change in files.values_mut() {
            *change = "added".into();
        }
    }
    Ok(files.into_iter().collect())
}

struct Inputs(PathBuf);
impl Drop for Inputs {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
pub fn text_patch(before: &[u8], after: &[u8]) -> Result<(String, bool), GitError> {
    if before.len() > FILE_LIMIT || after.len() > FILE_LIMIT {
        return Err(GitError::MalformedOutput);
    }
    let root = std::env::temp_dir().join(format!("aibridge-diff-{}", uuid::Uuid::new_v4()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .map_err(|_| GitError::Io)?;
    let inputs = Inputs(root);
    fs::write(inputs.0.join("before"), before).map_err(|_| GitError::Io)?;
    fs::write(inputs.0.join("after"), after).map_err(|_| GitError::Io)?;
    let args = [
        "--no-pager",
        "diff",
        "--no-index",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--text",
        "--unified=3",
        "--diff-algorithm=myers",
        "--",
        "before",
        "after",
    ];
    let output = run_bounded(
        OsStr::new("git"),
        &inputs.0,
        &args.map(OsStr::new),
        Duration::from_secs(10),
    )?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(GitError::CommandFailed);
    }
    let mut bytes = output.stdout;
    let truncated = bytes.len() > PATCH_LIMIT;
    if truncated {
        bytes.truncate(PATCH_LIMIT);
        bytes.truncate(
            bytes
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(0, |index| index + 1),
        );
    }
    Ok((
        String::from_utf8(bytes).map_err(|_| GitError::MalformedOutput)?,
        truncated,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patch_limits_and_missing_newline_preserve_valid_unicode() {
        let (patch, truncated) = text_patch("старое".as_bytes(), "новое".as_bytes()).unwrap();
        assert!(!truncated);
        assert!(patch.contains("-старое"));
        assert!(patch.contains("+новое"));
        assert!(patch.contains("No newline"));
        let (patch, truncated) =
            text_patch(&b"a\n".repeat(100000), &b"b\n".repeat(100000)).unwrap();
        assert!(truncated);
        assert!(patch.len() <= PATCH_LIMIT);
        assert!(patch.ends_with('\n'));
        assert!(text_patch(&vec![0; FILE_LIMIT + 1], b"").is_err());
    }
}
