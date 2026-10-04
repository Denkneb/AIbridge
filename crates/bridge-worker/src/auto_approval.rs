//! Permission decisions; filesystem state is read, never modified.
use bridge_config::ProjectEntry;
use serde_json::Value;
use std::{
    collections::VecDeque,
    ffi::OsString,
    fmt, fs,
    path::{Component, Path, PathBuf},
};

/// Content-free reason with an explicitly accessible, redacted diagnostic detail.
#[derive(Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    approved: bool,
    reason: &'static str,
    detail: Option<String>,
}
impl PermissionDecision {
    #[must_use]
    pub const fn approved(&self) -> bool {
        self.approved
    }
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        self.reason
    }
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
    fn new(approved: bool, reason: &'static str) -> Self {
        Self {
            approved,
            reason,
            detail: None,
        }
    }
    fn denied(reason: &'static str, detail: &str) -> Self {
        Self {
            approved: false,
            reason,
            detail: Some(detail.chars().take(200).collect()),
        }
    }
}
impl fmt::Debug for PermissionDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermissionDecision")
            .field("approved", &self.approved)
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

/// Uses opt-in state only for permission access. Project Git roots remain intact.
/// The raw boundary also supports independently frozen malformed-request fixtures.
#[must_use]
pub fn permission_decision(
    project: &ProjectEntry,
    state_root: &Path,
    permission: &Value,
) -> PermissionDecision {
    let decision = PermissionDecision::new;
    if permission
        .get("id")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return decision(false, "missing_request_id");
    }
    let Some(name) = permission
        .get("permission")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    else {
        return decision(false, "missing_permission");
    };
    let Some(patterns) = permission
        .get("patterns")
        .and_then(Value::as_array)
        .filter(|items| items.iter().all(Value::is_string))
    else {
        return decision(false, "malformed_patterns");
    };
    if name == "external_directory" {
        let mut roots = project.auto_approve_external_directories().to_vec();
        if project.auto_approve_state_directory() {
            if bridge_config::state_directory_permission_pattern(state_root).is_err() {
                return decision(false, "external_target_out_of_root");
            }
            roots.push(state_root.to_path_buf());
        }
        if roots.is_empty() {
            return decision(false, "external_directory_not_configured");
        }
        let mut targets: Vec<&str> = patterns.iter().filter_map(Value::as_str).collect();
        if let Some(directories) = permission
            .get("metadata")
            .and_then(Value::as_object)
            .and_then(|object| object.get("directories"))
            .filter(|value| !value.is_null())
        {
            let Some(directories) = directories
                .as_array()
                .filter(|items| items.iter().all(Value::is_string))
            else {
                return decision(false, "malformed_external_targets");
            };
            targets.extend(directories.iter().filter_map(Value::as_str));
        }
        if targets.is_empty() {
            return decision(false, "missing_external_targets");
        }
        let resolved: Vec<PathBuf> = roots
            .iter()
            .filter_map(|root| fs::canonicalize(root).ok())
            .filter(|root| root.parent().is_some())
            .collect();
        for target in targets {
            if !external_target_allowed(target, &roots, &resolved) {
                return PermissionDecision::denied("external_target_out_of_root", target);
            }
        }
        return decision(true, "external_directory_trusted");
    }
    if !project
        .auto_approve_permissions()
        .iter()
        .any(|configured| configured == name)
    {
        return decision(false, "not_configured");
    }
    if !matches!(
        name,
        "bash"
            | "read"
            | "edit"
            | "glob"
            | "grep"
            | "webfetch"
            | "websearch"
            | "task"
            | "todowrite"
            | "lsp"
            | "skill"
    ) {
        return decision(false, "unknown_permission");
    }
    if name == "bash" {
        if patterns.is_empty() {
            return decision(false, "missing_bash_patterns");
        }
        for pattern in patterns.iter().filter_map(Value::as_str) {
            if let Some(problem) = bridge_command_policy::bash_pattern_problem(pattern) {
                return PermissionDecision::denied(problem.as_str(), pattern);
            }
        }
    }
    decision(true, "configured")
}
fn has_glob(text: &str) -> bool {
    text.contains(['*', '?', '['])
}
fn within(path: &Path, roots: &[PathBuf]) -> bool {
    path.parent().is_some() && roots.iter().any(|root| path.starts_with(root))
}
fn literal(target: &str, lexical_roots: &[PathBuf], resolved_roots: &[PathBuf]) -> Option<PathBuf> {
    let path = Path::new(target);
    if !path.is_absolute()
        || target.split('/').any(|part| part == "..")
        || !within(path, lexical_roots)
    {
        return None;
    }
    let resolved = fs::canonicalize(path).ok()?;
    within(&resolved, resolved_roots).then_some(resolved)
}
fn external_target_allowed(
    target: &str,
    lexical_roots: &[PathBuf],
    resolved_roots: &[PathBuf],
) -> bool {
    if target.is_empty() || target.contains('\0') {
        return false;
    }
    if !has_glob(target) {
        return literal(target, lexical_roots, resolved_roots).is_some();
    }
    let Some(prefix) = target
        .strip_suffix("/*")
        .filter(|prefix| !prefix.is_empty() && !has_glob(prefix))
    else {
        return false;
    };
    let Some(base) = literal(prefix, lexical_roots, resolved_roots).filter(|base| base.is_dir())
    else {
        return false;
    };
    let Ok(entries) = fs::read_dir(base) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let Some(resolved) = resolve_allow_missing(&entry.path()) else {
            return false;
        };
        if !within(&resolved, resolved_roots) {
            return false;
        }
    }
    true
}
// Like resolve(strict=False) for direct glob children: a dangling link may
// resolve within a root; loops, escaping links and unreadable components fail.
fn resolve_allow_missing(path: &Path) -> Option<PathBuf> {
    let mut pending: VecDeque<OsString> = path
        .components()
        .map(|part| part.as_os_str().to_owned())
        .collect();
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        match Path::new(&part).components().next()? {
            Component::RootDir => resolved.push(&part),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Prefix(_) => return None,
            Component::Normal(_) => {
                resolved.push(&part);
                match fs::symlink_metadata(&resolved) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        links += 1;
                        if links > 40 {
                            return None;
                        }
                        let target = fs::read_link(&resolved).ok()?;
                        resolved.pop();
                        if target.is_absolute() {
                            resolved.clear();
                        }
                        for part in target.components().rev() {
                            pending.push_front(part.as_os_str().to_owned());
                        }
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return None,
                }
            }
        }
    }
    Some(resolved)
}
