//! MCP transports and standalone delegated task adapters.
//! Delegation requires an injected worker launcher; manual delivery only.
mod errors;
pub mod http;
pub mod protocol;
mod status;
pub mod stdio;
mod tasks;
mod tools;
pub type WorkerSpawner =
    std::sync::Arc<dyn Fn(&bridge_storage::RoundRef) -> std::result::Result<(), ()> + Send + Sync>;
use bridge_config::{DEFAULT_PROFILE_ID, ProjectEntry};
use bridge_storage::{RuntimeLock, RustStateLayout};
use serde_json::{Value, json};
use std::os::unix::{
    fs::{OpenOptionsExt, PermissionsExt},
    io::AsRawFd,
};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    path::{Component, Path},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpError {
    Binding,
    State,
    Busy,
    Io,
    FrameTooLarge,
    Credentials,
    Endpoint,
}
impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Binding => "mcp_binding_invalid",
            Self::State => "mcp_state_unavailable",
            Self::Busy => "mcp_already_running",
            Self::Io => "mcp_io_error",
            Self::FrameTooLarge => "mcp_frame_too_large",
            Self::Credentials => "mcp_credentials_unavailable",
            Self::Endpoint => "mcp_http_endpoint_unavailable",
        })
    }
}
impl std::error::Error for McpError {}
pub type Result<T> = std::result::Result<T, McpError>;

/// Immutable project binding. The process holds the project MCP flock until drop.
pub struct McpServer {
    project: ProjectEntry,
    layout: RustStateLayout,
    _lock: File,
    spawner: Option<WorkerSpawner>,
    registry: Vec<ProjectEntry>,
}
impl McpServer {
    /// Initializes only a proven Rust-owned namespace and claims its MCP lock.
    /// # Errors
    /// Foreign state, aliased paths, workspace-local roots and a second MCP
    /// process fail closed. Existing tasks/reservations are never recovered.
    pub fn open(project: ProjectEntry, layout: RustStateLayout) -> Result<Self> {
        check_layout(&project, &layout)?;
        layout.initialize().map_err(|_| McpError::State)?;
        layout.open().map_err(|_| McpError::State)?;
        fs::set_permissions(layout.project_dir(), fs::Permissions::from_mode(0o700))
            .map_err(|_| McpError::Io)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(layout.lock(RuntimeLock::Mcp))
            .map_err(|_| McpError::Io)?;
        if !lock.metadata().map_err(|_| McpError::Io)?.is_file() {
            return Err(McpError::Io);
        }
        nix::fcntl::flock(
            lock.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        )
        .map_err(|e| {
            if e == nix::errno::Errno::EWOULDBLOCK {
                McpError::Busy
            } else {
                McpError::Io
            }
        })?;
        lock.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| McpError::Io)?;
        Ok(Self {
            registry: vec![project.clone()],
            project,
            layout,
            _lock: lock,
            spawner: None,
        })
    }
    /// Enable delegated tools only with a trusted worker launcher. Embedders
    /// without one retain the read-only foundation surface.
    pub fn with_workers(
        mut self,
        spawner: WorkerSpawner,
        registry: Vec<ProjectEntry>,
    ) -> Result<Self> {
        if !registry
            .iter()
            .any(|p| p.id() == self.project.id() && p.workspace() == self.project.workspace())
        {
            return Err(McpError::Binding);
        }
        self.spawner = Some(spawner);
        self.registry = registry;
        Ok(self)
    }
    pub fn delegated_tools_enabled(&self) -> bool {
        self.spawner.is_some()
    }
    /// Recovery runs beside the transport, while the MCP ownership lock remains
    /// held. Shutdown drains recovery before releasing that lock.
    pub(crate) fn with_startup_recovery<T>(&self, run: impl FnOnce() -> Result<T>) -> Result<T> {
        std::thread::scope(|scope| {
            if self.delegated_tools_enabled() {
                std::thread::Builder::new()
                    .name("bridge-mcp-recovery".into())
                    .spawn_scoped(scope, || {
                        if self.recover_startup().is_err() {
                            eprintln!("warning: mcp_startup_recovery_failed");
                        }
                    })
                    .map_err(|_| McpError::Io)?;
            }
            run()
        })
    }
    /// Reads config labels and one coherent active-set snapshot; no activation.
    /// # Errors
    /// Ownership/schema/corrupt activity errors are fixed, content-free labels.
    pub fn project_info(&self) -> Result<Value> {
        check_layout(&self.project, &self.layout)?;
        let storage = self.layout.open().map_err(|_| McpError::State)?;
        let active = storage
            .active_set(self.project.id())
            .map_err(|_| McpError::State)?;
        let task = active.tasks.first();
        let writer_ids: Vec<_> = active
            .reservations
            .iter()
            .map(|w| w.task_id.to_string())
            .collect();
        let profiles: Vec<_> = self
            .project
            .profile_definitions()
            .values()
            .map(|p| json!({"id":p.id(),"purpose":p.purpose()}))
            .collect();
        Ok(json!({
            "project_id":self.project.id().as_str(),"workspace":self.project.workspace(),
            "endpoint":self.project.opencode_endpoint().url(),"max_rounds":self.project.max_rounds(),
            "execution_mode":self.project.execution_mode(),"delivery_mode":self.project.delivery_mode(),
            "max_active_tasks":self.project.max_active_tasks(),
            "default_profile":self.project.default_profile().unwrap_or(DEFAULT_PROFILE_ID),"profiles":profiles,
            "parallel_writers_enabled":self.project.allow_parallel_writers(),
            "active_writer_count":writer_ids.len(),"active_writer_task_id":writer_ids.first(),
            "active_writer_task_ids":writer_ids,"active_task_id":task.map(|t|t.task_id.to_string()),
            "active_status":task.map(|t|t.status),
        }))
    }
}
fn check_layout(project: &ProjectEntry, layout: &RustStateLayout) -> Result<()> {
    let root = layout.state_root();
    if layout.project_id() != project.id()
        || !root.is_absolute()
        || root.to_str().is_none()
        || root.components().any(|c| matches!(c, Component::ParentDir))
        || root.starts_with(project.workspace())
        || project.workspace().starts_with(root)
    {
        return Err(McpError::Binding);
    }
    let mut ancestor = std::path::PathBuf::new();
    for component in layout.project_dir().components() {
        ancestor.push(component);
        check_file_type(&ancestor, true)?;
    }
    for file in [layout.marker(), layout.database()] {
        check_file_type(&file, false)?;
    }
    Ok(())
}
fn check_file_type(path: &Path, directory: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta)
            if meta.file_type().is_symlink()
                || (directory && !meta.is_dir())
                || (!directory && !meta.is_file()) =>
        {
            Err(McpError::Binding)
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(McpError::Io),
    }
}
