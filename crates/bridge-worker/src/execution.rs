//! Frozen execution root, checkout preparation, task runtime and cwd consumers.
//! Caller holds the worker/task lifecycle fence throughout preparation/dispatch.
use bridge_config::ProjectEntry;
use bridge_domain::{ExecutionMode, RoundKind, RoundStatus, TaskStatus};
use bridge_git::{
    RepositoryComparison, RepositorySnapshot,
    checkout::{CheckoutPaths, check_supported, create_checkout, probe_checkout, remove_checkout},
};
use bridge_opencode::OpenCodeClient;
use bridge_runtime::{RuntimeOptions, ServerCommand};
use bridge_storage::{
    RoundRef, RoundRow, RustStateLayout, StorageConnection, Task, WorktreeStatus,
};
use serde_json::Value;
use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionError {
    Ownership,
    Binding,
    Round,
    Profile,
    MissingRecord,
    MissingBase,
    MissingCheckout,
    Baseline,
    Unsupported,
    Git,
    Storage,
    Runtime,
    Verifier,
}
impl ExecutionError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ownership => "execution_state_unowned",
            Self::Binding => "execution_root_mismatch",
            Self::Round => "execution_round_not_dispatchable",
            Self::Profile => "execution_profile_invalid",
            Self::MissingRecord => "worktree_row_missing",
            Self::MissingBase => "worktree_base_missing",
            Self::MissingCheckout => "worktree_missing",
            Self::Baseline => "worktree_baseline_corrupt",
            Self::Unsupported => "worktree_unsupported",
            Self::Git => "worktree_creation_failed",
            Self::Storage => "execution_storage_error",
            Self::Runtime => "worktree_server_unavailable",
            Self::Verifier => "execution_verifier_error",
        }
    }
}
impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl std::error::Error for ExecutionError {}

pub(crate) fn mode(
    storage: &StorageConnection,
    task: &Task,
) -> Result<ExecutionMode, ExecutionError> {
    let raw: String = storage
        .connection()
        .query_row(
            "SELECT execution_mode FROM tasks WHERE task_id=?1 AND project_id=?2",
            [
                task.task_id.to_string(),
                task.project_id.as_str().to_owned(),
            ],
            |r| r.get(0),
        )
        .map_err(|_| ExecutionError::Storage)?;
    serde_json::from_value(serde_json::json!(raw)).map_err(|_| ExecutionError::Binding)
}
/// Shared proof used before session/dispatch and again by verifier/collection.
pub(crate) fn execution_root(
    storage: &StorageConnection,
    layout: &RustStateLayout,
    task: &Task,
    require_base: bool,
) -> Result<PathBuf, ExecutionError> {
    if layout.project_id() != &task.project_id {
        return Err(ExecutionError::Binding);
    }
    if mode(storage, task)? == ExecutionMode::Direct {
        return std::fs::canonicalize(&task.workspace).map_err(|_| ExecutionError::Binding);
    }
    let record = storage
        .get_worktree(task.task_id, &task.project_id)
        .map_err(|_| ExecutionError::Storage)?
        .ok_or(ExecutionError::MissingRecord)?;
    if record.status != WorktreeStatus::Created {
        return Err(ExecutionError::Binding);
    }
    let paths = CheckoutPaths::new(&layout.project_dir(), task.task_id)
        .map_err(|_| ExecutionError::Binding)?;
    if record.runtime_dir.as_deref() != paths.runtime_dir.to_str() {
        return Err(ExecutionError::Binding);
    }
    let base = record
        .base_head
        .as_deref()
        .ok_or(ExecutionError::MissingBase)?;
    if task.base_head.as_deref() != Some(base) {
        return Err(ExecutionError::Binding);
    }
    let binding = probe_checkout(
        Path::new(&task.workspace),
        &layout.project_dir(),
        task.task_id,
        Path::new(&record.path),
        require_base.then_some(base),
    )
    .map_err(|_| ExecutionError::Binding)?;
    Ok(binding.paths.checkout)
}
fn current_task(
    storage: &mut StorageConnection,
    layout: &RustStateLayout,
    project: &ProjectEntry,
    round: &RoundRef,
    observing: bool,
) -> Result<Task, ExecutionError> {
    if layout.project_id() != project.id() || &round.project_id != project.id() {
        return Err(ExecutionError::Binding);
    }
    let task = storage
        .get_task(round.task_id)
        .map_err(|_| ExecutionError::Storage)?
        .ok_or(ExecutionError::Binding)?;
    if task.project_id != round.project_id || Path::new(&task.workspace) != project.workspace() {
        return Err(ExecutionError::Binding);
    }
    let latest: u32 = storage
        .connection()
        .query_row(
            "SELECT MAX(round_number) FROM rounds WHERE task_id=?1",
            [round.task_id.to_string()],
            |r| r.get(0),
        )
        .map_err(|_| ExecutionError::Storage)?;
    let row = storage
        .connection()
        .query_row(
            "SELECT * FROM rounds WHERE task_id=?1 AND round_number=?2",
            rusqlite::params![round.task_id.to_string(), round.round_number],
            |r| Ok(RoundRow::from_row(r)),
        )
        .map_err(|_| ExecutionError::Round)?
        .map_err(|_| ExecutionError::Round)?;
    if latest != round.round_number
        || row.project_id != round.project_id
        || if observing {
            !row.attempted
                || !matches!(
                    row.status,
                    RoundStatus::Sent
                        | RoundStatus::Observing
                        | RoundStatus::NeedsUser
                        | RoundStatus::DeliveryUnknown
                )
                || row.session_id.as_deref().is_none_or(str::is_empty)
                || row.outbound_message_id.as_deref().is_none_or(str::is_empty)
        } else {
            row.status != RoundStatus::Pending || row.attempted
        }
        || task.close_requested_at.is_some()
        || !matches!(
            (row.kind, task.status),
            (RoundKind::Implement, TaskStatus::Implementing)
                | (RoundKind::Revise, TaskStatus::Revising)
        )
    {
        return Err(ExecutionError::Round);
    }
    if let Err(error) = storage.get_task_profile(task.task_id, &task.project_id) {
        if matches!(
            error,
            bridge_storage::profiles::ProfileReadError::MissingSnapshot
                | bridge_storage::profiles::ProfileReadError::CorruptSnapshot
        ) {
            storage
                .fail_profile_snapshot(round.clone(), error)
                .map_err(|_| ExecutionError::Storage)?;
        }
        return Err(ExecutionError::Profile);
    }
    Ok(task)
}
/// Context keeps the frozen baseline separate from submit-time permission flags.
pub struct RoundExecution {
    pub client: OpenCodeClient,
    pub root: PathBuf,
    pub server_port: Option<u16>,
    baseline: Option<RepositorySnapshot>,
    submit_snapshot: Option<Value>,
    allowed_paths: Vec<String>,
    trusted_roots: Vec<PathBuf>,
    externals: Vec<crate::completion::ExternalBaseline>,
    round: RoundRef,
}
impl fmt::Debug for RoundExecution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoundExecution { .. }")
    }
}
impl RoundExecution {
    pub(crate) fn round(&self) -> &RoundRef {
        &self.round
    }
    pub fn baseline_json(&self) -> Result<Option<Value>, ExecutionError> {
        self.baseline
            .as_ref()
            .map(|b| b.to_json().map_err(|_| ExecutionError::Baseline))
            .transpose()
    }
    pub(crate) fn task_and_root(
        &self,
        layout: &RustStateLayout,
    ) -> Result<(StorageConnection, Task), ExecutionError> {
        let storage = layout.open().map_err(|_| ExecutionError::Ownership)?;
        let task = storage
            .get_task(self.round.task_id)
            .map_err(|_| ExecutionError::Storage)?
            .ok_or(ExecutionError::Binding)?;
        if task.project_id != self.round.project_id
            || execution_root(&storage, layout, &task, false)? != self.root
        {
            return Err(ExecutionError::Binding);
        }
        let latest: u32 = storage
            .connection()
            .query_row(
                "SELECT MAX(round_number) FROM rounds WHERE task_id=?1",
                [task.task_id.to_string()],
                |r| r.get(0),
            )
            .map_err(|_| ExecutionError::Storage)?;
        if latest != self.round.round_number {
            return Err(ExecutionError::Round);
        }
        if task.snapshot != self.submit_snapshot || task.allowed_paths != self.allowed_paths {
            return Err(ExecutionError::Baseline);
        }
        for external in &self.externals {
            crate::completion::prove_path(&external.root)?;
        }
        for scope in self
            .allowed_paths
            .iter()
            .filter(|p| Path::new(p).is_absolute())
        {
            crate::completion::prove_path(Path::new(scope))?;
        }
        if mode(&storage, &task)? == ExecutionMode::Worktree {
            let record = storage
                .get_worktree(task.task_id, &task.project_id)
                .map_err(|_| ExecutionError::Storage)?
                .ok_or(ExecutionError::MissingRecord)?;
            let port = record.server_port.ok_or(ExecutionError::Runtime)?.get();
            let endpoint =
                bridge_config::Endpoint::loopback(port).map_err(|_| ExecutionError::Binding)?;
            if self.server_port != Some(port)
                || record.server_endpoint.as_deref() != Some(endpoint.url().as_str())
            {
                return Err(ExecutionError::Binding);
            }
            let saved: Value = serde_json::from_str(
                record
                    .baseline_json
                    .as_deref()
                    .ok_or(ExecutionError::Baseline)?,
            )
            .map_err(|_| ExecutionError::Baseline)?;
            if self.baseline_json()?.as_ref() != Some(&saved) {
                return Err(ExecutionError::Baseline);
            }
        }
        Ok((storage, task))
    }
    /// Dispatches the current round using the proven root and frozen profile.
    /// # Errors
    /// Propagates fail-closed dispatch guards without changing the main workspace.
    pub fn dispatch(
        &self,
        layout: &RustStateLayout,
    ) -> Result<crate::DispatchedRound, crate::DispatchError> {
        if self.round.round_number == 1 {
            crate::dispatch_initial_round(&self.client, layout, self.round.clone())
        } else {
            crate::dispatch_revision_round_with_trusted_roots(
                &self.client,
                layout,
                self.round.clone(),
                &self
                    .trusted_roots
                    .iter()
                    .map(PathBuf::as_path)
                    .collect::<Vec<_>>(),
            )
        }
    }
    /// Runs exactly saved test_commands in the execution root, persist-once.
    /// # Errors
    /// Refuses changed binding/baseline/current round before verifier spawn.
    pub fn verify(
        &self,
        layout: &RustStateLayout,
        timeout: Duration,
        tail_bytes: usize,
    ) -> Result<bridge_verifier::PersistedVerification, ExecutionError> {
        let (mut storage, task) = self.task_and_root(layout)?;
        let commands = task
            .test_commands
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let external_roots = self
            .externals
            .iter()
            .map(|e| e.root.as_path())
            .collect::<Vec<_>>();
        bridge_verifier::run_round_verification_persisted_with_repositories(
            &mut storage,
            self.round.clone(),
            &self.root,
            &external_roots,
            &commands,
            timeout,
            tail_bytes,
        )
        .map_err(|_| ExecutionError::Verifier)
    }
    /// Main-repository comparison in worktree cwd, with submit-time commit policy.
    /// # Errors
    /// Missing/corrupt frozen baseline or rebound execution root fails closed.
    pub fn collect_changes(
        &self,
        layout: &RustStateLayout,
    ) -> Result<RepositoryComparison, ExecutionError> {
        let (_, task) = self.task_and_root(layout)?;
        let baseline = self.baseline.as_ref().ok_or(ExecutionError::Baseline)?;
        let allow_commit = task
            .snapshot
            .as_ref()
            .is_some_and(|s| s["allow_commit"] == true);
        bridge_git::compare_repository_snapshot(
            &self.root,
            baseline,
            &task.allowed_paths,
            allow_commit,
            false,
        )
        .map_err(|_| ExecutionError::Git)
    }
    /// Collects the frozen main baseline and every affected external baseline.
    /// # Errors
    /// Rejects rebound state/paths; no baseline is refreshed from current files.
    pub fn collect_repositories(
        &self,
        layout: &RustStateLayout,
    ) -> Result<Vec<RepositoryComparison>, ExecutionError> {
        let (_, task) = self.task_and_root(layout)?;
        let mut repositories = vec![self.collect_changes(layout)?];
        let allow_commit = task
            .snapshot
            .as_ref()
            .is_some_and(|s| s["allow_commit"] == true);
        let allowed = task
            .allowed_paths
            .iter()
            .filter(|p| Path::new(p).is_absolute())
            .cloned()
            .collect::<Vec<_>>();
        for external in &self.externals {
            repositories.push(
                bridge_git::compare_repository_snapshot(
                    &external.root,
                    &external.snapshot,
                    &allowed,
                    allow_commit,
                    true,
                )
                .map_err(|_| ExecutionError::Git)?,
            );
        }
        self.task_and_root(layout)?;
        Ok(repositories)
    }
    /// Persists usage and checkpoint using the same checkout baseline and cwd.
    /// # Errors
    pub(crate) fn finish_unsent(
        &self,
        layout: &RustStateLayout,
        mut input: bridge_storage::FinishRoundInput,
    ) -> Result<bridge_storage::RoundUpdateOutcome, ExecutionError> {
        if input.round != self.round {
            return Err(ExecutionError::Round);
        }
        let (mut storage, task) = self.task_and_root(layout)?;
        let mut baseline = self.baseline_json()?;
        if let Some(baseline) = baseline.as_mut() {
            for key in [
                "allow_commit",
                "allow_dirty",
                "dirty_paths",
                "external_repositories",
            ] {
                if let Some(value) = task.snapshot.as_ref().and_then(|s| s.get(key)) {
                    baseline[key] = value.clone();
                }
            }
        }
        let checkpoint = crate::checkpoint::build_round_checkpoint(
            &storage,
            &input.round,
            &self.root,
            &self
                .trusted_roots
                .iter()
                .map(PathBuf::as_path)
                .collect::<Vec<_>>(),
            baseline.as_ref(),
        );
        let result = input.result_json.as_mut().ok_or(ExecutionError::Round)?;
        result["usage"] = crate::usage::observed_accounting(&[], "")["usage"].clone();
        storage
            .finish_worker_pre_send(input, checkpoint.as_ref())
            .map_err(|_| ExecutionError::Storage)
    }
    /// Revalidates task/root/baseline; ordinary atomic finish guards still apply.
    pub fn finish(
        &self,
        layout: &RustStateLayout,
        input: bridge_storage::FinishRoundInput,
        messages: &[bridge_opencode::Message],
    ) -> Result<bridge_storage::RoundUpdateOutcome, ExecutionError> {
        if input.round != self.round {
            return Err(ExecutionError::Round);
        }
        let (mut storage, task) = self.task_and_root(layout)?;
        let mut baseline = self.baseline_json()?;
        if let Some(baseline) = baseline.as_mut() {
            for key in [
                "allow_commit",
                "allow_dirty",
                "dirty_paths",
                "external_repositories",
            ] {
                if let Some(value) = task.snapshot.as_ref().and_then(|s| s.get(key)) {
                    baseline[key] = value.clone();
                }
            }
        }
        crate::checkpoint::finish_round_with_diagnostics(
            &mut storage,
            input,
            messages,
            &self.root,
            &self
                .trusted_roots
                .iter()
                .map(PathBuf::as_path)
                .collect::<Vec<_>>(),
            baseline.as_ref(),
        )
        .map_err(|_| ExecutionError::Storage)
    }
}
/// Creates/reuses the registered checkout and starts the proven task runtime.
/// Direct tasks keep their saved workspace and current configured client.
/// # Errors
/// Never recreates a lost `created` checkout or silently rebaselines it. All
/// unsupported scopes/repositories, stale rounds and corrupt profiles refuse.
#[allow(clippy::too_many_arguments)]
pub fn prepare_round_execution(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    round: RoundRef,
    layouts: &[&RustStateLayout],
    projects: &[&ProjectEntry],
    command: &ServerCommand,
    options: RuntimeOptions,
) -> Result<RoundExecution, ExecutionError> {
    let mut storage = layout.open().map_err(|_| ExecutionError::Ownership)?;
    let task = current_task(&mut storage, layout, project, &round, false)?;
    if mode(&storage, &task)? == ExecutionMode::Direct {
        let trusted_roots = project.auto_approve_external_directories().to_vec();
        let externals =
            crate::completion::external_baselines(&task, project.workspace(), &trusted_roots)?;
        let client = OpenCodeClient::from_project(project, options.request_timeout)
            .map_err(|_| ExecutionError::Runtime)?;
        let baseline = task
            .snapshot
            .as_ref()
            .filter(|s| s.get("manifest").is_some())
            .map(RepositorySnapshot::from_json)
            .transpose()
            .map_err(|_| ExecutionError::Baseline)?;
        return Ok(RoundExecution {
            client,
            root: project.workspace().to_path_buf(),
            server_port: None,
            baseline,
            submit_snapshot: task.snapshot.clone(),
            allowed_paths: task.allowed_paths.clone(),
            trusted_roots,
            externals,
            round,
        });
    }
    if task
        .allowed_paths
        .iter()
        .any(|p| Path::new(p).is_absolute())
        || task
            .snapshot
            .as_ref()
            .and_then(|s| s.get("external_repositories"))
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        || task
            .snapshot
            .as_ref()
            .is_some_and(|s| s.get("automation_parent").is_some())
    {
        return Err(ExecutionError::Unsupported);
    }
    let record = storage
        .get_worktree(task.task_id, &task.project_id)
        .map_err(|_| ExecutionError::Storage)?
        .ok_or(ExecutionError::MissingRecord)?;
    let base = record
        .base_head
        .as_deref()
        .ok_or(ExecutionError::MissingBase)?;
    if task.base_head.as_deref() != Some(base) {
        return Err(ExecutionError::Binding);
    }
    check_supported(project.workspace(), base).map_err(|_| ExecutionError::Unsupported)?;
    bridge_git::checkout::check_worktree_scopes(project.workspace(), &task.allowed_paths)
        .map_err(|_| ExecutionError::Unsupported)?;
    let paths = CheckoutPaths::new(&layout.project_dir(), task.task_id)
        .map_err(|_| ExecutionError::Binding)?;
    paths
        .require_checkout(Path::new(&record.path))
        .map_err(|_| ExecutionError::Binding)?;
    if record.runtime_dir.as_deref() != paths.runtime_dir.to_str() {
        return Err(ExecutionError::Binding);
    }
    let baseline = match record.status {
        WorktreeStatus::Pending | WorktreeStatus::Creating => {
            storage
                .update_worktree_status(
                    task.task_id,
                    &task.project_id,
                    WorktreeStatus::Creating,
                    None,
                )
                .map_err(|_| ExecutionError::Storage)?;
            if paths.checkout.exists() {
                remove_checkout(
                    project.workspace(),
                    &layout.project_dir(),
                    task.task_id,
                    &paths.checkout,
                )
                .map_err(|_| ExecutionError::Git)?;
            }
            create_checkout(
                project.workspace(),
                &layout.project_dir(),
                task.task_id,
                base,
            )
            .map_err(|_| ExecutionError::Git)?;
            let baseline =
                bridge_git::take_snapshot(&paths.checkout).map_err(|_| ExecutionError::Git)?;
            storage
                .update_worktree_baseline(
                    task.task_id,
                    &task.project_id,
                    &baseline
                        .to_json()
                        .map_err(|_| ExecutionError::Baseline)?
                        .to_string(),
                    Some(base),
                )
                .map_err(|_| ExecutionError::Storage)?;
            storage
                .update_worktree_status(
                    task.task_id,
                    &task.project_id,
                    WorktreeStatus::Created,
                    None,
                )
                .map_err(|_| ExecutionError::Storage)?;
            baseline
        }
        WorktreeStatus::Created => {
            if !paths.checkout.is_dir() {
                return Err(ExecutionError::MissingCheckout);
            }
            probe_checkout(
                project.workspace(),
                &layout.project_dir(),
                task.task_id,
                &paths.checkout,
                (round.round_number == 1).then_some(base),
            )
            .map_err(|_| ExecutionError::Binding)?;
            let value: Value = serde_json::from_str(
                record
                    .baseline_json
                    .as_deref()
                    .ok_or(ExecutionError::Baseline)?,
            )
            .map_err(|_| ExecutionError::Baseline)?;
            RepositorySnapshot::from_json(&value).map_err(|_| ExecutionError::Baseline)?
        }
        _ => return Err(ExecutionError::Binding),
    };
    drop(storage);
    let server = bridge_runtime::start_worktree_server(
        layout,
        project,
        task.task_id,
        layouts,
        projects,
        command,
        options,
    )
    .map_err(|_| ExecutionError::Runtime)?;
    layout
        .open()
        .map_err(|_| ExecutionError::Ownership)?
        .update_worktree_server(
            task.task_id,
            &task.project_id,
            &format!("http://127.0.0.1:{}", server.port),
            std::num::NonZeroU16::new(server.port).ok_or(ExecutionError::Runtime)?,
            paths.runtime_dir.join("opencode.process.json").to_str(),
        )
        .map_err(|_| ExecutionError::Storage)?;
    Ok(RoundExecution {
        client: server.client,
        root: paths.checkout,
        server_port: Some(server.port),
        baseline: Some(baseline),
        submit_snapshot: task.snapshot.clone(),
        allowed_paths: task.allowed_paths.clone(),
        trusted_roots: Vec::new(),
        externals: Vec::new(),
        round,
    })
}

/// Prepared execution with its worker fences held until the context is dropped.
/// The short admission lock is released before runtime/HTTP readiness probes.
pub struct FencedRoundExecution {
    pub execution: RoundExecution,
    pub(crate) _fences: crate::admission::WorkerFences,
}
impl fmt::Debug for FencedRoundExecution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FencedRoundExecution { .. }")
    }
}
/// Production entry point for B1/B2 execution. No waiting-task autoactivation.
/// # Errors
/// Busy locks, incompatible reservations and ordinary execution failures refuse.
#[allow(clippy::too_many_arguments)]
pub fn prepare_fenced_round_execution(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    round: RoundRef,
    layouts: &[&RustStateLayout],
    projects: &[&ProjectEntry],
    command: &ServerCommand,
    options: RuntimeOptions,
) -> Result<FencedRoundExecution, ExecutionError> {
    let fences = crate::admission::acquire_worker_fences(layout, project, round.task_id)
        .map_err(|_| ExecutionError::Binding)?
        .ok_or(ExecutionError::Round)?;
    let execution =
        prepare_round_execution(layout, project, round, layouts, projects, command, options)?;
    Ok(FencedRoundExecution {
        execution,
        _fences: fences,
    })
}

/// Reconstructs an attempted round from persisted evidence only. Caller holds
/// worker fences. No checkout/session creation, runtime start, prompt or HTTP.
/// # Errors
/// Requires a current open attempted round and intact saved root/baseline/profile.
pub fn resume_round_execution(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    round: RoundRef,
    timeout: Duration,
) -> Result<RoundExecution, ExecutionError> {
    let mut storage = layout.open().map_err(|_| ExecutionError::Ownership)?;
    let task = current_task(&mut storage, layout, project, &round, true)?;
    let worktree = mode(&storage, &task)? == ExecutionMode::Worktree;
    let root = execution_root(&storage, layout, &task, false)?;
    let view = crate::recovery::saved_view(&storage, layout, project, &task)
        .map_err(|_| ExecutionError::Binding)?;
    let (baseline, port, trusted_roots, externals) = if worktree {
        if task
            .allowed_paths
            .iter()
            .any(|p| Path::new(p).is_absolute())
            || task
                .snapshot
                .as_ref()
                .and_then(|s| s.get("external_repositories"))
                .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        {
            return Err(ExecutionError::Unsupported);
        }
        let record = storage
            .get_worktree(task.task_id, &task.project_id)
            .map_err(|_| ExecutionError::Storage)?
            .ok_or(ExecutionError::MissingRecord)?;
        let saved: Value = serde_json::from_str(
            record
                .baseline_json
                .as_deref()
                .ok_or(ExecutionError::Baseline)?,
        )
        .map_err(|_| ExecutionError::Baseline)?;
        (
            Some(RepositorySnapshot::from_json(&saved).map_err(|_| ExecutionError::Baseline)?),
            Some(record.server_port.ok_or(ExecutionError::Runtime)?.get()),
            Vec::new(),
            Vec::new(),
        )
    } else {
        let trusted = project.auto_approve_external_directories().to_vec();
        let externals = crate::completion::external_baselines(&task, &root, &trusted)?;
        let baseline = task
            .snapshot
            .as_ref()
            .filter(|s| s.get("manifest").is_some())
            .map(RepositorySnapshot::from_json)
            .transpose()
            .map_err(|_| ExecutionError::Baseline)?;
        (baseline, None, trusted, externals)
    };
    let execution = RoundExecution {
        client: OpenCodeClient::from_project(&view, timeout)
            .map_err(|_| ExecutionError::Runtime)?,
        root,
        server_port: port,
        baseline,
        submit_snapshot: task.snapshot.clone(),
        allowed_paths: task.allowed_paths.clone(),
        trusted_roots,
        externals,
        round,
    };
    current_task(&mut storage, layout, project, execution.round(), true)?;
    execution.task_and_root(layout)?;
    Ok(execution)
}
/// Acquires worker fences before reconstructing a previously attempted round.
/// # Errors
/// Busy/closed/stale/foreign rounds refuse without runtime or prompt operations.
pub fn resume_fenced_round_execution(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    round: RoundRef,
    timeout: Duration,
) -> Result<FencedRoundExecution, ExecutionError> {
    let fences = crate::admission::acquire_worker_fences(layout, project, round.task_id)
        .map_err(|_| ExecutionError::Binding)?
        .ok_or(ExecutionError::Round)?;
    let execution = resume_round_execution(layout, project, round, timeout)?;
    Ok(FencedRoundExecution {
        execution,
        _fences: fences,
    })
}
