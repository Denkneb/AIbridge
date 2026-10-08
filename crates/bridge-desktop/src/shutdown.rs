//! Application shutdown stops processes without removing projects or worktrees.
use crate::projects::ProjectService;
use bridge_config::ProjectEntry;
use bridge_storage::{RustStateLayout, WorktreeStatus};
use serde_json::{Value, json};
use std::sync::{RwLockReadGuard, atomic::Ordering};
impl ProjectService {
    /// Nonblocking admission fence used immediately by the native close event.
    pub fn begin_shutdown(&self) {
        self.closing.store(true, Ordering::Release);
    }
    pub(crate) fn activity_guard(&self) -> Result<RwLockReadGuard<'_, bool>, &'static str> {
        if self.closing.load(Ordering::Acquire) {
            return Err("application is shutting down");
        }
        let guard = self
            .activity
            .read()
            .map_err(|_| "application state unavailable")?;
        if *guard || self.closing.load(Ordering::Acquire) {
            return Err("application is shutting down");
        }
        Ok(guard)
    }
    /// Fences in-flight desktop launches before taking the project snapshot.
    /// Errors from one project never skip cleanup of the remaining projects.
    pub fn shutdown_all(&self) -> Result<Value, String> {
        self.begin_shutdown();
        let mut activity = self
            .activity
            .write()
            .map_err(|_| "application state unavailable")?;
        *activity = true;
        let mut errors = Vec::new();
        match self.workers.lock() {
            Ok(mut workers) => {
                for tree in workers.drain(..) {
                    if let Err(e) = tree.stop() {
                        errors.push(json!({"stage":"recovery_worker","error":e.to_string()}));
                    }
                }
            }
            Err(_) => {
                errors.push(json!({"stage":"recovery_worker","error":"worker state unavailable"}))
            }
        }
        let config = self.config_view()?;
        for entry in config.projects().values() {
            let layout = RustStateLayout::new(self.state.clone(), entry.id().clone())
                .map_err(|_| "state invalid")?;
            errors.extend(shutdown(entry, &layout));
        }
        Ok(json!({"status":if errors.is_empty() {"stopped"} else {"incomplete"},"errors":errors}))
    }
    /// Remote executor RPC closes exactly its configured project.
    pub fn shutdown_project(&self, id: &str) -> Result<Value, String> {
        self.begin_shutdown();
        let mut activity = self
            .activity
            .write()
            .map_err(|_| "application state unavailable")?;
        *activity = true;
        let (entry, layout) = self.project(id)?;
        let errors = shutdown(&entry, &layout);
        if !errors.is_empty() {
            return Err(json!(errors).to_string());
        }
        Ok(json!({"status":"stopped"}))
    }
}
fn shutdown(entry: &ProjectEntry, layout: &RustStateLayout) -> Vec<Value> {
    let mut errors = Vec::new();
    let mut error = |stage: &str, detail: String| {
        errors.push(json!({"project":entry.id().as_str(),"stage":stage,"error":detail}))
    };
    // Pause the local controller first, so it cannot restart the remote executor.
    if layout.database().exists()
        && let Err(e) = bridge_automation::lifecycle::shutdown(layout, entry)
    {
        error("automation", e.to_string());
    }
    if let Some(remote) = entry.remote_execution() {
        if let Err(e) = bridge_automation::remote::rpc(remote, &json!({"op":"shutdown"})) {
            error("remote", e);
        }
        return errors;
    }
    if !layout.database().exists() {
        // Saving preferences can create a private namespace without setup.
        // Such a project owns no runtime; unexpected runtime artifacts still fail closed.
        if [
            "mcp.process.json",
            "opencode.process.json",
            "automation",
            "worktrees",
        ]
        .iter()
        .any(|name| std::fs::symlink_metadata(layout.project_dir().join(name)).is_ok())
        {
            error("state", "runtime state ownership invalid".into());
        }
        return errors;
    }
    // MCP owns its task workers; process-tree stop also terminates their children.
    if let Err(e) = bridge_runtime::project::shutdown(entry, layout) {
        error("services", e.to_string());
    }
    let storage = match layout.open_readonly() {
        Ok(storage) => storage,
        Err(_) => {
            error("state", "state ownership invalid".into());
            return errors;
        }
    };
    let mut offset = 0;
    loop {
        let tasks = match storage.list_tasks(entry.id(), false, 200, offset) {
            Ok(tasks) => tasks,
            Err(_) => {
                error("tasks", "task state unavailable".into());
                break;
            }
        };
        if tasks.is_empty() {
            break;
        }
        offset += tasks.len() as i64;
        for task in tasks {
            match storage.get_worktree(task.task_id, entry.id()) {
                Ok(Some(tree))
                    if matches!(
                        tree.status,
                        WorktreeStatus::Created | WorktreeStatus::Removing
                    ) =>
                {
                    if let Err(e) =
                        bridge_runtime::stop_worktree_server(layout, entry, task.task_id, &[])
                    {
                        error("worktree", e.to_string());
                    }
                }
                Ok(_) => {}
                Err(_) => error("worktree", "worktree state unavailable".into()),
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shutdown_waits_for_existing_operations_but_rejects_new_ones_immediately() {
        let root =
            std::env::temp_dir().join(format!("bridge-shutdown-gate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let service = std::sync::Arc::new(
            ProjectService::new(root.join("projects.toml"), root.join("state")).unwrap(),
        );
        let operation = service.activity_guard().unwrap();
        let worker = service.clone();
        let shutdown = std::thread::spawn(move || worker.shutdown_all());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !service.closing.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(service.activity_guard().is_err());
        assert!(!shutdown.is_finished());
        drop(operation);
        assert_eq!(shutdown.join().unwrap().unwrap()["status"], "stopped");
        assert!(!service.state.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
