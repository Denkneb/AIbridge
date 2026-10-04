//! Detached task checkouts. Writes are limited to one proven task slot and its
//! Git registration; main HEAD/index/refs and unrelated registrations are kept.
use crate::{GitError, runner::run_bounded};
use bridge_domain::TaskId;
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutError {
    Traversal,
    Mismatch,
    Unsupported,
    Io,
    Git(GitError),
}
impl fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Traversal => "worktree_path_unsafe",
            Self::Mismatch => "execution_root_mismatch",
            Self::Unsupported => "worktree_repository_unsupported",
            Self::Io => "worktree_filesystem_error",
            Self::Git(_) => "worktree_git_error",
        })
    }
}
impl std::error::Error for CheckoutError {}
impl From<GitError> for CheckoutError {
    fn from(e: GitError) -> Self {
        Self::Git(e)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct CheckoutPaths {
    pub task_dir: PathBuf,
    pub checkout: PathBuf,
    pub runtime_dir: PathBuf,
}
impl fmt::Debug for CheckoutPaths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CheckoutPaths { .. }")
    }
}
impl CheckoutPaths {
    /// Derives a deterministic slot, refusing every symlink below canonical state.
    /// # Errors
    /// Rejects a missing state directory, symlinks or non-directory components.
    pub fn new(state: &Path, task: TaskId) -> Result<Self, CheckoutError> {
        let state = fs::canonicalize(state).map_err(|_| CheckoutError::Io)?;
        let task_dir = state.join("worktrees").join(task.to_string());
        for path in [
            state.join("worktrees"),
            task_dir.clone(),
            task_dir.join("checkout"),
            task_dir.join("runtime"),
        ] {
            safe_directory(&path)?;
        }
        Ok(Self {
            checkout: task_dir.join("checkout"),
            runtime_dir: task_dir.join("runtime"),
            task_dir,
        })
    }
    /// # Errors
    /// Refuses a persisted path that differs from the deterministic task slot.
    pub fn require_checkout(&self, expected: &Path) -> Result<(), CheckoutError> {
        if expected != self.checkout {
            return Err(CheckoutError::Mismatch);
        }
        Ok(())
    }
}
fn safe_directory(path: &Path) -> Result<(), CheckoutError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => Err(CheckoutError::Traversal),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(CheckoutError::Io),
    }
}
/// Proven two-way Git admin binding. Debug never includes paths or identities.
#[derive(Clone, PartialEq, Eq)]
pub struct CheckoutBinding {
    pub paths: CheckoutPaths,
    pub base_head: String,
    pub common_dir: PathBuf,
    pub git_dir: PathBuf,
}
impl fmt::Debug for CheckoutBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CheckoutBinding { .. }")
    }
}
fn git(root: &Path, args: &[&OsStr]) -> Result<Vec<u8>, CheckoutError> {
    let result = run_bounded(OsStr::new("git"), root, args, Duration::from_secs(30))?;
    if !result.status.success() {
        return Err(CheckoutError::Git(GitError::CommandFailed));
    }
    Ok(result.stdout)
}
fn text(root: &Path, args: &[&str]) -> Result<String, CheckoutError> {
    String::from_utf8(git(root, &args.iter().map(OsStr::new).collect::<Vec<_>>())?)
        .map(|s| s.trim_end_matches(['\r', '\n']).into())
        .map_err(|_| CheckoutError::Mismatch)
}
fn git_path(root: &Path, flag: &str) -> Result<PathBuf, CheckoutError> {
    let raw = PathBuf::from(text(root, &["rev-parse", flag])?);
    fs::canonicalize(if raw.is_absolute() {
        raw
    } else {
        root.join(raw)
    })
    .map_err(|_| CheckoutError::Mismatch)
}
/// # Errors
/// Requires a non-bare top-level workspace, with an accessible shared admin dir.
pub fn main_common_dir(workspace: &Path) -> Result<PathBuf, CheckoutError> {
    let workspace = fs::canonicalize(workspace).map_err(|_| CheckoutError::Io)?;
    if text(&workspace, &["rev-parse", "--is-inside-work-tree"])? != "true"
        || text(&workspace, &["rev-parse", "--is-bare-repository"])? != "false"
        || git_path(&workspace, "--show-toplevel")? != workspace
    {
        return Err(CheckoutError::Mismatch);
    }
    git_path(&workspace, "--git-common-dir")
}
/// Cheap runtime recheck, including committed attributes at the pinned base.
/// # Errors
/// Rejects submodules, LFS and sparse checkout; infrastructure failures refuse.
pub fn check_supported(workspace: &Path, base: &str) -> Result<(), CheckoutError> {
    for name in [".gitmodules", ".lfsconfig"] {
        if workspace.join(name).exists()
            || !text(workspace, &["ls-tree", "--name-only", base, "--", name])?.is_empty()
        {
            return Err(CheckoutError::Unsupported);
        }
    }
    if fs::read(workspace.join(".gitattributes"))
        .is_ok_and(|v| v.windows(10).any(|w| w == b"filter=lfs"))
    {
        return Err(CheckoutError::Unsupported);
    }
    let attributes = run_bounded(
        OsStr::new("git"),
        workspace,
        &[
            OsStr::new("show"),
            OsStr::new(&format!("{base}:.gitattributes")),
        ],
        Duration::from_secs(30),
    )?;
    if attributes.stdout.windows(10).any(|w| w == b"filter=lfs") {
        return Err(CheckoutError::Unsupported);
    }
    let sparse = run_bounded(
        OsStr::new("git"),
        workspace,
        &[
            OsStr::new("config"),
            OsStr::new("--get"),
            OsStr::new("core.sparseCheckout"),
        ],
        Duration::from_secs(30),
    )?;
    if sparse.status.code() != Some(1) && !sparse.status.success() {
        return Err(CheckoutError::Mismatch);
    }
    if sparse.stdout.starts_with(b"true") {
        return Err(CheckoutError::Unsupported);
    }
    Ok(())
}
fn exact_commit(workspace: &Path, base: &str) -> Result<String, CheckoutError> {
    if !matches!(base.len(), 40 | 64)
        || !base
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(CheckoutError::Mismatch);
    }
    let oid = text(
        workspace,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{base}^{{commit}}"),
        ],
    )?;
    if oid != base {
        return Err(CheckoutError::Mismatch);
    }
    Ok(oid)
}
/// # Errors
/// Returns an error instead of guessing registrations when Git cannot be read.
pub fn registrations(workspace: &Path) -> Result<BTreeSet<PathBuf>, CheckoutError> {
    let raw = git(
        workspace,
        &[
            OsStr::new("worktree"),
            OsStr::new("list"),
            OsStr::new("--porcelain"),
        ],
    )?;
    raw.split(|b| *b == b'\n')
        .filter_map(|v| v.strip_prefix(b"worktree "))
        .map(|v| {
            std::str::from_utf8(v)
                .map(PathBuf::from)
                .map_err(|_| CheckoutError::Mismatch)
        })
        .collect()
}
/// Read-only proof of slot, root, repository, HEAD and admin round-trip binding.
/// # Errors
/// Refuses any missing/replaced/mismatched checkout without writing or repairing.
pub fn probe_checkout(
    workspace: &Path,
    state: &Path,
    task: TaskId,
    expected: &Path,
    base: Option<&str>,
) -> Result<CheckoutBinding, CheckoutError> {
    let paths = CheckoutPaths::new(state, task)?;
    paths.require_checkout(expected)?;
    let common = main_common_dir(workspace)?;
    if !paths.checkout.is_dir()
        || git_path(&paths.checkout, "--show-toplevel")? != paths.checkout
        || git_path(&paths.checkout, "--git-common-dir")? != common
    {
        return Err(CheckoutError::Mismatch);
    }
    let head = text(&paths.checkout, &["rev-parse", "HEAD"])?;
    if let Some(base) = base {
        exact_commit(workspace, base)?;
        if head != base {
            return Err(CheckoutError::Mismatch);
        }
    }
    let file = paths.checkout.join(".git");
    if fs::symlink_metadata(&file)
        .map_err(|_| CheckoutError::Mismatch)?
        .file_type()
        .is_symlink()
    {
        return Err(CheckoutError::Mismatch);
    }
    let raw = fs::read_to_string(&file).map_err(|_| CheckoutError::Mismatch)?;
    let admin = PathBuf::from(
        raw.strip_prefix("gitdir:")
            .ok_or(CheckoutError::Mismatch)?
            .trim(),
    );
    let admin = fs::canonicalize(if admin.is_absolute() {
        admin
    } else {
        paths.checkout.join(admin)
    })
    .map_err(|_| CheckoutError::Mismatch)?;
    if admin.parent() != Some(common.join("worktrees").as_path())
        || git_path(&paths.checkout, "--git-dir")? != admin
    {
        return Err(CheckoutError::Mismatch);
    }
    let back = fs::read_to_string(admin.join("gitdir")).map_err(|_| CheckoutError::Mismatch)?;
    if Path::new(back.trim()) != file || !registrations(workspace)?.contains(&paths.checkout) {
        return Err(CheckoutError::Mismatch);
    }
    Ok(CheckoutBinding {
        paths,
        base_head: head,
        common_dir: common,
        git_dir: admin,
    })
}
fn private_directory(path: &Path) -> Result<(), CheckoutError> {
    safe_directory(path)?;
    fs::create_dir_all(path).map_err(|_| CheckoutError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| CheckoutError::Io)?;
    }
    Ok(())
}
/// Creates only a detached checkout at the task's exact submit-time commit.
/// # Errors
/// Refuses unsupported repositories, occupied slots, symlinks and invalid bases.
pub fn create_checkout(
    workspace: &Path,
    state: &Path,
    task: TaskId,
    base: &str,
) -> Result<CheckoutBinding, CheckoutError> {
    let paths = CheckoutPaths::new(state, task)?;
    let workspace = fs::canonicalize(workspace).map_err(|_| CheckoutError::Io)?;
    main_common_dir(&workspace)?;
    exact_commit(&workspace, base)?;
    check_supported(&workspace, base)?;
    if paths.checkout.exists()
        || paths.checkout.starts_with(&workspace)
        || registrations(&workspace)?.contains(&paths.checkout)
    {
        return Err(CheckoutError::Mismatch);
    }
    private_directory(paths.task_dir.parent().ok_or(CheckoutError::Traversal)?)?;
    private_directory(&paths.task_dir)?;
    private_directory(&paths.runtime_dir)?;
    git(
        &workspace,
        &[
            OsStr::new("-c"),
            OsStr::new("core.hooksPath=/dev/null"),
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            paths.checkout.as_os_str(),
            OsStr::new(base),
        ],
    )?;
    probe_checkout(&workspace, state, task, &paths.checkout, Some(base))
}
/// Explicit destructive cleanup of only a proven task checkout. No global prune.
/// # Errors
/// Missing checkout with a live registration and foreign/swapped trees refuse.
pub fn remove_checkout(
    workspace: &Path,
    state: &Path,
    task: TaskId,
    expected: &Path,
) -> Result<(), CheckoutError> {
    let paths = CheckoutPaths::new(state, task)?;
    paths.require_checkout(expected)?;
    if !paths.checkout.exists() {
        if registrations(workspace)?.contains(&paths.checkout) {
            return Err(CheckoutError::Mismatch);
        }
        return Ok(());
    }
    probe_checkout(workspace, state, task, expected, None)?;
    git(
        workspace,
        &[
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            expected.as_os_str(),
        ],
    )?;
    if expected.exists() || registrations(workspace)?.contains(expected) {
        return Err(CheckoutError::Mismatch);
    }
    Ok(())
}
