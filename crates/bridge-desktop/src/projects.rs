use bridge_config::{
    load_config_with_state_root,
    migration::{atomic_write, safe_path},
    validate_config_text,
};
use bridge_storage::RustStateLayout;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    net::TcpListener,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::{Mutex, RwLock, atomic::AtomicBool},
    time::Duration,
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDraft {
    pub id: String,
    pub workspace: String,
    #[serde(default)]
    pub remote_execution: Option<bridge_config::remote::RemoteExecution>,
    pub opencode_url: String,
    pub mcp_url: Option<String>,
    #[serde(default)]
    pub opencode_model: Option<String>,
    #[serde(default)]
    pub opencode_controller_model: Option<String>,
    #[serde(default)]
    pub opencode_env_file: Option<String>,
    pub max_rounds: u64,
    pub execution_mode: String,
    pub delivery_mode: String,
    pub max_active_tasks: u64,
    pub allow_parallel_writers: bool,
    pub auto_approve_state_directory: bool,
    pub auto_approve_permissions: Vec<String>,
    pub auto_approve_external_directories: Vec<String>,
}
#[derive(Serialize)]
pub struct ProjectEndpoints {
    pub opencode_url: String,
    pub mcp_url: String,
}
impl ProjectDraft {
    fn from_project(p: &bridge_config::ProjectEntry) -> Self {
        Self {
            id: p.id().to_string(),
            remote_execution: p.remote_execution().cloned(),
            workspace: p.workspace().to_string_lossy().into(),
            opencode_url: p.opencode_endpoint().url(),
            mcp_url: p.mcp_endpoint().map(|v| v.url()),
            opencode_model: p
                .opencode_model()
                .map(|m| format!("{}/{}", m.provider(), m.model())),
            opencode_controller_model: p
                .opencode_controller_model()
                .map(|m| format!("{}/{}", m.provider(), m.model())),
            opencode_env_file: p
                .opencode_env_file()
                .map(|f| f.as_path().to_string_lossy().into_owned()),
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
    remove: bool,
}
pub struct ProjectService {
    pub(crate) activity: RwLock<bool>,
    pub(crate) closing: AtomicBool,
    pub(crate) workers: Mutex<Vec<bridge_runtime::process_tree::ProcessTree>>,
    pub config: PathBuf,
    pub state: PathBuf,
    pending: Mutex<HashMap<String, Pending>>,
    pub(crate) pending_automation: Mutex<HashMap<String, crate::automation::PendingPlan>>,
    pub(crate) pending_opencode: Mutex<HashMap<String, crate::opencode_config::PendingConfig>>,
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
            activity: RwLock::new(false),
            closing: AtomicBool::new(false),
            workers: Mutex::new(Vec::new()),
            config,
            state,
            pending: Mutex::new(HashMap::new()),
            pending_automation: Mutex::new(HashMap::new()),
            pending_opencode: Mutex::new(HashMap::new()),
        })
    }
    pub(crate) fn config_bytes(&self) -> Result<Vec<u8>, &'static str> {
        match fs::read(&self.config) {
            Ok(v) => Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(_) => Err("config unavailable"),
        }
    }
    pub(crate) fn config_view(&self) -> Result<bridge_config::Config, &'static str> {
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
    /// Suggests currently free ports without changing or reserving configuration.
    pub fn suggest_endpoints(&self) -> Result<ProjectEndpoints, &'static str> {
        let config = self.config_view()?;
        let mut used = BTreeSet::new();
        for project in config.projects().values() {
            used.insert(project.opencode_endpoint().port());
            if let Some(endpoint) = project.mcp_endpoint() {
                used.insert(endpoint.port());
            }
        }
        // Match the ranges used by CLI add-project, including ports belonging
        // to stopped projects and listeners outside AIbridge.
        let free_port = |start, end| {
            (start..=end)
                .find(|port| {
                    !used.contains(port) && TcpListener::bind(("127.0.0.1", *port)).is_ok()
                })
                .ok_or("Нет свободных портов для нового проекта")
        };
        let opencode = free_port(4101, 4199)?;
        let mcp = free_port(4201, 4299)?;
        Ok(ProjectEndpoints {
            opencode_url: format!("http://127.0.0.1:{opencode}"),
            mcp_url: format!("http://127.0.0.1:{mcp}/mcp"),
        })
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
        let _activity = self.activity_guard()?;
        let (p, l) = self.project(id)?;
        if let Some(settings) = p.remote_execution() {
            return bridge_automation::remote::rpc(settings, &json!({"op":"lifecycle","command":command})).map_err(|_| "Удалённый мост недоступен; проверьте SSH, agent-bridge и настройки проекта на втором ПК");
        }
        match command {
            "setup" => bridge_runtime::project::setup(&[(p, l)]).map_err(|error| match error {
                bridge_runtime::RuntimeError::Ownership =>
                    "setup failed: existing state is incomplete, incompatible or not owned by this project",
                bridge_runtime::RuntimeError::Credentials =>
                    "setup failed: credentials or OpenCode environment file unavailable; check private file permissions",
                bridge_runtime::RuntimeError::Binding =>
                    "setup failed: invalid state or credential path",
                bridge_runtime::RuntimeError::Io =>
                    "setup failed: unable to create or write state or credential files",
                _ => "setup failed",
            }),
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
                .map_err(|error| match error {
                    bridge_runtime::RuntimeError::OpenCode(issue) => issue.message(),
                    bridge_runtime::RuntimeError::ProjectPortBusy => "Порт сервера проекта занят; готовность OpenCode не подтверждена.",
                    bridge_runtime::RuntimeError::Spawn => "Не удалось запустить сервер проекта; проверьте наличие OpenCode и agent-bridge.",
                    _ => "start failed",
                })
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
        for (key, value) in [
            ("opencode_model", &draft.opencode_model),
            (
                "opencode_controller_model",
                &draft.opencode_controller_model,
            ),
            ("opencode_env_file", &draft.opencode_env_file),
        ] {
            if let Some(value) = value.as_ref().filter(|v| !v.is_empty()) {
                table[key] = toml_edit::value(value.as_str());
            } else if let Some(table) = table.as_table_mut() {
                table.remove(key);
            }
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
        if let Some(remote) = &draft.remote_execution {
            remote.validate().map_err(|e| e.message())?;
            let value = serde_json::to_value(remote).map_err(|_| "remote settings invalid")?;
            let mut settings = toml_edit::Table::new();
            for (key, value) in value.as_object().ok_or("remote settings invalid")? {
                settings[key] = match value {
                    Value::String(v) => toml_edit::value(v.as_str()),
                    Value::Number(v) => toml_edit::value(v.as_i64().ok_or("remote port invalid")?),
                    _ => return Err("remote settings invalid"),
                };
            }
            table["remote_execution"] = toml_edit::Item::Table(settings);
        } else if let Some(table) = table.as_table_mut() {
            table.remove("remote_execution");
        }
        let proposed = doc.to_string().into_bytes();
        let checked = validate_config_text(
            std::str::from_utf8(&proposed).map_err(|_| "config invalid")?,
            &self.config,
            Some(&self.state),
        )
        .map_err(|error| error.message())?;
        let p = checked.project(&draft.id).ok_or("project missing")?;
        let l = RustStateLayout::new(self.state.clone(), p.id().clone())
            .map_err(|_| "state invalid")?;
        bridge_runtime::project::validate(p, &l).map_err(|_| "state/workspace overlap")?;
        p.read_opencode_env()
            .map_err(|_| "env file missing, unsafe or invalid (required mode: 0600)")?;
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
                remove: false,
            },
        );
        Ok(
            json!({"review_id":id,"before":before,"after":draft,"credentials_changed":pending[&id].password.is_some()||pending[&id].token.is_some()}),
        )
    }
    /// Preview removal of a registration; user files and runtime history are retained.
    pub fn preview_remove(&self, project: &str) -> Result<Value, String> {
        safe_path(&self.config)?;
        let original = self.config_bytes()?;
        let text = std::str::from_utf8(&original).map_err(|_| "config encoding invalid")?;
        let current = validate_config_text(text, &self.config, Some(&self.state))
            .map_err(|e| e.to_string())?;
        let before =
            ProjectDraft::from_project(current.project(project).ok_or("project not configured")?);
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| "config invalid")?;
        let projects = doc["projects"]
            .as_table_mut()
            .ok_or("projects table missing")?;
        projects.remove(project);
        if projects.is_empty() {
            projects.set_implicit(false);
        }
        // toml_edit attaches a file header to the first project table. Keep that
        // header when removing the table, without retaining its project settings.
        let header: String = text
            .split_inclusive('\n')
            .take_while(|line| {
                let line = line.trim();
                line.is_empty() || line.starts_with('#')
            })
            .collect();
        let mut proposed = doc.to_string();
        if !header.is_empty() && !proposed.starts_with(&header) {
            proposed.insert_str(0, &header);
        }
        let proposed = proposed.into_bytes();
        validate_config_text(
            std::str::from_utf8(&proposed).map_err(|_| "config invalid")?,
            &self.config,
            Some(&self.state),
        )
        .map_err(|e| e.to_string())?;
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
                project: project.into(),
                password: None,
                token: None,
                remove: true,
            },
        );
        Ok(json!({"review_id":id,"before":before,"after":null}))
    }
    pub fn apply(&self, review_id: &str) -> Result<Value, String> {
        let mut pending = self.pending.lock().map_err(|_| "preview unavailable")?;
        let change = pending.get(review_id).ok_or("preview expired")?;
        safe_path(&self.config)?;
        let _config_lock = self.config_file_guard()?;
        if self.config_bytes()? != change.original {
            return Err("config changed; request a new preview".into());
        }
        let config = self.config_view()?;
        let proposed = validate_config_text(
            std::str::from_utf8(&change.proposed).map_err(|_| "config invalid")?,
            &self.config,
            Some(&self.state),
        )
        .map_err(|_| "config invalid")?;
        // A removal affects readers of the old registration, including linked
        // controllers and shared credentials. No surviving project table changes.
        let removal_layout = if change.remove {
            Some(self.project(&change.project)?.1)
        } else {
            None
        };
        let _automation = match removal_layout
            .as_ref()
            .filter(|layout| layout.database().exists())
        {
            Some(layout) => Some(
                bridge_automation::run::AutomationLock::acquire(layout)
                    .map_err(|e| e.to_string())?,
            ),
            None => None,
        };
        let _runtime = bridge_runtime::project::project_config_edit_guard(
            &config,
            if change.remove { &config } else { &proposed },
            &change.project,
            &self.state,
        )
        .map_err(|e| e.to_string())?;
        if let Some(layout) = &removal_layout {
            if proposed.project(&change.project).is_some() {
                return Err("project removal preview invalid".into());
            }
            if layout.database().exists() {
                let storage = layout.open_readonly().map_err(|_| "state unavailable")?;
                if storage
                    .count_tasks(layout.project_id(), true)
                    .map_err(|_| "task state unavailable")?
                    > 0
                {
                    return Err(
                        "Завершите или закройте незавершённые задачи проекта перед удалением."
                            .into(),
                    );
                }
                use bridge_storage::automation::{AutomationRunStore, AutomationStoreError};
                match AutomationRunStore::new(layout.clone()).load(None) {
                    Ok(run) if !run.status().is_terminal() => {
                        return Err(
                            "Остановите автоматический запуск проекта перед удалением.".into()
                        );
                    }
                    Ok(_) | Err(AutomationStoreError::NotFound) => {}
                    Err(_) => return Err("Состояние автоматизации недоступно".into()),
                }
            }
        }
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
        if change.remove {
            atomic_write(&self.config, &change.proposed, mode)?;
            let project = change.project.clone();
            pending.remove(review_id);
            return Ok(json!({"removed":true,"project":project}));
        }
        let p = proposed.project(&change.project).ok_or("project missing")?;
        p.read_opencode_env()
            .map_err(|_| "env file missing, unsafe or invalid (required mode: 0600)")?;
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
                    return Err("credential path inside workspace".into());
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
    pub(crate) fn config_file_guard(&self) -> Result<fs::File, &'static str> {
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
        Ok(handle)
    }
    pub fn cancel(&self, id: &str) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(id);
        }
    }
}
