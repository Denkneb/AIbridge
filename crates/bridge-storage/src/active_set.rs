//! One read transaction for the complete unfinished set and reservation ledger.
use super::{StorageConnection, Task, WriterError, WriterReservation};
use bridge_domain::{ExecutionMode, ProjectId, TaskId, TaskStatus};
use std::{error::Error, fmt};

/// Content-free task card. Detail readers load task text/snapshot separately.
#[derive(Clone, PartialEq, Eq)]
pub struct ActiveTaskSummary {
    pub task_id: TaskId,
    pub project_id: ProjectId,
    pub status: TaskStatus,
    pub execution_mode: ExecutionMode,
    pub created_at: String,
    pub updated_at: String,
}
impl fmt::Debug for ActiveTaskSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ActiveTaskSummary { .. }")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveSetError {
    Database,
    TaskData,
    ReservationData,
}
impl fmt::Display for ActiveSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Database => "active set storage unavailable",
            Self::TaskData => "active set task data invalid",
            Self::ReservationData => "active set reservation data invalid",
        })
    }
}
impl Error for ActiveSetError {}

/// Writers precede waiters, then created_at/task_id; reservations are oldest first.
#[derive(Debug)]
pub struct ActiveSet {
    pub tasks: Vec<ActiveTaskSummary>,
    pub reservations: Vec<WriterReservation>,
    pub writer_count: usize,
    pub waiting_count: usize,
}
impl ActiveSet {
    #[must_use]
    pub fn unfinished_count(&self) -> usize {
        self.tasks.len()
    }
    /// Conservative delivery/diagnostic fence, including orphan reservations.
    #[must_use]
    pub fn writer_activity_present(&self) -> bool {
        self.writer_count > 0 || !self.reservations.is_empty()
    }
    /// Never silently chooses the oldest task when more than one is unfinished.
    #[must_use]
    pub fn resolve_without_id(&self) -> TaskSelection {
        match self.tasks.as_slice() {
            [] => TaskSelection::Missing,
            [task] => TaskSelection::Selected(task.clone()),
            tasks => TaskSelection::Ambiguous(tasks.to_vec()),
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum TaskSelection {
    Missing,
    Selected(ActiveTaskSummary),
    Ambiguous(Vec<ActiveTaskSummary>),
}
impl TaskSelection {
    /// Stable public error code for an ambiguous ID-less status request.
    #[must_use]
    pub fn error_code(&self) -> Option<&'static str> {
        match self {
            Self::Missing => Some("task_not_found"),
            Self::Ambiguous(_) => Some("ambiguous_task"),
            Self::Selected(_) => None,
        }
    }
}
fn summary(task: Task, mode: String) -> Result<ActiveTaskSummary, ActiveSetError> {
    Ok(ActiveTaskSummary {
        task_id: task.task_id,
        project_id: task.project_id,
        status: task.status,
        execution_mode: serde_json::from_value(serde_json::json!(mode))
            .map_err(|_| ActiveSetError::TaskData)?,
        created_at: task.created_at,
        updated_at: task.updated_at,
    })
}
impl StorageConnection {
    /// Reads every unfinished task and reservation in one SQLite snapshot.
    /// No initialization, reconcile, activation or pagination occurs here.
    /// # Errors
    /// Corrupt task/mode/reservation data fails closed; no partial set is returned.
    pub fn active_set(&self, project: &ProjectId) -> Result<ActiveSet, ActiveSetError> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(|_| ActiveSetError::Database)?;
        let tasks = {
            let mut statement=tx.prepare("SELECT * FROM tasks WHERE project_id=?1 AND status NOT IN ('accepted','closed') ORDER BY CASE WHEN status='waiting_dependencies' THEN 1 ELSE 0 END, created_at, task_id")
                .map_err(|_|ActiveSetError::Database)?;
            let rows = statement
                .query_map([project.as_str()], |r| {
                    Ok((
                        crate::map_task_runtime(&self.connection, r),
                        r.get::<_, String>("execution_mode")?,
                    ))
                })
                .map_err(|_| ActiveSetError::Database)?;
            let mut tasks = Vec::new();
            for row in rows {
                let (task, mode) = row.map_err(|_| ActiveSetError::TaskData)?;
                tasks.push(summary(task.map_err(|_| ActiveSetError::TaskData)?, mode)?);
            }
            tasks
        };
        let reservations = self
            .get_active_writers(project)
            .map_err(|error| match error {
                WriterError::Database(_) => ActiveSetError::Database,
                _ => ActiveSetError::ReservationData,
            })?;
        tx.commit().map_err(|_| ActiveSetError::Database)?;
        let waiting_count = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::WaitingDependencies)
            .count();
        let writer_count = tasks.len() - waiting_count;
        Ok(ActiveSet {
            tasks,
            reservations,
            writer_count,
            waiting_count,
        })
    }
    /// Resolves ID-less status through the full set. An explicit ID may select
    /// a terminal task, but never a task belonging to another project.
    /// # Errors
    /// Invalid row data/database failures return fixed, content-free errors.
    pub fn resolve_status_task(
        &self,
        project: &ProjectId,
        id: Option<TaskId>,
    ) -> Result<TaskSelection, ActiveSetError> {
        let Some(id) = id else {
            return Ok(self.active_set(project)?.resolve_without_id());
        };
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(|_| ActiveSetError::Database)?;
        let task = self.get_task(id).map_err(|_| ActiveSetError::TaskData)?;
        let selected = if let Some(task) = task.filter(|t| &t.project_id == project) {
            let mode = tx
                .query_row(
                    "SELECT execution_mode FROM tasks WHERE task_id=?1",
                    [id.to_string()],
                    |r| r.get(0),
                )
                .map_err(|_| ActiveSetError::Database)?;
            TaskSelection::Selected(summary(task, mode)?)
        } else {
            TaskSelection::Missing
        };
        tx.commit().map_err(|_| ActiveSetError::Database)?;
        Ok(selected)
    }
}
