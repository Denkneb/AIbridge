//! Explicit editing of the selected project's OpenCode JSON/JSONC only.
use crate::projects::ProjectService;
use bridge_config::migration::{atomic_write, safe_path};
use bridge_runtime::controller::validate_workspace_config_text;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const LIMIT: usize = 1024 * 1024;
pub(crate) struct PendingConfig {
    project: String,
    file: String,
    config: Vec<u8>,
    path: PathBuf,
    original: Option<Vec<u8>>,
    proposed: Vec<u8>,
}
fn read(path: &Path) -> Result<Option<Vec<u8>>, &'static str> {
    safe_path(path)?;
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("OpenCode config unavailable"),
    };
    let metadata = file.metadata().map_err(|_| "OpenCode config unavailable")?;
    if !metadata.is_file() || metadata.len() > LIMIT as u64 {
        return Err("OpenCode config must be a regular file up to 1 MiB");
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "OpenCode config unavailable")?;
    if bytes.len() > LIMIT {
        return Err("OpenCode config exceeds 1 MiB");
    }
    Ok(Some(bytes))
}
impl ProjectService {
    fn opencode_path(&self, project: &str, file: &str) -> Result<PathBuf, &'static str> {
        if !matches!(file, "opencode.json" | "opencode.jsonc") {
            return Err("unsupported OpenCode config filename");
        }
        let (p, _) = self.project(project)?;
        Ok(p.workspace().join(file))
    }
    pub fn read_opencode_config(&self, project: &str, file: &str) -> Result<Value, &'static str> {
        let bytes = read(&self.opencode_path(project, file)?)?;
        let content = bytes
            .as_ref()
            .map(|v| std::str::from_utf8(v).map_err(|_| "OpenCode config must be UTF-8"))
            .transpose()?;
        Ok(
            json!({"file": file, "exists": bytes.is_some(), "content": content.unwrap_or("{\n  \"$schema\": \"https://opencode.ai/config.json\"\n}\n")}),
        )
    }
    pub fn preview_opencode_config(
        &self,
        project: &str,
        file: &str,
        content: &str,
        original_content: Option<&str>,
    ) -> Result<Value, &'static str> {
        if content.len() > LIMIT {
            return Err("OpenCode config exceeds 1 MiB");
        }
        let config = self.config_view()?;
        let p = config.project(project).ok_or("project not configured")?;
        validate_workspace_config_text(&config.linked_projects(p.id().as_str()), content)
            .map_err(|_| "invalid OpenCode JSON/JSONC or reserved bridge settings (default_agent, subagent_depth, bridge-controller, agent_bridge)")?;
        let path = self.opencode_path(project, file)?;
        let original = read(&path)?;
        if original.as_deref() != original_content.map(str::as_bytes) {
            return Err("OpenCode config changed; reload before preview");
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut pending = self
            .pending_opencode
            .lock()
            .map_err(|_| "preview unavailable")?;
        if pending.len() >= 16 {
            pending.clear();
        }
        pending.insert(
            id.clone(),
            PendingConfig {
                project: project.into(),
                file: file.into(),
                config: self.config_bytes()?,
                path,
                original,
                proposed: content.as_bytes().to_vec(),
            },
        );
        Ok(json!({"review_id": id, "file": file, "valid": true}))
    }
    pub fn apply_opencode_config(&self, review_id: &str) -> Result<Value, String> {
        let mut pending = self
            .pending_opencode
            .lock()
            .map_err(|_| "preview unavailable")?;
        let change = pending.get(review_id).ok_or("preview expired")?;
        let _config_lock = self.config_file_guard()?;
        let config = self.config_view()?;
        let _runtime = bridge_runtime::project::project_config_edit_guard(
            &config,
            &config,
            &change.project,
            &self.state,
        )
        .map_err(|e| e.to_string())?;
        if self.config_bytes()? != change.config
            || self.opencode_path(&change.project, &change.file)? != change.path
            || read(&change.path)? != change.original
        {
            return Err("configuration changed; reload and request a new preview".into());
        }
        let p = config
            .project(&change.project)
            .ok_or("project not configured")?;
        validate_workspace_config_text(
            &config.linked_projects(p.id().as_str()),
            std::str::from_utf8(&change.proposed).map_err(|_| "invalid UTF-8")?,
        )
        .map_err(|_| "OpenCode config no longer valid")?;
        let mode = fs::metadata(&change.path)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        if let Some(original) = &change.original {
            let backup =
                change
                    .path
                    .with_file_name(format!("{}.{}.bak", change.file, uuid::Uuid::new_v4()));
            atomic_write(&backup, original, mode)?;
        }
        atomic_write(&change.path, &change.proposed, mode)?;
        pending.remove(review_id);
        Ok(json!({"saved": true}))
    }
    pub fn cancel_opencode_config(&self, review_id: &str) {
        if let Ok(mut pending) = self.pending_opencode.lock() {
            pending.remove(review_id);
        }
    }
}
