use bridge_config::{
    load_config_with_state_root,
    migration::{atomic_write, safe_path},
    validate_config_text,
};
use bridge_storage::RustStateLayout;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDraft {
    pub id: String,
    pub workspace: String,
    pub opencode_url: String,
    pub mcp_url: Option<String>,
    pub max_rounds: u64,
    pub execution_mode: String,
    pub delivery_mode: String,
    pub max_active_tasks: u64,
    pub allow_parallel_writers: bool,
    pub auto_approve_state_directory: bool,
    pub auto_approve_permissions: Vec<String>,
    pub auto_approve_external_directories: Vec<String>,
}
impl ProjectDraft {
    fn from_project(p: &bridge_config::ProjectEntry) -> Self {
        Self {
            id: p.id().to_string(),
            workspace: p.workspace().to_string_lossy().into(),
            opencode_url: p.opencode_endpoint().url(),
            mcp_url: p.mcp_endpoint().map(|v| v.url()),
            max_rounds: p.max_rounds(),
            execution_mode: if p.execution_mode() == bridge_domain::ExecutionMode::Worktree {
                "worktree"
            } else {
                "direct"
            }
            .into(),
            delivery_mode: if p.delivery_mode() == bridge_domain::DeliveryMode::OnAccept {
                "on_accept"
            } else {
                "manual"
            }
            .into(),
            max_active_tasks: p.max_active_tasks(),
            allow_parallel_writers: p.allow_parallel_writers(),
            auto_approve_state_directory: p.auto_approve_state_directory(),
            auto_approve_permissions: p.auto_approve_permissions().to_vec(),
            auto_approve_external_directories: p
                .auto_approve_external_directories()
                .iter()
                .map(|p| p.to_string_lossy().into())
                .collect(),
        }
    }
}
struct Pending {
    original: Vec<u8>,
    proposed: Vec<u8>,
    project: String,
    password: Option<String>,
    token: Option<String>,
}
pub struct ProjectService {
    pub config: PathBuf,
    pub state: PathBuf,
    pending: Mutex<HashMap<String, Pending>>,
}
impl ProjectService {
    pub fn new(config: PathBuf, state: PathBuf) -> Result<Self, &'static str> {
        safe_path(&config)?;
        // Opening a fresh settings window is read-only: an absent file is an
        // empty proposed configuration until the first explicit save.
        if config.exists() {
            load_config_with_state_root(&config, &state).map_err(|_| "config invalid")?;
        } else {
            validate_config_text("[projects]\n", &config, Some(&state))
                .map_err(|_| "config invalid")?;
        }
        Ok(Self {
            config,
            state,
            pending: Mutex::new(HashMap::new()),
        })
    }
    fn config_bytes(&self) -> Result<Vec<u8>, &'static str> {
        match fs::read(&self.config) {
            Ok(v) => Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(_) => Err("config unavailable"),
        }
    }
    fn config_view(&self) -> Result<bridge_config::Config, &'static str> {
        if self.config.exists() {
            load_config_with_state_root(&self.config, &self.state).map_err(|_| "config invalid")
        } else {
            validate_config_text("[projects]\n", &self.config, Some(&self.state))
                .map_err(|_| "config invalid")
        }
    }
    pub fn projects(&self) -> Result<Vec<ProjectDraft>, &'static str> {
        Ok(self
            .config_view()?
            .projects()
            .values()
            .map(ProjectDraft::from_project)
            .collect())
    }
    pub fn project(
        &self,
        id: &str,
    ) -> Result<(bridge_config::ProjectEntry, RustStateLayout), &'static str> {
        let config = self.config_view()?;
        let p = config.project(id).ok_or("project not configured")?.clone();
        let l = RustStateLayout::new(self.state.clone(), p.id().clone())
            .map_err(|_| "state invalid")?;
        bridge_runtime::project::validate(&p, &l).map_err(|_| "state binding invalid")?;
        Ok((p, l))
    }
    pub fn lifecycle(&self, id: &str, command: &str) -> Result<Value, &'static str> {
        let (p, l) = self.project(id)?;
        match command {
            "setup" => bridge_runtime::project::setup(&[(p, l)]).map_err(|_| "setup failed"),
            "doctor" => Ok(bridge_runtime::diagnostics::status(
                &p,
                &l,
                Duration::from_millis(300),
            )),
            "start" => {
                let executable = std::env::var_os("AIBRIDGE_CLI")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("../../target/debug/agent-bridge")
                    });
                if !executable.is_absolute() || !executable.is_file() {
                    return Err("agent-bridge executable unavailable");
                }
                bridge_runtime::project::start(
                    &[(p, l)],
                    &bridge_runtime::ServerCommand::opencode(),
                    &executable,
                    Duration::from_secs(20),
                )
                .map_err(|_| "start failed")
            }
            "stop" => bridge_runtime::project::stop(&[(p, l)]).map_err(|_| "stop failed"),
            _ => Err("unsupported lifecycle action"),
        }
    }
    pub fn preview(
        &self,
        draft: ProjectDraft,
        password: Option<String>,
        token: Option<String>,
    ) -> Result<Value, &'static str> {
        draft
            .id
            .parse::<bridge_domain::ProjectId>()
            .map_err(|_| "invalid project id")?;
        if password.as_ref().is_some_and(|s| {
            s.trim().is_empty() || s.len() > 8192 || s.contains(['\n', '\r', '\0'])
        }) || token.as_ref().is_some_and(|s| {
            s.trim().is_empty() || s.len() > 8192 || s.contains(['\n', '\r', '\0'])
        }) {
            return Err("invalid credential value");
        }
        safe_path(&self.config)?;
        let original = self.config_bytes()?;
        let text = if original.is_empty() {
            "[projects]\n"
        } else {
            std::str::from_utf8(&original).map_err(|_| "config encoding invalid")?
        };
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| "config invalid")?;
        let table = &mut doc["projects"][&draft.id];
        if table.is_none() {
            *table = toml_edit::Item::Table(toml_edit::Table::new());
        }
        for (key, value) in [
            ("workspace", draft.workspace.as_str()),
            ("opencode_url", draft.opencode_url.as_str()),
            ("execution_mode", draft.execution_mode.as_str()),
            ("delivery_mode", draft.delivery_mode.as_str()),
        ] {
            table[key] = toml_edit::value(value);
        }
        for (key, n) in [
            ("max_rounds", draft.max_rounds),
            ("max_active_tasks", draft.max_active_tasks),
        ] {
            table[key] = toml_edit::value(i64::try_from(n).map_err(|_| "numeric option invalid")?);
        }
        for (key, b) in [
            ("allow_parallel_writers", draft.allow_parallel_writers),
            (
                "auto_approve_state_directory",
                draft.auto_approve_state_directory,
            ),
        ] {
            table[key] = toml_edit::value(b);
        }
        for (key, values) in [
            ("auto_approve_permissions", &draft.auto_approve_permissions),
            (
                "auto_approve_external_directories",
                &draft.auto_approve_external_directories,
            ),
        ] {
            let mut array = toml_edit::Array::new();
            for s in values {
                array.push(s.as_str());
            }
            table[key] = toml_edit::value(array);
        }
        if table.get("password_file").is_none_or(|i| i.is_none()) {
            table["password_file"] = toml_edit::value(format!("secrets/{}.password", draft.id));
        }
        if let Some(url) = &draft.mcp_url {
            table["mcp_url"] = toml_edit::value(url.as_str());
            if table.get("mcp_token_file").is_none_or(|i| i.is_none()) {
                table["mcp_token_file"] =
                    toml_edit::value(format!("secrets/{}.mcp-token", draft.id));
            }
        } else if let Some(t) = table.as_table_mut() {
            t.remove("mcp_url");
            t.remove("mcp_token_file");
        }
        let proposed = doc.to_string().into_bytes();
        let checked = validate_config_text(
            std::str::from_utf8(&proposed).map_err(|_| "config invalid")?,
            &self.config,
            Some(&self.state),
        )
        .map_err(|_| "project configuration invalid or conflicts with another project")?;
        let p = checked.project(&draft.id).ok_or("project missing")?;
        let l = RustStateLayout::new(self.state.clone(), p.id().clone())
            .map_err(|_| "state invalid")?;
        bridge_runtime::project::validate(p, &l).map_err(|_| "state/workspace overlap")?;
        let existing = self.config_view()?;
        let before = existing.project(&draft.id).map(ProjectDraft::from_project);
        let id = uuid::Uuid::new_v4().to_string();
        let mut pending = self.pending.lock().map_err(|_| "preview unavailable")?;
        if pending.len() >= 16 {
            pending.clear();
        }
        pending.insert(
            id.clone(),
            Pending {
                original,
                proposed,
                project: draft.id.clone(),
                password,
                token,
            },
        );
        Ok(
            json!({"review_id":id,"before":before,"after":draft,"credentials_changed":pending[&id].password.is_some()||pending[&id].token.is_some()}),
        )
    }
    pub fn apply(&self, review_id: &str) -> Result<Value, &'static str> {
        let mut pending = self.pending.lock().map_err(|_| "preview unavailable")?;
        let change = pending.get(review_id).ok_or("preview expired")?;
        safe_path(&self.config)?;
        use std::os::fd::AsRawFd;
        let parent = self.config.parent().ok_or("config directory required")?;
        fs::create_dir_all(parent).map_err(|_| "config directory unavailable")?;
        let handle = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(parent.join(".agent-bridge-config.lock"))
            .map_err(|_| "config lock unavailable")?;
        nix::fcntl::flock(
            handle.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        )
        .map_err(|_| "config lock busy")?;
        if self.config_bytes()? != change.original {
            return Err("config changed; request a new preview");
        }
        let config = self.config_view()?;
        let _runtime = bridge_runtime::project::config_edit_guard(&config, &self.state)
            .map_err(|_| "stop tasks, services and controllers before changing configuration")?;
        let mode = fs::metadata(&self.config)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        let backup = PathBuf::from(format!(
            "{}.{}.bak",
            self.config.to_string_lossy(),
            uuid::Uuid::new_v4()
        ));
        if !change.original.is_empty() {
            atomic_write(&backup, &change.original, mode)?;
        }
        let proposed = validate_config_text(
            std::str::from_utf8(&change.proposed).map_err(|_| "config invalid")?,
            &self.config,
            Some(&self.state),
        )
        .map_err(|_| "config invalid")?;
        let p = proposed.project(&change.project).ok_or("project missing")?;
        for (path, value) in [
            (
                p.password_file().map(|p| p.as_path()),
                change.password.as_ref(),
            ),
            (
                p.mcp_token_file().map(|p| p.as_path()),
                change.token.as_ref(),
            ),
        ] {
            if let (Some(path), Some(_value)) = (path, value) {
                safe_path(path)?;
                if path.starts_with(p.workspace()) {
                    return Err("credential path inside workspace");
                }
            }
        }
        atomic_write(&self.config, &change.proposed, mode)?;
        for (path, value) in [
            (
                p.password_file().map(|p| p.as_path()),
                change.password.as_ref(),
            ),
            (
                p.mcp_token_file().map(|p| p.as_path()),
                change.token.as_ref(),
            ),
        ] {
            if let (Some(path), Some(value)) = (path, value) {
                atomic_write(path, format!("{value}\n").as_bytes(), 0o600)
                    .map_err(|_| "config committed; credential write failed")?;
            }
        }
        let project = change.project.clone();
        pending.remove(review_id);
        Ok(json!({"saved":true,"project":project}))
    }
    pub fn cancel(&self, id: &str) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(id);
        }
    }
}
