//! Worker startup, dispatch, observation and cooperative close in owned state.
use crate::{
    DispatchErrorKind, WorkerLock, WorkerLockOutcome,
    execution::{self, ExecutionError, FencedRoundExecution, RoundExecution},
    lifecycle::{RecoveryOutcome, recover_worktree_task},
    observation::RoundObserver,
    observation_loop::ObservationSettings,
};
use bridge_config::ProjectEntry;
use bridge_domain::{RoundStatus, TaskStatus};
use bridge_opencode::{SessionError, TransportError};
use bridge_runtime::{RuntimeOptions, ServerCommand};
use bridge_storage::{FinishRoundInput, RoundRef, RoundRow, RustStateLayout, Task};
use rusqlite::OptionalExtension;
use serde_json::json;
use std::{
    fmt,
    path::Path,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy)]
pub struct WorkerSettings {
    pub deadline: Duration,
    pub delivery_grace: Duration,
    pub http_timeout: Duration,
    pub observation: ObservationSettings,
}
impl Default for WorkerSettings {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(3600),
            delivery_grace: Duration::from_secs(30),
            http_timeout: Duration::from_secs(30),
            observation: ObservationSettings::default(),
        }
    }
}
impl WorkerSettings {
    /// Frozen AB_* names. Missing/unparseable values use defaults; invalid
    /// numeric durations refuse instead of panicking or starting a busy loop.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, WorkerRunError> {
        let defaults = Self::default();
        let duration = |name, fallback: Duration, zero_allowed: bool| {
            let seconds = lookup(name)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(fallback.as_secs_f64());
            if !seconds.is_finite() || seconds < 0.0 || (!zero_allowed && seconds < 0.001) {
                return Err(WorkerRunError::Settings);
            }
            Duration::try_from_secs_f64(seconds).map_err(|_| WorkerRunError::Settings)
        };
        Ok(Self {
            deadline: duration("AB_ROUND_DEADLINE", defaults.deadline, false)?,
            delivery_grace: duration("AB_DELIVERY_GRACE", defaults.delivery_grace, true)?,
            http_timeout: duration("AB_HTTP_TIMEOUT", defaults.http_timeout, false)?,
            observation: ObservationSettings {
                poll_interval: duration(
                    "AB_POLL_INTERVAL",
                    defaults.observation.poll_interval,
                    false,
                )?,
                stale_blocker_grace: duration(
                    "AB_STALE_BLOCKER_GRACE",
                    defaults.observation.stale_blocker_grace,
                    true,
                )?,
                ..defaults.observation
            },
        })
    }
    fn validate(self) -> Result<(), WorkerRunError> {
        if self.deadline < Duration::from_millis(1)
            || self.http_timeout.is_zero()
            || self.observation.poll_interval.is_zero()
            || self.observation.verification_timeout.is_zero()
        {
            return Err(WorkerRunError::Settings);
        }
        // Storage's wall-clock deadline must be representable too.
        std::time::SystemTime::now()
            .checked_add(self.deadline)
            .ok_or(WorkerRunError::Settings)?;
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerOutcome {
    Skipped,
    Busy,
    Finished,
    Closed,
    CloseDeferred,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRunError {
    Settings,
    Ownership,
    Binding,
    UnknownTask,
    UnknownRound,
    StaleRound,
    Storage,
    Execution(ExecutionError),
    Dispatch(DispatchErrorKind),
}
impl WorkerRunError {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::UnknownTask | Self::UnknownRound | Self::StaleRound => 2,
            _ => 1,
        }
    }
}
impl fmt::Display for WorkerRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Settings => "worker settings invalid",
            Self::Ownership => "worker state unowned",
            Self::Binding => "worker binding mismatch",
            Self::UnknownTask => "worker task unknown",
            Self::UnknownRound => "worker round unknown",
            Self::StaleRound => "worker round not current",
            Self::Storage => "worker storage error",
            Self::Execution(e) => e.as_str(),
            Self::Dispatch(e) => e.as_str(),
        })
    }
}
impl std::error::Error for WorkerRunError {}
impl From<ExecutionError> for WorkerRunError {
    fn from(e: ExecutionError) -> Self {
        Self::Execution(e)
    }
}
fn task(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    round: &RoundRef,
) -> Result<Task, WorkerRunError> {
    if layout.project_id() != project.id()
        || project.id() != &round.project_id
        || round.round_number == 0
    {
        return Err(WorkerRunError::Binding);
    }
    let task = layout
        .open()
        .map_err(|_| WorkerRunError::Ownership)?
        .get_task(round.task_id)
        .map_err(|_| WorkerRunError::Storage)?
        .ok_or(WorkerRunError::UnknownTask)?;
    if task.project_id != round.project_id || Path::new(&task.workspace) != project.workspace() {
        return Err(WorkerRunError::Binding);
    }
    Ok(task)
}
fn row(layout: &RustStateLayout, round: &RoundRef) -> Result<RoundRow, WorkerRunError> {
    let storage = layout.open().map_err(|_| WorkerRunError::Ownership)?;
    let row = storage
        .connection()
        .query_row(
            "SELECT * FROM rounds WHERE task_id=?1 AND round_number=?2",
            rusqlite::params![round.task_id.to_string(), round.round_number],
            |r| Ok(RoundRow::from_row(r)),
        )
        .optional()
        .map_err(|_| WorkerRunError::Storage)?
        .ok_or(WorkerRunError::UnknownRound)?
        .map_err(|_| WorkerRunError::Storage)?;
    if row.project_id != round.project_id {
        return Err(WorkerRunError::Binding);
    }
    let latest: u32 = storage
        .connection()
        .query_row(
            "SELECT MAX(round_number) FROM rounds WHERE task_id=?1",
            [round.task_id.to_string()],
            |r| r.get(0),
        )
        .map_err(|_| WorkerRunError::Storage)?;
    if latest != round.round_number {
        return Err(WorkerRunError::StaleRound);
    }
    Ok(row)
}
fn pending_guard(
    execution: &RoundExecution,
    layout: &RustStateLayout,
) -> Result<RoundRow, WorkerRunError> {
    let (_, task) = execution.task_and_root(layout)?;
    let row = row(layout, execution.round())?;
    if task.close_requested_at.is_some()
        || row.status != RoundStatus::Pending
        || row.attempted
        || !matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising)
    {
        return Err(WorkerRunError::Binding);
    }
    Ok(row)
}
fn finish_pending(
    execution: &RoundExecution,
    layout: &RustStateLayout,
    status: TaskStatus,
    code: &str,
) -> Result<WorkerOutcome, WorkerRunError> {
    pending_guard(execution, layout)?;
    let mut result = if execution.baseline_json()?.is_some() {
        crate::completion::collection_json(&execution.collect_repositories(layout)?)?
    } else {
        json!({})
    };
    if status == TaskStatus::NeedsUser {
        result["blockers"] = json!([{"type":code}]);
    } else {
        result["error"] = json!(code);
    }
    pending_guard(execution, layout)?;
    execution.finish_unsent(
        layout,
        FinishRoundInput {
            round: execution.round().clone(),
            round_status: if status == TaskStatus::Failed {
                RoundStatus::Failed
            } else {
                RoundStatus::NeedsUser
            },
            task_status: status,
            response: None,
            response_message_id: None,
            error_code: Some(code.to_owned()),
            result_json: Some(result),
        },
    )?;
    Ok(WorkerOutcome::Finished)
}
fn pause_pending(
    execution: &RoundExecution,
    layout: &RustStateLayout,
    interval: Duration,
) -> Result<(), WorkerRunError> {
    let start = Instant::now();
    while start.elapsed() < interval {
        pending_guard(execution, layout)?;
        thread::sleep(
            interval
                .saturating_sub(start.elapsed())
                .min(Duration::from_millis(100)),
        );
    }
    Ok(())
}
fn dispatch_checked(
    context: &FencedRoundExecution,
    layout: &RustStateLayout,
    settings: WorkerSettings,
    start: Instant,
) -> Result<WorkerOutcome, WorkerRunError> {
    let execution = &context.execution;
    pending_guard(execution, layout)?;
    let identity = execution.client.verify_workspace();
    pending_guard(execution, layout)?;
    if identity.is_err() {
        return finish_pending(execution, layout, TaskStatus::Failed, "workspace_mismatch");
    }
    let resolved =
        crate::resolve_round_session(&execution.client, layout, execution.round().clone());
    pending_guard(execution, layout)?;
    let resolved = match resolved {
        Ok(s) => s,
        Err(e) => {
            use crate::SessionResolutionErrorKind as K;
            let code = match e.kind() {
                K::SessionAmbiguous => "session_ambiguous",
                K::SessionUnknown => "session_unknown",
                K::SessionDirectoryMismatch => {
                    return finish_pending(
                        execution,
                        layout,
                        TaskStatus::Failed,
                        "session_directory_mismatch",
                    );
                }
                _ => return Err(WorkerRunError::Binding),
            };
            return finish_pending(execution, layout, TaskStatus::NeedsUser, code);
        }
    };
    loop {
        if pending_guard(execution, layout)?.session_id.as_deref() != Some(resolved.id()) {
            return Err(WorkerRunError::Binding);
        }
        let session = execution.client.get_session(resolved.id());
        pending_guard(execution, layout)?;
        match session {
            Ok(s) => {
                if s.id() != Some(resolved.id())
                    || !s.directory().is_some_and(|d| {
                        crate::session::directory_matches_workspace(d, &execution.root)
                    })
                {
                    return finish_pending(
                        execution,
                        layout,
                        TaskStatus::Failed,
                        "session_directory_mismatch",
                    );
                }
                break;
            }
            Err(SessionError::Transport(TransportError::NotFound)) => {
                return finish_pending(execution, layout, TaskStatus::Failed, "session_not_found");
            }
            Err(_) if start.elapsed() > settings.deadline => {
                return finish_pending(execution, layout, TaskStatus::NeedsUser, "transient_error");
            }
            Err(_) => pause_pending(execution, layout, settings.observation.poll_interval)?,
        }
    }
    match execution.dispatch(layout) {
        Ok(_) => {}
        Err(e) if e.kind() == DispatchErrorKind::Delivery => {} // Sent: observe, never resend.
        Err(e) => return Err(WorkerRunError::Dispatch(e.kind())),
    }
    Ok(WorkerOutcome::Finished) // Dispatch complete; caller now observes.
}
fn close(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    round: &RoundRef,
    layouts: &[&RustStateLayout],
) -> Result<WorkerOutcome, WorkerRunError> {
    match recover_worktree_task(layout, project, round.task_id, layouts)? {
        RecoveryOutcome::Closed => return Ok(WorkerOutcome::Closed),
        RecoveryOutcome::Direct => {}
        _ => {
            return Ok(
                if task(layout, project, round)?.status == TaskStatus::Closed {
                    WorkerOutcome::Closed
                } else {
                    WorkerOutcome::CloseDeferred
                },
            );
        }
    }
    let _admission =
        match WorkerLock::try_acquire_admission(layout).map_err(|_| WorkerRunError::Ownership)? {
            WorkerLockOutcome::Busy => return Ok(WorkerOutcome::CloseDeferred),
            WorkerLockOutcome::Acquired(g) => g,
        };
    let Some(_fences) =
        crate::admission::acquire_lifecycle_fences(layout, project, round.task_id, false)
            .map_err(|_| WorkerRunError::Binding)?
    else {
        return Ok(WorkerOutcome::CloseDeferred);
    };
    let current = task(layout, project, round)?;
    if current.close_requested_at.is_none() {
        return Ok(WorkerOutcome::Skipped);
    }
    layout
        .open()
        .map_err(|_| WorkerRunError::Ownership)?
        .complete_requested_close(round.task_id)
        .map_err(|_| WorkerRunError::Storage)?;
    Ok(WorkerOutcome::Closed)
}

/// Runs existing Rust-owned state only; parked tasks need explicit recovery
/// claims. Fences cover startup writes, dispatch, observation and verifier.
/// Close cleanup runs after dropping them, acquiring lifecycle fences anew.
/// # Errors
/// Foreign/missing/stale state and service failures return fixed diagnostics.
#[allow(clippy::too_many_arguments)]
pub fn run_worker(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    round: RoundRef,
    layouts: &[&RustStateLayout],
    projects: &[&ProjectEntry],
    command: &ServerCommand,
    mut runtime: RuntimeOptions,
    settings: WorkerSettings,
) -> Result<WorkerOutcome, WorkerRunError> {
    settings.validate()?;
    let current = task(layout, project, &round)?;
    if current.close_requested_at.is_some() {
        return close(layout, project, &round, layouts);
    }
    if current.status.is_terminal() || current.status == TaskStatus::WaitingDependencies {
        return Ok(WorkerOutcome::Skipped);
    }
    row(layout, &round)?;
    if !matches!(
        current.status,
        TaskStatus::Implementing | TaskStatus::Revising
    ) {
        return Ok(WorkerOutcome::Skipped);
    }
    let Some(fences) = crate::admission::acquire_worker_fences(layout, project, round.task_id)
        .map_err(|_| WorkerRunError::Binding)?
    else {
        if task(layout, project, &round)?.close_requested_at.is_some() {
            return close(layout, project, &round, layouts);
        }
        return Ok(WorkerOutcome::Busy);
    };
    let result = (|| {
        let current = task(layout, project, &round)?;
        let saved = row(layout, &round)?;
        if current.close_requested_at.is_some() {
            return Err(WorkerRunError::Binding);
        }
        if !saved.status.is_open()
            || !matches!(
                current.status,
                TaskStatus::Implementing | TaskStatus::Revising
            )
        {
            return Ok(WorkerOutcome::Skipped);
        }
        layout
            .open()
            .map_err(|_| WorkerRunError::Ownership)?
            .mark_worker_started(round.clone(), settings.deadline.as_secs_f64())
            .map_err(|_| WorkerRunError::Storage)?;
        runtime.request_timeout = settings.http_timeout;
        let execution = if saved.attempted {
            execution::resume_round_execution(
                layout,
                project,
                round.clone(),
                settings.http_timeout,
            )?
        } else {
            execution::prepare_round_execution(
                layout,
                project,
                round.clone(),
                layouts,
                projects,
                command,
                runtime,
            )?
        };
        let context = FencedRoundExecution {
            execution,
            _fences: fences,
        };
        let start = Instant::now();
        if !saved.attempted {
            dispatch_checked(&context, layout, settings, start)?;
            if !row(layout, &round)?.attempted {
                return Ok(WorkerOutcome::Finished);
            }
        }
        let mut observer =
            RoundObserver::new(&context, layout, settings.delivery_grace, settings.deadline)?;
        // Initial dispatch just proved root/session. Recovery proves them in
        // poll_verified before its first message request.
        if !saved.attempted {
            observer.workspace_verified = true;
            observer.identity_verified = true;
        }
        observer.observe_from(project, settings.observation, start)?;
        Ok(WorkerOutcome::Finished)
    })();
    // The closure consumed/dropped the fences even on preparation failure.
    if task(layout, project, &round)?.close_requested_at.is_some() {
        return close(layout, project, &round, layouts);
    }
    result
}
