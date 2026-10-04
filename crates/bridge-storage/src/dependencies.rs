//! Explicit dependency-gate transition and fenced execution-baseline refresh.

use std::{error::Error, fmt};

use bridge_domain::{ProjectId, TaskId, TaskStatus, TaskTransitionEvent};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{
    AdmissionSettings, StorageConnection, Task, TaskRowError, WriterError, utc_now_rfc3339_millis,
    writers,
};

/// Safe errors for dependency activation and baseline refresh.
#[non_exhaustive]
pub enum DependencyUpdateError {
    /// Writer admission or reservation persistence failed.
    Writer(WriterError),
    /// The supplied baseline snapshot is not a JSON object.
    InvalidSnapshot,
    /// The dependency event cannot perform the requested task transition.
    InvalidTransition,
    /// The waiting task cannot be mapped through the validated task contract.
    TaskRow(TaskRowError),
    /// An unexpected SQLite failure; details are available only through source.
    Database(rusqlite::Error),
}

impl fmt::Display for DependencyUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Writer(_) => "dependency writer admission failed",
            Self::InvalidSnapshot => "dependency baseline snapshot is invalid",
            Self::InvalidTransition => "dependency task transition is invalid",
            Self::TaskRow(_) => "dependency task row could not be mapped",
            Self::Database(_) => "dependency storage update failed",
        })
    }
}

impl fmt::Debug for DependencyUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Error for DependencyUpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Writer(error) => Some(error),
            Self::TaskRow(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl StorageConnection {
    /// Refreshes the baseline of a task still waiting in this project.
    ///
    /// The snapshot must be a JSON object. Only snapshot, base head and the
    /// updated timestamp change; no event or round is written. The conditional
    /// UPDATE cannot overwrite a baseline after activation or close. Returns
    /// false for a missing, other-project or no-longer-waiting task.
    ///
    /// # Errors
    /// Returns [`DependencyUpdateError`] for invalid snapshot input or SQLite
    /// failures, without rendering input text, identifiers or paths.
    pub fn refresh_task_baseline(
        &mut self,
        task_id: TaskId,
        project_id: &ProjectId,
        snapshot: &serde_json::Value,
        base_head: Option<&str>,
    ) -> Result<bool, DependencyUpdateError> {
        if !snapshot.is_object() {
            return Err(DependencyUpdateError::InvalidSnapshot);
        }
        let snapshot = snapshot.to_string();
        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(DependencyUpdateError::Database)?;
        let changed = transaction
            .execute(
                "UPDATE tasks SET snapshot=?1, base_head=?2, updated_at=?3 \
             WHERE task_id=?4 AND project_id=?5 AND status='waiting_dependencies' AND close_requested_at IS NULL",
                params![
                    snapshot,
                    base_head,
                    now,
                    task_id.to_string(),
                    project_id.as_str()
                ],
            )
            .map_err(DependencyUpdateError::Database)?;
        transaction
            .commit()
            .map_err(DependencyUpdateError::Database)?;
        Ok(changed == 1)
    }

    /// Explicitly activates a dependency-gated task in a schema v15 database.
    ///
    /// The caller must first establish that dependencies are accepted. Opening
    /// state alone never invokes this transition. One BEGIN IMMEDIATE contains
    /// the writer check, conditional status UPDATE and dependencies_satisfied
    /// event. Exactly one racing caller returns true; repeats, missing tasks,
    /// other projects and other statuses return false without writes.
    ///
    /// This default call preserves single-writer behavior. Another writer task
    /// or reservation blocks activation. An existing valid reservation owned by
    /// this task is reused; otherwise one is inserted atomically. No round is
    /// created or attempted and no worker is spawned.
    ///
    /// # Errors
    /// Corrupt waiting rows, invalid domain transitions and SQLite failures
    /// return [`DependencyUpdateError`]. The status and event roll back together.
    pub fn activate_waiting_dependencies(
        &mut self,
        task_id: TaskId,
        project_id: &ProjectId,
    ) -> Result<bool, DependencyUpdateError> {
        self.activate_waiting_dependencies_with_admission(
            task_id,
            project_id,
            &AdmissionSettings::default(),
        )
    }

    /// Activates using validated writer settings; scope and reservation checks
    /// share the status/event transaction. Parallel mode requires a worktree task.
    /// # Errors
    /// Invalid config or SQLite failures are safe typed errors. Busy/overlap/
    /// malformed normalized scopes refuse activation without any writes.
    pub fn activate_waiting_dependencies_with_admission(
        &mut self,
        task_id: TaskId,
        project_id: &ProjectId,
        settings: &AdmissionSettings,
    ) -> Result<bool, DependencyUpdateError> {
        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(DependencyUpdateError::Database)?;
        let waiting = transaction.query_row(
            "SELECT * FROM tasks WHERE task_id=?1 AND project_id=?2 AND status='waiting_dependencies' AND close_requested_at IS NULL",
            params![task_id.to_string(), project_id.as_str()],
            |row| Ok(Task::from_row(row)),
        ).optional().map_err(DependencyUpdateError::Database)?;
        let Some(waiting) = waiting else {
            return Ok(false);
        };
        let waiting = waiting.map_err(DependencyUpdateError::TaskRow)?;
        let status = waiting
            .status
            .transition_on(TaskTransitionEvent::DependenciesSatisfied)
            .map_err(|_| DependencyUpdateError::InvalidTransition)?;
        debug_assert_eq!(status, TaskStatus::Implementing);
        let mode: String = transaction
            .query_row(
                "SELECT execution_mode FROM tasks WHERE task_id=?1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .map_err(DependencyUpdateError::Database)?;
        if settings.allow_parallel_writers() && mode != "worktree" {
            return Err(DependencyUpdateError::Writer(WriterError::InvalidSettings));
        }
        let raw = serde_json::to_string(&waiting.allowed_paths)
            .map_err(|_| DependencyUpdateError::Writer(WriterError::ScopeDataError))?;
        let scopes = match writers::parse_scopes(&raw) {
            Ok(scopes) => scopes,
            Err(_) => return Ok(false),
        };
        if let Err(error) = writers::check_admission(
            &transaction,
            project_id.as_str(),
            &scopes,
            std::path::Path::new(&waiting.workspace),
            settings.allow_parallel_writers(),
            &task_id.to_string(),
        ) {
            return match error {
                WriterError::ProjectBusy
                | WriterError::ScopeOverlap
                | WriterError::ScopeDataError => Ok(false),
                error => Err(DependencyUpdateError::Writer(error)),
            };
        }
        let changed = transaction.execute(
            "UPDATE tasks SET status=?1, updated_at=?2 WHERE task_id=?3 AND project_id=?4 AND status='waiting_dependencies' AND close_requested_at IS NULL",
            params![status.as_str(), now, task_id.to_string(), project_id.as_str()],
        ).map_err(DependencyUpdateError::Database)?;
        if changed != 1 {
            return Ok(false);
        }
        if let Err(error) = writers::reserve(
            &transaction,
            &task_id.to_string(),
            project_id.as_str(),
            &raw,
            &now,
            settings.allow_parallel_writers(),
        ) {
            return match error {
                WriterError::ProjectBusy | WriterError::ScopeDataError => Ok(false),
                error => Err(DependencyUpdateError::Writer(error)),
            };
        }
        transaction.execute(
            "INSERT INTO events(task_id,round_number,kind,message,created_at) VALUES (?1,1,'dependencies_satisfied','accepted dependencies unlocked the task',?2)",
            params![task_id.to_string(), now],
        ).map_err(DependencyUpdateError::Database)?;
        transaction
            .commit()
            .map_err(DependencyUpdateError::Database)?;
        Ok(true)
    }
}
