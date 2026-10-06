//! Read-only task routing; no server start, worker activation or recovery.
use crate::{RuntimeError, ServerCommand};
use bridge_config::{Endpoint, ProjectEntry};
use bridge_domain::{ExecutionMode, TaskId};
use bridge_opencode::OpenCodeClient;
use bridge_storage::{RustStateLayout, WorktreeStatus};
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::Duration,
};
pub struct Target {
    project: ProjectEntry,
    session: Option<String>,
}
impl Target {
    pub fn workspace(&self) -> &Path {
        self.project.workspace()
    }
    pub fn endpoint(&self) -> String {
        self.project.opencode_endpoint().url()
    }
    pub fn argv(&self) -> Vec<String> {
        let mut args = vec![
            "attach".into(),
            self.endpoint(),
            "--dir".into(),
            self.workspace().to_string_lossy().into_owned(),
        ];
        if let Some(s) = &self.session {
            args.extend(["--session".into(), s.clone()]);
        }
        args
    }
    pub fn launch(&self, command: &ServerCommand) -> Result<ExitStatus, RuntimeError> {
        let env = self
            .project
            .read_opencode_env()
            .map_err(|_| RuntimeError::Credentials)?;
        let password = self
            .project
            .read_password()
            .map_err(|_| RuntimeError::Credentials)?;
        let mut cmd = Command::new(&command.program);
        cmd.args(&command.args)
            .args(self.argv())
            .current_dir(self.workspace());
        if let Some(env) = env {
            cmd.envs(env.iter());
        }
        cmd.env("OPENCODE_SERVER_USERNAME", "opencode")
            .env("OPENCODE_SERVER_PASSWORD", password.expose_secret());
        cmd.status().map_err(|_| RuntimeError::Spawn)
    }
}
pub fn resolve(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    task: Option<TaskId>,
    attach: bool,
    timeout: Duration,
) -> Result<Target, RuntimeError> {
    let mut view = project.clone();
    let mut session = None;
    if let Some(id) = task {
        if layout.project_id() != project.id() {
            return Err(RuntimeError::Binding);
        }
        let s = layout
            .open_readonly()
            .map_err(|_| RuntimeError::Ownership)?;
        let saved = s
            .get_task(id)
            .map_err(|_| RuntimeError::Binding)?
            .ok_or(RuntimeError::Binding)?;
        if saved.project_id != *project.id() || Path::new(&saved.workspace) != project.workspace() {
            return Err(RuntimeError::Binding);
        }
        let raw: String = s
            .connection()
            .query_row(
                "SELECT execution_mode FROM tasks WHERE task_id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .map_err(|_| RuntimeError::Binding)?;
        let mode = ExecutionMode::try_from(raw).map_err(|_| RuntimeError::Binding)?;
        if mode == ExecutionMode::Worktree {
            if saved.status.is_terminal() {
                return Err(RuntimeError::Binding);
            }
            let record = s
                .get_worktree(id, project.id())
                .map_err(|_| RuntimeError::Binding)?
                .ok_or(RuntimeError::Binding)?;
            if record.status != WorktreeStatus::Created || record.base_head != saved.base_head {
                return Err(RuntimeError::Binding);
            }
            let proof = bridge_git::checkout::probe_checkout(
                project.workspace(),
                &layout.project_dir(),
                id,
                &PathBuf::from(record.path),
                None,
            )
            .map_err(|_| RuntimeError::Binding)?;
            if record.runtime_dir.as_deref() != proof.paths.runtime_dir.to_str() {
                return Err(RuntimeError::Binding);
            }
            let port = record.server_port.ok_or(RuntimeError::Binding)?.get();
            let endpoint = Endpoint::loopback(port).map_err(|_| RuntimeError::Binding)?;
            if record.server_endpoint.as_deref() != Some(endpoint.url().as_str()) {
                return Err(RuntimeError::Binding);
            }
            view = project
                .execution_view(&proof.paths.checkout, endpoint)
                .map_err(|_| RuntimeError::Binding)?;
        }
        if attach {
            session = Some(
                saved
                    .session_id
                    .filter(|s| {
                        !s.is_empty()
                            && s.len() <= 128
                            && !s.starts_with('-')
                            && s.bytes().all(|c| {
                                c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b':')
                            })
                    })
                    .ok_or(RuntimeError::Binding)?,
            );
        }
    } else if attach {
        return Err(RuntimeError::Binding);
    }
    let client =
        OpenCodeClient::from_project(&view, timeout).map_err(|_| RuntimeError::Credentials)?;
    client
        .verify_workspace()
        .map_err(|_| RuntimeError::Binding)?;
    if let Some(id) = &session {
        let remote = client.get_session(id).map_err(|_| RuntimeError::Binding)?;
        if remote.id() != Some(id) || remote.directory() != view.workspace().to_str() {
            return Err(RuntimeError::Binding);
        }
    }
    Ok(Target {
        project: view,
        session,
    })
}
