//! Controller-only OpenCode configuration and foreground launch.
//! No executor credentials or project files are written by this module.
use crate::controller_permissions::{CONTROLLER_SUBAGENT_DEPTH, controller_agent_permission};
use bridge_config::ProjectEntry;
use bridge_storage::RustStateLayout;
use serde_json::{Value, json};
use std::os::unix::{
    fs::{OpenOptionsExt, PermissionsExt},
    io::AsRawFd,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path},
    process::{Command, ExitStatus},
    sync::atomic::{AtomicU64, Ordering},
};

pub const CONFIG_FILENAME: &str = "controller-opencode.json";
pub const CONTROLLER_AGENT: &str = "bridge-controller";
pub const MCP_TIMEOUT_MS: u32 = 330_000;
pub const CONTROLLER_PROMPT: &str = include_str!("controller_prompt.txt");
const PRIMARY_TOKEN: &str = "AGENT_BRIDGE_MCP_TOKEN";

/// Fixed labels deliberately exclude workspace contents, paths and credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerError {
    Binding,
    WorkspaceConfig,
    WorkspaceConflict,
    TokenCollision,
    Credentials,
    Permissions,
    State,
    Busy,
    Io,
    Spawn,
}
impl fmt::Display for ControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Binding => "controller_binding_invalid",
            Self::WorkspaceConfig => "controller_workspace_config_invalid",
            Self::WorkspaceConflict => "controller_workspace_config_conflict",
            Self::TokenCollision => "controller_token_env_collision",
            Self::Credentials => "controller_credentials_unavailable",
            Self::Permissions => "controller_permissions_invalid",
            Self::State => "controller_state_unowned",
            Self::Busy => "controller_already_running",
            Self::Io => "controller_io_error",
            Self::Spawn => "controller_spawn_failed",
        })
    }
}
impl std::error::Error for ControllerError {}
type Result<T> = std::result::Result<T, ControllerError>;

fn token_var(project: &ProjectEntry) -> String {
    format!(
        "AGENT_BRIDGE_MCP_TOKEN_{}",
        project.id().as_str().to_ascii_uppercase().replace('-', "_")
    )
}
fn server_name(project: &ProjectEntry) -> String {
    format!("agent_bridge_{}", project.id())
}
pub(crate) fn check_bindings(
    primary: &ProjectEntry,
    linked: &[&ProjectEntry],
    layout: &RustStateLayout,
) -> Result<()> {
    let root = layout.state_root();
    if layout.project_id() != primary.id()
        || !root.is_absolute()
        || root.to_str().is_none()
        || root.parent().is_none()
        || root.components().any(|c| matches!(c, Component::ParentDir))
    {
        return Err(ControllerError::Binding);
    }
    // Refuse aliases and workspace-local state, including an existing symlink
    // in an ancestor of a not-yet-created root. This is a Rust isolation guard.
    let mut ancestor = std::path::PathBuf::new();
    for component in layout.project_dir().components() {
        ancestor.push(component);
        match fs::symlink_metadata(&ancestor) {
            Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
                return Err(ControllerError::Binding);
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(ControllerError::Binding),
        }
    }
    for path in [layout.marker(), layout.database()] {
        match fs::symlink_metadata(path) {
            Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
                return Err(ControllerError::State);
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(ControllerError::State),
        }
    }
    let mut ids = BTreeSet::new();
    let mut vars = BTreeSet::new();
    for project in std::iter::once(primary).chain(linked.iter().copied()) {
        if root.starts_with(project.workspace())
            || project.workspace().starts_with(root)
            || !ids.insert(project.id().as_str())
        {
            return Err(ControllerError::Binding);
        }
        if project.id() != primary.id()
            && project.mcp_endpoint().is_some()
            && !vars.insert(token_var(project))
        {
            return Err(ControllerError::TokenCollision);
        }
    }
    Ok(())
}

/// Read-only generator supporting the frozen HTTP and stdio wiring.
/// # Errors
/// Rejects unsafe bindings, colliding token variables and unsafe permissions.
pub fn build_controller_config(
    primary: &ProjectEntry,
    linked: &[&ProjectEntry],
    layout: &RustStateLayout,
    bridge_exe: &Path,
    config_path: &Path,
) -> Result<Value> {
    check_bindings(primary, linked, layout)?;
    if !bridge_exe.is_absolute()
        || !config_path.is_absolute()
        || bridge_exe.to_str().is_none()
        || config_path.to_str().is_none()
    {
        return Err(ControllerError::Binding);
    }
    let mut mcp = serde_json::Map::new();
    for (index, project) in std::iter::once(primary)
        .chain(linked.iter().copied())
        .enumerate()
    {
        let name = if index == 0 {
            "agent_bridge".to_owned()
        } else {
            server_name(project)
        };
        let entry = if let Some(endpoint) = project.mcp_endpoint() {
            let var = if index == 0 {
                PRIMARY_TOKEN.to_owned()
            } else {
                token_var(project)
            };
            json!({"type":"remote", "url":endpoint.url(), "enabled":true, "oauth":false,
                "headers":{"Authorization":format!("Bearer {{env:{var}}}")}, "timeout":MCP_TIMEOUT_MS})
        } else {
            json!({"type":"local", "command":[bridge_exe, "mcp", "--project",project.id().as_str(),
                "--config",config_path,"--state-root",layout.state_root()], "enabled":true,"timeout":MCP_TIMEOUT_MS})
        };
        mcp.insert(name, entry);
    }
    let permissions = controller_agent_permission(primary, layout.state_root())
        .map_err(|_| ControllerError::Permissions)?;
    Ok(
        json!({"$schema":"https://opencode.ai/config.json", "mcp":mcp,
        "default_agent":CONTROLLER_AGENT,"subagent_depth":CONTROLLER_SUBAGENT_DEPTH,
        "agent":{CONTROLLER_AGENT:{"description":"agent-bridge controller: plans, submits and reviews delegated tasks; never edits code itself",
            "mode":"primary","prompt":CONTROLLER_PROMPT,"permission":permissions}}}),
    )
}

/// Parses comments and trailing commas without changing quoted strings.
/// # Errors
/// Broken comments, strings and JSON are rejected with a fixed safe label.
pub fn parse_jsonc(text: &str) -> Result<Value> {
    let bytes = text.as_bytes();
    let mut clean = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                clean.push(bytes[i]);
                i += 1;
                let mut closed = false;
                while i < bytes.len() {
                    let c = bytes[i];
                    clean.push(c);
                    i += 1;
                    if c == b'\\' {
                        if i == bytes.len() {
                            return Err(ControllerError::WorkspaceConfig);
                        }
                        clean.push(bytes[i]);
                        i += 1;
                    } else if c == b'"' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return Err(ControllerError::WorkspaceConfig);
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                clean.push(b' ');
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                clean.push(b' ');
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 >= bytes.len() {
                    return Err(ControllerError::WorkspaceConfig);
                }
                i += 2;
            }
            c => {
                clean.push(c);
                i += 1;
            }
        }
    }
    let mut quoted = false;
    let mut escaped = false;
    for i in 0..clean.len() {
        let c = clean[i];
        if quoted {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                quoted = false;
            }
        } else if c == b'"' {
            quoted = true;
        } else if c == b',' {
            let next = clean[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
            if matches!(next, Some(b'}' | b']')) {
                clean[i] = b' ';
            }
        }
    }
    serde_json::from_slice(&clean).map_err(|_| ControllerError::WorkspaceConfig)
}

/// Validates both project config files before reading tokens or creating state.
/// # Errors
/// Refuses malformed files and controller-owned names/keys, even null values.
pub fn check_workspace_config(primary: &ProjectEntry, linked: &[&ProjectEntry]) -> Result<()> {
    let mut reserved = BTreeSet::from(["agent_bridge".to_owned()]);
    reserved.extend(linked.iter().map(|p| server_name(p)));
    for name in ["opencode.json", "opencode.jsonc"] {
        let text = match fs::read_to_string(primary.workspace().join(name)) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is unreadable configuration, not absence.
                if fs::symlink_metadata(primary.workspace().join(name)).is_ok() {
                    return Err(ControllerError::WorkspaceConfig);
                }
                continue;
            }
            Err(_) => return Err(ControllerError::WorkspaceConfig),
        };
        let data = parse_jsonc(&text)?;
        let object = data.as_object().ok_or(ControllerError::WorkspaceConfig)?;
        if object.contains_key("default_agent") || object.contains_key("subagent_depth") {
            return Err(ControllerError::WorkspaceConflict);
        }
        for (key, names) in [
            ("mcp", &reserved),
            ("agent", &BTreeSet::from([CONTROLLER_AGENT.to_owned()])),
        ] {
            if let Some(value) = object.get(key).filter(|v| !v.is_null()) {
                let table = value.as_object().ok_or(ControllerError::WorkspaceConfig)?;
                if names.iter().any(|name| table.contains_key(name)) {
                    return Err(ControllerError::WorkspaceConflict);
                }
            }
        }
    }
    Ok(())
}

/// Trusted executable override for offline integration tests; production uses PATH.
pub struct ControllerCommand {
    program: OsString,
}
impl ControllerCommand {
    pub fn opencode() -> Self {
        Self {
            program: "opencode".into(),
        }
    }
    /// # Errors
    /// A fixture override must be an absolute existing regular file.
    pub fn executable(path: &Path) -> Result<Self> {
        if !path.is_absolute() || !path.is_file() {
            return Err(ControllerError::Spawn);
        }
        Ok(Self {
            program: path.as_os_str().to_owned(),
        })
    }
}

pub(crate) fn controller_env(
    primary: &ProjectEntry,
    linked: &[&ProjectEntry],
    config: Option<&Path>,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<BTreeMap<OsString, OsString>> {
    let mut env: BTreeMap<_, _> = inherited.into_iter().collect();
    env.retain(|name, _| {
        !name.to_str().is_some_and(|s| {
            s == "AGENT_BRIDGE_MCP_TOKEN" || s.starts_with("AGENT_BRIDGE_MCP_TOKEN_")
        })
    });
    if let Some(config) = config {
        env.insert("OPENCODE_CONFIG".into(), config.as_os_str().to_owned());
    }
    env.remove(OsStr::new("OPENCODE_SERVER_PASSWORD"));
    env.remove(OsStr::new("OPENCODE_SERVER_USERNAME"));
    for (index, project) in std::iter::once(primary)
        .chain(linked.iter().copied())
        .enumerate()
    {
        if project.mcp_endpoint().is_none() {
            continue;
        }
        let token = project
            .read_mcp_token()
            .map_err(|_| ControllerError::Credentials)?
            .ok_or(ControllerError::Credentials)?;
        if token.expose_secret().contains('\0') {
            return Err(ControllerError::Credentials);
        }
        let var = if index == 0 {
            PRIMARY_TOKEN.to_owned()
        } else {
            token_var(project)
        };
        env.insert(var.into(), token.expose_secret().into());
    }
    for key in ["NO_PROXY", "no_proxy"] {
        let existing = env.get(OsStr::new(key)).cloned().unwrap_or_default();
        let text = existing.to_str().ok_or(ControllerError::Binding)?;
        let mut additions = Vec::new();
        for host in ["127.0.0.1", "localhost"] {
            if !text.split(',').any(|part| part == host) {
                additions.push(host);
            }
        }
        if !additions.is_empty() {
            let mut value = existing;
            for host in additions {
                if !value.is_empty() {
                    value.push(",");
                }
                value.push(host);
            }
            env.insert(key.into(), value);
        }
    }
    Ok(env)
}

pub(crate) fn lock_controller(layout: &RustStateLayout) -> Result<File> {
    layout.initialize().map_err(|_| ControllerError::State)?;
    layout.open().map_err(|_| ControllerError::State)?;
    let dir = layout.project_dir();
    if fs::symlink_metadata(&dir)
        .map_err(|_| ControllerError::Io)?
        .file_type()
        .is_symlink()
    {
        return Err(ControllerError::State);
    }
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|_| ControllerError::Io)?;
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(dir.join("controller.lock"))
        .map_err(|_| ControllerError::Io)?;
    if !guard.metadata().map_err(|_| ControllerError::Io)?.is_file() {
        return Err(ControllerError::Io);
    }
    nix::fcntl::flock(
        guard.as_raw_fd(),
        nix::fcntl::FlockArg::LockExclusiveNonblock,
    )
    .map_err(|e| {
        if e == nix::errno::Errno::EWOULDBLOCK {
            ControllerError::Busy
        } else {
            ControllerError::Io
        }
    })?;
    Ok(guard)
}

fn write_config(layout: &RustStateLayout, payload: &Value) -> Result<(std::path::PathBuf, File)> {
    let guard = lock_controller(layout)?;
    let dir = layout.project_dir();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp = dir.join(format!(
        ".controller-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let path = dir.join(CONFIG_FILENAME);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(&temp)
        .map_err(|_| ControllerError::Io)?;
    let result = (|| {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| ControllerError::Io)?;
        serde_json::to_writer_pretty(&mut file, payload).map_err(|_| ControllerError::Io)?;
        file.write_all(b"\n").map_err(|_| ControllerError::Io)?;
        file.sync_all().map_err(|_| ControllerError::Io)?;
        fs::rename(&temp, &path).map_err(|_| ControllerError::Io)?;
        File::open(&dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| ControllerError::Io)?;
        Ok((path, guard))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Launches only controller TUI, in the project workspace with inherited stdio.
/// HTTP servers must already be running; this does not start executor or MCP.
/// # Errors
/// All config/token checks fail before state writes. Local transports launch the Rust MCP command.
/// Foreign Rust/Python state is never adopted. Runtime failures use safe labels.
pub fn launch_controller(
    primary: &ProjectEntry,
    linked: &[&ProjectEntry],
    layout: &RustStateLayout,
    bridge_exe: &Path,
    config_path: &Path,
    executable: &ControllerCommand,
) -> Result<ExitStatus> {
    let payload = build_controller_config(primary, linked, layout, bridge_exe, config_path)?;
    check_workspace_config(primary, linked)?;
    let path = layout.project_dir().join(CONFIG_FILENAME);
    let env = controller_env(primary, linked, Some(&path), std::env::vars_os())?;
    let (_, _guard) = write_config(layout, &payload)?;
    Command::new(&executable.program)
        .current_dir(primary.workspace())
        .env_clear()
        .envs(env)
        .status()
        .map_err(|_| ControllerError::Spawn)
}
