//! Delegated task adapters. Raw diagnostics never enter protocol errors.
use crate::{McpServer, check_layout};
use bridge_domain::{
    DeliveryMode, ExecutionMode, ProfileDefinitionSource, RoundKind, TaskId, TaskStatus,
    request_payload_hash,
};
use bridge_storage::{
    CreateTaskError, CreateTaskInput, RoundRef, RoundRow, RustStateLayout, StorageConnection, Task,
    TaskBudget,
};
use bridge_worker::{
    WorkerLock, WorkerLockOutcome,
    recovery_startup::{spawn_lease_pending, task_worker_running},
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime},
};
type Result<T> = std::result::Result<T, &'static str>;
const PROBE: Duration = Duration::from_secs(2);
const SPAWN_GRACE: Duration = Duration::from_secs(5);
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("invalid_request")
}
fn flag(v: &Value, key: &str) -> Result<bool> {
    match v.get(key) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        _ => Err(match key {
            "verbose" => "invalid_verbose",
            "allow_dirty" | "allow_commit" => "invalid_git_policy",
            "allow_suspected_secrets" => "invalid_allow_suspected_secrets",
            "allow_budget_override" => "invalid_allow_budget_override",
            _ => "invalid_boolean",
        }),
    }
}
fn strings(v: &Value, key: &str) -> Result<Vec<String>> {
    v.get(key)
        .and_then(Value::as_array)
        .ok_or("invalid_request")?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or("invalid_request"))
        .collect()
}
fn id(v: &Value) -> Result<TaskId> {
    text(v, "task_id")
        .map_err(|_| "invalid_task_id")?
        .parse()
        .map_err(|_| "invalid_task_id")
}
fn secret_gate(v: &Value) -> Result<()> {
    if flag(v, "allow_suspected_secrets")? {
        return Ok(());
    }
    fn visit(v: &Value) -> bool {
        match v {
            Value::String(s) => !bridge_config::suspected_secret_categories(s).is_empty(),
            Value::Array(a) => a.iter().any(visit),
            Value::Object(o) => o.values().any(visit),
            _ => false,
        }
    }
    if visit(v) {
        Err("suspected_secrets")
    } else {
        Ok(())
    }
}
fn round(storage: &StorageConnection, id: TaskId) -> Result<RoundRow> {
    storage
        .connection()
        .query_row(
            "SELECT * FROM rounds WHERE task_id=?1 ORDER BY round_number DESC LIMIT 1",
            [id.to_string()],
            |r| Ok(RoundRow::from_row(r)),
        )
        .map_err(|_| "state_unavailable")?
        .map_err(|_| "state_unavailable")
}
fn request(
    storage: &StorageConnection,
    project: &bridge_domain::ProjectId,
    request_id: &str,
) -> Result<Option<RoundRow>> {
    storage
        .connection()
        .query_row(
            "SELECT * FROM rounds WHERE project_id=?1 AND request_id=?2",
            rusqlite::params![project.as_str(), request_id],
            |r| Ok(RoundRow::from_row(r)),
        )
        .optional()
        .map_err(|_| "state_unavailable")?
        .transpose()
        .map_err(|_| "state_unavailable")
}
fn reference(task: &Task, row: &RoundRow) -> RoundRef {
    RoundRef {
        task_id: task.task_id,
        project_id: task.project_id.clone(),
        round_number: row.round_number,
    }
}
impl McpServer {
    /// Reconcile owned state and resume every eligible writer after a restart.
    /// Parked tasks keep their blocker gate; waiting/review/failed tasks are
    /// never activated. Each task is reread under the existing spawn fences.
    /// # Errors
    /// Invalid state fails closed. A per-task failure does not prevent recovery
    /// of the other writers; diagnostics never include task or endpoint data.
    pub fn recover_startup(&self) -> crate::Result<()> {
        if self.spawner.is_none() {
            return Ok(());
        }
        check_layout(&self.project, &self.layout)?;
        let settings = bridge_storage::AdmissionSettings::new(
            self.project.max_active_tasks(),
            self.project.allow_parallel_writers(),
            self.project.execution_mode(),
        )
        .map_err(|_| crate::McpError::State)?;
        {
            let _admission = match WorkerLock::try_acquire_admission(&self.layout)
                .map_err(|_| crate::McpError::State)?
            {
                WorkerLockOutcome::Busy => return Ok(()),
                WorkerLockOutcome::Acquired(g) => g,
            };
            self.layout
                .open()
                .map_err(|_| crate::McpError::State)?
                .reconcile_active_writers(self.project.id(), &settings)
                .map_err(|_| crate::McpError::State)?;
        }
        // A running direct worker can hold the scan fence. This is a deferred
        // scan, not permission to bypass it or stop recovering other writers.
        let mut failed =
            match bridge_worker::lifecycle::quarantine_orphans(&self.layout, &self.project) {
                Ok(_) | Err(bridge_worker::execution::ExecutionError::Round) => false,
                Err(_) => true,
            };
        let active = self
            .layout
            .open()
            .map_err(|_| crate::McpError::State)?
            .active_set(self.project.id())
            .map_err(|_| crate::McpError::State)?;
        let layouts = self.layouts().map_err(|_| crate::McpError::State)?;
        let refs = layouts.iter().collect::<Vec<_>>();
        for summary in active.tasks {
            let outcome = (|| -> Result<()> {
                let Some(task) = self.task(summary.task_id)? else {
                    return Ok(());
                };
                if task.close_requested_at.is_some() {
                    return self.finish_close(&task);
                }
                let gate = bridge_worker::lifecycle::recover_worktree_task(
                    &self.layout,
                    &self.project,
                    task.task_id,
                    &refs,
                )
                .map_err(|_| "cleanup_failed")?;
                match task.status {
                    TaskStatus::Implementing | TaskStatus::Revising if gate.may_spawn() => {
                        self.maybe_spawn(task.task_id)
                    }
                    TaskStatus::DeliveryUnknown => self.maybe_spawn(task.task_id),
                    TaskStatus::NeedsUser => {
                        let spawn = self.spawner.as_ref().ok_or("worker_unavailable")?;
                        let outcome = bridge_worker::recovery::recover_needs_user(
                            &self.layout,
                            &self.project,
                            task.task_id,
                            false,
                            &refs,
                            PROBE,
                            |r| spawn(r),
                        )
                        .map_err(|_| "recovery_failed")?;
                        if matches!(
                            outcome,
                            bridge_worker::recovery::RecoverySpawnOutcome::SpawnFailed
                        ) {
                            return Err("worker_spawn_failed");
                        }
                        Ok(())
                    }
                    _ => Ok(()),
                }
            })();
            failed |= outcome.is_err();
        }
        if failed {
            Err(crate::McpError::State)
        } else {
            Ok(())
        }
    }

    pub(crate) fn call_task(&self, name: &str, args: &Value) -> Result<Value> {
        check_layout(&self.project, &self.layout).map_err(|_| "mcp_state_unavailable")?;
        let outcome = match name {
            "submit_task" => self.submit(args),
            "task_status" => self.status(args),
            "request_changes" => self.revise(args),
            "accept_task" => self.accept(args),
            "close_task" => self.close(args),
            _ => Err("unknown_tool"),
        };
        Ok(outcome.unwrap_or_else(|code| self.error_payload(code, args)))
    }
    /// Trusted coordinator calls only. Run identity is an explicit Rust
    /// parameter, never a field accepted by public JSON-RPC wrappers.
    pub fn call_automation(
        &self,
        name: &str,
        args: &Value,
        run: bridge_storage::automation::RunId,
    ) -> Value {
        let outcome = match name {
            "submit_task" => self.submit_managed(args, Some(run)),
            "request_changes" => self.revise_managed(args, Some(run)),
            "task_status" => self.status(args),
            "accept_task" => self.accept(args),
            "close_task" => self.close(args),
            _ => Err("unknown_tool"),
        };
        outcome.unwrap_or_else(|code| self.error_payload(code, args))
    }
    pub(super) fn storage(&self) -> Result<StorageConnection> {
        self.layout.open().map_err(|_| "state_unavailable")
    }
    pub(super) fn task(&self, id: TaskId) -> Result<Option<Task>> {
        let task = self
            .storage()?
            .get_task(id)
            .map_err(|_| "state_unavailable")?;
        if let Some(t) = &task
            && (&t.project_id != self.project.id()
                || Path::new(&t.workspace) != self.project.workspace())
        {
            return Err("task_binding_mismatch");
        }
        Ok(task)
    }
    pub(super) fn layouts(&self) -> Result<Vec<RustStateLayout>> {
        self.registry
            .iter()
            .map(|p| {
                RustStateLayout::new(self.layout.state_root(), p.id().clone())
                    .map_err(|_| "state_unavailable")
            })
            .collect()
    }
    fn view(&self, task: &Task) -> Result<Option<bridge_config::ProjectEntry>> {
        if task.status == TaskStatus::AwaitingReview {
            return bridge_worker::recovery::review_execution_view(
                &self.layout,
                &self.project,
                task,
            )
            .map(Some)
            .map_err(|_| "execution_binding_invalid");
        }
        bridge_worker::recovery::task_execution_view(&self.layout, &self.project, task.task_id)
            .map_err(|_| "execution_binding_invalid")
    }
    pub(super) fn turn_idle(&self, task: &Task) -> Result<()> {
        let Some(view) = self.view(task)? else {
            return Ok(());
        };
        let Some(session) = task.session_id.as_deref() else {
            return Ok(());
        };
        let client = bridge_opencode::OpenCodeClient::from_project(&view, PROBE)
            .map_err(|_| "server_unavailable")?;
        client
            .verify_workspace()
            .map_err(|_| "server_unavailable")?;
        let active = client
            .session_turn_active(session)
            .map_err(|_| "server_unavailable")?;
        let latest = self.status_task(task.task_id)?.ok_or("unknown_task")?;
        let current_view = self.view(&latest)?.ok_or("execution_binding_invalid")?;
        if latest.session_id != task.session_id
            || current_view.workspace() != view.workspace()
            || current_view.opencode_endpoint() != view.opencode_endpoint()
        {
            return Err("execution_binding_invalid");
        }
        if active { Err("server_busy") } else { Ok(()) }
    }
    fn maybe_spawn(&self, id: TaskId) -> Result<()> {
        let Some(task) = self.task(id)? else {
            return Ok(());
        };
        if task.status == TaskStatus::DeliveryUnknown && task.close_requested_at.is_none() {
            let layouts = self.layouts()?;
            let spawn = self.spawner.as_ref().ok_or("worker_unavailable")?;
            let outcome = bridge_worker::recovery::recover_delivery_unknown(
                &self.layout,
                &self.project,
                id,
                &layouts.iter().collect::<Vec<_>>(),
                PROBE,
                |r| spawn(r),
            )
            .map_err(|_| "recovery_failed")?;
            return if matches!(
                outcome,
                bridge_worker::recovery::RecoverySpawnOutcome::SpawnFailed
            ) {
                Err("worker_spawn_failed")
            } else {
                Ok(())
            };
        }
        if !matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising)
            || task.close_requested_at.is_some()
        {
            return Ok(());
        }
        let lease = {
            let _admission = match WorkerLock::try_acquire_admission(&self.layout)
                .map_err(|_| "state_unavailable")?
            {
                WorkerLockOutcome::Busy => return Ok(()),
                WorkerLockOutcome::Acquired(g) => g,
            };
            let task = self.task(id)?.ok_or("unknown_task")?;
            if !matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising)
                || task.close_requested_at.is_some()
            {
                return Ok(());
            }
            if task_worker_running(&self.layout, id).map_err(|_| "state_unavailable")? {
                return Ok(());
            }
            let mut storage = self.storage()?;
            let row = round(&storage, id)?;
            if !row.status.is_open()
                || spawn_lease_pending(
                    row.worker_started_at.as_deref(),
                    SystemTime::now(),
                    SPAWN_GRACE,
                )
            {
                return Ok(());
            }
            let reference = reference(&task, &row);
            let lease = storage
                .mark_worker_started(reference.clone(), 3600.0)
                .map_err(|_| "state_unavailable")?
                .round
                .worker_started_at
                .ok_or("state_unavailable")?;
            (reference, lease)
        };
        if self.spawner.as_ref().ok_or("worker_unavailable")?(&lease.0).is_err() {
            self.storage()?
                .release_worker_spawn(&lease.0, &lease.1)
                .map_err(|_| "state_unavailable")?;
            return Err("worker_spawn_failed");
        }
        Ok(())
    }
    fn submit(&self, args: &Value) -> Result<Value> {
        self.submit_managed(args, None)
    }
    fn submit_managed(
        &self,
        args: &Value,
        caller: Option<bridge_storage::automation::RunId>,
    ) -> Result<Value> {
        if caller.is_some()
            && (self.project.execution_mode() != ExecutionMode::Worktree
                || self.project.delivery_mode() != DeliveryMode::Manual)
        {
            return Err("automation_execution_policy_mismatch");
        }
        let provenance = caller
            .map(|run| {
                bridge_worker::automation::submit_provenance(&self.layout, &self.project, run, args)
            })
            .transpose()?;
        if caller.is_none()
            && [
                "automation_run_id",
                "inherit_task_id",
                "inherit_fingerprint",
            ]
            .iter()
            .any(|k| args.get(*k).is_some())
        {
            return Err("invalid_arguments");
        }
        let request_id = text(args, "request_id").map_err(|_| "invalid_request_id")?;
        let task_text = text(args, "task").map_err(|_| "empty_task")?;
        let paths = strings(args, "allowed_paths").map_err(|_| "invalid_allowed_paths")?;
        if paths.is_empty() {
            return Err("invalid_allowed_paths");
        }
        let commands = strings(args, "test_commands").map_err(|_| "invalid_test_commands")?;
        let allow_dirty = flag(args, "allow_dirty")?;
        let allow_commit = flag(args, "allow_commit")?;
        secret_gate(args)?;
        let workflow = bridge_worker::workflow::normalize(args)?;
        let profile_arg = args.get("profile").filter(|v| !v.is_null());
        let profile_id = profile_arg
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty() && s.trim() == *s)
                    .ok_or("invalid_profile")
            })
            .transpose()?;
        let profile = self
            .project
            .profile_snapshot(profile_id)
            .map_err(|_| "unknown_profile")?;
        let budget = args
            .get("budget")
            .filter(|v| !v.is_null())
            .map(TaskBudget::from_json)
            .transpose()
            .map_err(|_| "invalid_budget")?;
        let command_refs = commands.iter().map(String::as_str).collect::<Vec<_>>();
        if !bridge_command_policy::validate_test_commands(&command_refs).is_empty() {
            return Err("invalid_test_commands");
        }
        let mut payload = json!({"kind":"implement","task":task_text,"allowed_paths":paths,"test_commands":commands,"allow_dirty":allow_dirty,"allow_commit":allow_commit});
        if let Some(b) = &budget {
            payload["budget"] = b.as_json().clone();
        }
        if profile.source != ProfileDefinitionSource::Builtin || profile.id != "implementer" {
            payload["profile"] = json!({"id":profile.id,"definition_hash":profile.definition_hash,"model":profile.model});
        }
        if workflow.workflow_id.is_some() {
            payload["workflow_id"] = json!(workflow.workflow_id);
        }
        if !workflow.depends_on.is_empty() {
            payload["depends_on"] = json!(workflow.depends_on);
        }
        if let Some(provenance) = &provenance {
            for (key, value) in provenance.as_object().ok_or("automation_state_corrupt")? {
                payload[key] = value.clone();
            }
        }
        let hash = request_payload_hash(&payload);
        if let Some(existing) = request(&self.storage()?, self.project.id(), request_id)? {
            if existing.kind != RoundKind::Implement || existing.payload_hash != hash {
                return Err("request_conflict");
            }
            let task = self.task(existing.task_id)?.ok_or("state_unavailable")?;
            return self.result(&task, false);
        }
        bridge_worker::workflow::validate_references(
            &self.layout,
            &self.project,
            &self.registry,
            &workflow,
        )?;
        let initial_status = if bridge_worker::workflow::gate(
            &self.layout,
            &self.project,
            &self.registry,
            &workflow,
        )["state"]
            == "ready"
        {
            TaskStatus::Implementing
        } else {
            TaskStatus::WaitingDependencies
        };
        if self.project.execution_mode() == ExecutionMode::Worktree
            && paths.iter().any(|p| Path::new(p).is_absolute())
        {
            return Ok(
                json!({"error":"external_paths_not_supported_in_worktree_mode","paths":paths.iter().filter(|p|Path::new(p).is_absolute()).collect::<Vec<_>>()}),
            );
        }
        let trusted = self
            .project
            .auto_approve_external_directories()
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        let raw = paths.iter().map(String::as_str).collect::<Vec<_>>();
        let normalized = bridge_path_policy::validate_allowed_paths_with_trusted_roots(
            self.project.workspace(),
            &trusted,
            &raw,
        )
        .map_err(|_| "invalid_allowed_paths")?
        .iter()
        .map(|p| p.to_scope_string())
        .collect::<Vec<_>>();
        let normalized_refs = normalized.iter().map(String::as_str).collect::<Vec<_>>();
        let groups = bridge_path_policy::group_allowed_paths_by_repo(
            self.project.workspace(),
            &normalized_refs,
        )
        .map_err(|_| "invalid_allowed_paths")?;
        let main = bridge_git::take_snapshot(self.project.workspace())
            .map_err(|_| "git_snapshot_failed")?;
        let mut snapshot = main.to_json().map_err(|_| "git_snapshot_failed")?;
        let mut external = Vec::new();
        for group in &groups {
            let baseline = if group.root() == self.project.workspace() {
                main.clone()
            } else {
                bridge_git::take_snapshot(group.root()).map_err(|_| "git_snapshot_failed")?
            };
            if !allow_dirty && !baseline.dirty_paths().is_empty() {
                let dirty = baseline
                    .dirty_paths()
                    .iter()
                    .map(|p| {
                        if group.root() == self.project.workspace() {
                            p.to_string_lossy().into_owned()
                        } else {
                            group.root().join(p).to_string_lossy().into_owned()
                        }
                    })
                    .collect::<Vec<_>>();
                return Ok(json!({"error":"dirty_workspace","dirty_paths":dirty}));
            }
            for path in baseline.dirty_paths() {
                let qualified = if group.root() == self.project.workspace() {
                    path.to_str().ok_or("git_snapshot_failed")?.to_owned()
                } else {
                    group
                        .root()
                        .join(path)
                        .to_str()
                        .ok_or("git_snapshot_failed")?
                        .to_owned()
                };
                if !group.entries().iter().any(|scope| {
                    scope == "**"
                        || qualified == *scope
                        || (scope.ends_with('/') && qualified.starts_with(scope))
                }) {
                    return Ok(json!({"error":"dirty_paths_outside_scope","paths":[qualified]}));
                }
            }
            if group.root() != self.project.workspace() {
                let mut ext = baseline.to_json().map_err(|_| "git_snapshot_failed")?;
                ext["root"] = json!(group.root());
                external.push(ext);
            }
        }
        snapshot["external_repositories"] = json!(external);
        if let Some(provenance) = &provenance {
            for (key, value) in provenance.as_object().ok_or("automation_state_corrupt")? {
                snapshot[key] = value.clone();
            }
        }
        let _opencode_lease = if self.project.execution_mode() == ExecutionMode::Direct {
            let lease =
                bridge_runtime::project::OpenCodeLease::acquire(&self.project, &self.layout)
                    .map_err(|_| "project_busy")?;
            // Existing direct endpoints retain the original health/directory contract.
            // Only an absent managed server is started; occupied or unhealthy servers
            // are never replaced by a new process.
            let client = bridge_opencode::OpenCodeClient::from_project(&self.project, PROBE)
                .map_err(|_| "server_unavailable")?;
            if client.health().is_err() {
                bridge_runtime::project::ensure_opencode(
                    &self.project,
                    &self.layout,
                    &bridge_runtime::ServerCommand::opencode(),
                    std::time::Duration::from_secs(20),
                )
                .map_err(|_| "server_unavailable")?;
            }
            if !client.health().map_err(|_| "server_unavailable")?.healthy() {
                return Err("server_unhealthy");
            }
            client
                .verify_workspace()
                .map_err(|_| "server_wrong_directory")?;
            Some(lease)
        } else {
            None
        };
        let _admission = match WorkerLock::try_acquire_admission(&self.layout)
            .map_err(|_| "state_unavailable")?
        {
            WorkerLockOutcome::Busy => return Err("project_busy"),
            WorkerLockOutcome::Acquired(g) => g,
        };
        // The probe can outlive a repository update. Persist only the baseline
        // that was actually checked, including relevant external repositories.
        if bridge_git::take_snapshot(self.project.workspace())
            .map_err(|_| "git_snapshot_failed")?
            .to_json()
            .map_err(|_| "git_snapshot_failed")?
            != main.to_json().map_err(|_| "git_snapshot_failed")?
        {
            return Err("workspace_changed");
        }
        for saved in &external {
            let root = saved["root"].as_str().ok_or("git_snapshot_failed")?;
            let mut current = bridge_git::take_snapshot(Path::new(root))
                .map_err(|_| "git_snapshot_failed")?
                .to_json()
                .map_err(|_| "git_snapshot_failed")?;
            current["root"] = json!(root);
            if &current != saved {
                return Err("workspace_changed");
            }
        }
        let mut storage = self.storage()?;
        if let Some(caller) = caller
            && bridge_worker::automation::submit_provenance(
                &self.layout,
                &self.project,
                caller,
                args,
            )?
            .as_object()
                != provenance.as_ref().and_then(Value::as_object)
        {
            return Err("automation_binding_changed");
        }
        let outcome = bridge_submission::submit_task_with_workflow(
            &mut storage,
            &self.project,
            bridge_submission::ProfileSubmissionInput {
                task: CreateTaskInput {
                    task_id: uuid::Uuid::new_v4()
                        .to_string()
                        .parse()
                        .map_err(|_| "state_unavailable")?,
                    project_id: self.project.id().clone(),
                    workspace: self
                        .project
                        .workspace()
                        .to_str()
                        .ok_or("state_unavailable")?
                        .into(),
                    task: task_text.into(),
                    request_id: request_id.into(),
                    payload_hash: hash,
                    base_head: main.head().map(|h| h.as_str().into()),
                    allowed_paths: normalized,
                    test_commands: commands,
                    snapshot: Some(snapshot),
                },
                profile: profile_arg.cloned(),
                allow_dirty,
                allow_commit,
                budget,
                initial_status,
            },
            &paths,
            &workflow,
        )
        .map_err(|e| match e {
            bridge_submission::SubmissionError::Storage(CreateTaskError::ProjectBusy) => {
                "project_busy"
            }
            bridge_submission::SubmissionError::Storage(CreateTaskError::ScopeOverlap) => {
                "scope_overlap"
            }
            bridge_submission::SubmissionError::Storage(CreateTaskError::RequestConflict) => {
                "request_conflict"
            }
            bridge_submission::SubmissionError::WorktreeUnsupported => {
                "worktree_submission_unsupported"
            }
            _ => "submission_failed",
        })?;
        let task = outcome.into_task();
        drop(_admission);
        self.maybe_spawn(task.task_id)?;
        self.result(&self.task(task.task_id)?.ok_or("state_unavailable")?, false)
    }
    pub(super) fn execution_mode(&self, id: TaskId) -> Result<ExecutionMode> {
        let raw: String = self
            .storage()?
            .connection()
            .query_row(
                "SELECT execution_mode FROM tasks WHERE task_id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .map_err(|_| "state_unavailable")?;
        serde_json::from_value(json!(raw)).map_err(|_| "execution_binding_invalid")
    }
    fn status(&self, args: &Value) -> Result<Value> {
        let wait = args
            .get("wait_seconds")
            .map(|v| {
                v.as_u64()
                    .filter(|n| *n <= 300)
                    .ok_or("invalid_wait_seconds")
            })
            .transpose()?
            .unwrap_or(300);
        let verbose = flag(args, "verbose")?;
        let explicit = args.get("task_id").is_some_and(|v| !v.is_null());
        let id = match args.get("task_id").filter(|v| !v.is_null()) {
            Some(Value::String(s)) => s.parse().map_err(|_| "invalid_task_id")?,
            Some(_) => return Err("invalid_task_id"),
            None => {
                let active = self
                    .storage()?
                    .active_set(self.project.id())
                    .map_err(|_| "state_unavailable")?;
                match active.tasks.as_slice() {
                    [] => return Ok(json!({"status":"no_active_task"})),
                    [task] => task.task_id,
                    tasks => {
                        return Ok(
                            json!({"error":"ambiguous_task","tasks":tasks.iter().map(|t|json!({"task_id":t.task_id.to_string(),"status":t.status})).collect::<Vec<_>>()}),
                        );
                    }
                }
            }
        };
        let Some(task) = self.status_task(id)? else {
            return Ok(json!({"status":"unknown_task"}));
        };
        let layouts = self.layouts()?;
        let refs = layouts.iter().collect::<Vec<_>>();
        let mut failed_recovery_busy = false;
        let budget_valid = self.task(id).is_ok();
        if task.close_requested_at.is_some() {
            self.finish_close(&task)?;
        } else if task.status == TaskStatus::Failed && explicit && wait > 0 && budget_valid {
            let spawn = self.spawner.as_ref().ok_or("worker_unavailable")?;
            let outcome = bridge_worker::recovery::recover_failed_assistant(
                &self.layout,
                &self.project,
                id,
                &refs,
                PROBE,
                |r| spawn(r),
            )
            .map_err(|_| "recovery_failed")?;
            failed_recovery_busy =
                matches!(outcome, bridge_worker::recovery::RecoverySpawnOutcome::Busy);
            if matches!(
                outcome,
                bridge_worker::recovery::RecoverySpawnOutcome::SpawnFailed
            ) {
                return Err("worker_spawn_failed");
            }
        } else if task.status == TaskStatus::NeedsUser && explicit && budget_valid {
            let spawn = self.spawner.as_ref().ok_or("worker_unavailable")?;
            let outcome = bridge_worker::recovery::recover_needs_user(
                &self.layout,
                &self.project,
                id,
                true,
                &refs,
                PROBE,
                |r| spawn(r),
            )
            .map_err(|_| "recovery_failed")?;
            if matches!(
                outcome,
                bridge_worker::recovery::RecoverySpawnOutcome::SpawnFailed
            ) {
                return Err("worker_spawn_failed");
            }
        } else if task.status == TaskStatus::WaitingDependencies && explicit && budget_valid {
            bridge_worker::admission::activate_waiting_task_with_registry(
                &self.layout,
                &self.project,
                id,
                &self.registry,
            )
            .map_err(|_| "dependency_activation_failed")?;
            self.maybe_spawn(id)?;
        } else if budget_valid {
            self.maybe_spawn(id)?;
        }
        let start = Instant::now();
        loop {
            let task = self.status_task(id)?.ok_or("state_unavailable")?;
            let result = self.result(&task, verbose)?;
            if task.status == TaskStatus::Failed
                && failed_recovery_busy
                && start.elapsed() < Duration::from_secs(wait)
            {
                thread::sleep(
                    Duration::from_millis(100)
                        .min(Duration::from_secs(wait).saturating_sub(start.elapsed())),
                );
                let spawn = self.spawner.as_ref().ok_or("worker_unavailable")?;
                let outcome = bridge_worker::recovery::recover_failed_assistant(
                    &self.layout,
                    &self.project,
                    id,
                    &refs,
                    PROBE,
                    |r| spawn(r),
                )
                .map_err(|_| "recovery_failed")?;
                if matches!(
                    outcome,
                    bridge_worker::recovery::RecoverySpawnOutcome::SpawnFailed
                ) {
                    return Err("worker_spawn_failed");
                }
                failed_recovery_busy =
                    matches!(outcome, bridge_worker::recovery::RecoverySpawnOutcome::Busy);
                continue;
            }
            if !matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising)
                || result["phase"] == "verifying"
                || start.elapsed() >= Duration::from_secs(wait)
            {
                return Ok(result);
            }
            let row = round(&self.storage()?, id)?;
            if !task_worker_running(&self.layout, id).map_err(|_| "state_unavailable")?
                && !spawn_lease_pending(
                    row.worker_started_at.as_deref(),
                    SystemTime::now(),
                    SPAWN_GRACE,
                )
            {
                return Ok(result);
            }
            thread::sleep(
                Duration::from_millis(100)
                    .min(Duration::from_secs(wait).saturating_sub(start.elapsed())),
            );
        }
    }
    fn revise(&self, args: &Value) -> Result<Value> {
        self.revise_managed(args, None)
    }
    fn revise_managed(
        &self,
        args: &Value,
        caller: Option<bridge_storage::automation::RunId>,
    ) -> Result<Value> {
        let id = id(args)?;
        let request_id = text(args, "request_id").map_err(|_| "invalid_request_id")?;
        let findings = text(args, "findings").map_err(|_| "empty_findings")?;
        let override_budget = flag(args, "allow_budget_override")?;
        secret_gate(args)?;
        let Some(task) = self.status_task(id)? else {
            return Ok(json!({"status":"unknown_task"}));
        };
        bridge_worker::automation::revision_authorized(
            &self.layout,
            &self.project,
            &task,
            caller,
            args,
        )?;
        let trusted = self
            .project
            .auto_approve_external_directories()
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        let validated = bridge_worker::validate_revision_findings(
            findings,
            args.get("structured_findings"),
            self.project.workspace(),
            &trusted,
            &task.allowed_paths,
        )
        .map_err(|_| "invalid_structured_findings")?;
        if let Some(existing) = request(&self.storage()?, self.project.id(), request_id)? {
            if existing.task_id != id
                || existing.kind != RoundKind::Revise
                || existing.payload_hash != validated.payload_hash()
            {
                return Err("request_conflict");
            }
            return self.result(&task, false);
        }
        let _fences = bridge_worker::admission::try_review_fences(&self.layout, &self.project, id)
            .map_err(|_| "state_unavailable")?
            .ok_or("worker_running")?;
        let task = self.status_task(id)?.ok_or("unknown_task")?;
        bridge_worker::automation::revision_authorized(
            &self.layout,
            &self.project,
            &task,
            caller,
            args,
        )?;
        if task.status == TaskStatus::NeedsUser {
            return self.result(&task, false);
        }
        if task.status != TaskStatus::AwaitingReview || task.close_requested_at.is_some() {
            return Err("not_awaiting_review");
        }
        if u64::try_from(task.revision_count).map_err(|_| "state_unavailable")?
            >= self.project.max_rounds()
        {
            self.storage()?
                .park_revision_limit(id, self.project.id())
                .map_err(|_| "state_unavailable")?;
            return Err("revision_limit");
        }
        let mut storage = self.storage()?;
        let budget_decision = storage
            .revision_budget_decision(id, self.project.id(), override_budget)
            .map_err(|_| "state_unavailable")?;
        if let Some(error) = budget_decision.error {
            return Err(error);
        }
        self.turn_idle(&task)?;
        let previous = round(&storage, id)?;
        let number = previous
            .round_number
            .checked_add(1)
            .ok_or("revision_limit")?;
        let reference = RoundRef {
            task_id: id,
            project_id: self.project.id().clone(),
            round_number: number,
        };
        (if budget_decision.overridden {
            validated.create_round_with_budget_override(&mut storage, reference, request_id.into())
        } else {
            validated.create_round(&mut storage, reference, request_id.into())
        })
        .map_err(|e| {
            if matches!(e, bridge_storage::RoundUpdateError::RequestConflict) {
                "request_conflict"
            } else {
                "revision_failed"
            }
        })?;
        drop(_fences);
        self.maybe_spawn(id)?;
        self.result(&self.status_task(id)?.ok_or("state_unavailable")?, false)
    }
    fn accept(&self, args: &Value) -> Result<Value> {
        let id = id(args)?;
        let Some(task) = self.task(id)? else {
            return Ok(json!({"status":"unknown_task"}));
        };
        if task.delivery_mode != DeliveryMode::Manual {
            return self.accept_on_accept(&task);
        }
        if task.status == TaskStatus::Accepted {
            return Ok(json!({"task_id":id.to_string(),"status":"accepted"}));
        }
        let _fences = bridge_worker::admission::try_review_fences(&self.layout, &self.project, id)
            .map_err(|_| "state_unavailable")?
            .ok_or("worker_running")?;
        let task = self.task(id)?.ok_or("unknown_task")?;
        if task.status != TaskStatus::AwaitingReview || task.close_requested_at.is_some() {
            return Err("not_awaiting_review");
        }
        if task
            .snapshot
            .as_ref()
            .is_some_and(|s| s.get("automation_run_id").is_some())
        {
            bridge_worker::acceptance::positive_review_gate(&self.layout, &self.project, &task)
                .map_err(|_| "automation_acceptance_refused")?;
        }
        self.turn_idle(&task)?;
        if self.execution_mode(id)? == ExecutionMode::Worktree {
            let layouts = self.layouts()?;
            bridge_runtime::stop_worktree_server(
                &self.layout,
                &self.project,
                id,
                &layouts.iter().collect::<Vec<_>>(),
            )
            .map_err(|_| "worktree_server_stop_failed")?;
        }
        let task = self
            .storage()?
            .accept_manual_task_guarded(id, self.project.id(), |task| {
                !task
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.get("automation_run_id").is_some())
                    || bridge_worker::acceptance::positive_review_gate(
                        &self.layout,
                        &self.project,
                        task,
                    )
                    .is_ok()
            })
            .map_err(|_| {
                if task
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.get("automation_run_id").is_some())
                {
                    "automation_acceptance_refused"
                } else {
                    "accept_failed"
                }
            })?;
        Ok(json!({"task_id":task.task_id.to_string(),"status":task.status}))
    }
    fn finish_close(&self, task: &Task) -> Result<()> {
        if self.execution_mode(task.task_id)? == ExecutionMode::Worktree {
            let layouts = self.layouts()?;
            bridge_worker::lifecycle::recover_worktree_task(
                &self.layout,
                &self.project,
                task.task_id,
                &layouts.iter().collect::<Vec<_>>(),
            )
            .map_err(|_| "cleanup_failed")?;
        } else if let Some(_fences) =
            bridge_worker::admission::try_review_fences(&self.layout, &self.project, task.task_id)
                .map_err(|_| "state_unavailable")?
        {
            self.storage()?
                .complete_requested_close(task.task_id)
                .map_err(|_| "close_failed")?;
        }
        Ok(())
    }
    fn close(&self, args: &Value) -> Result<Value> {
        let id = id(args)?;
        let reason = text(args, "reason").map_err(|_| "reason_required")?;
        let Some(task) = self.task(id)? else {
            return Ok(json!({"status":"unknown_task"}));
        };
        if task.status.is_terminal() {
            return Ok(json!({"task_id":id.to_string(),"status":task.status}));
        }
        let fences = bridge_worker::admission::try_review_fences(&self.layout, &self.project, id)
            .map_err(|_| "state_unavailable")?;
        if task.close_requested_at.is_none() {
            self.turn_idle(&task)?;
        }
        self.storage()?
            .request_task_close(id, reason)
            .map_err(|_| "close_failed")?;
        drop(fences);
        self.finish_close(&task)?;
        let task = self.task(id)?.ok_or("state_unavailable")?;
        Ok(
            json!({"task_id":id.to_string(),"status":if task.status==TaskStatus::Closed{"closed"}else{"close_requested"},"close_requested":task.status!=TaskStatus::Closed}),
        )
    }
}
