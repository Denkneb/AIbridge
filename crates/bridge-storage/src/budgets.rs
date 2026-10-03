//! Validated optional task budgets; usage aggregation and round gates belong to consumers.
use super::{Task, TaskRowError, open_read_only_current, validate_database};
use bridge_domain::{ProjectId, TaskId};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::{error::Error, fmt, path::Path};

pub const DEFAULT_BUDGET_WARNING_THRESHOLD: f64 = 0.8;
pub const BUDGET_USAGE_FIELDS: &[&str] = &[
    "input",
    "output",
    "reasoning",
    "cache_read",
    "cache_write",
    "cost",
];

/// Immutable validated JSON. Integer and floating limit representations are preserved.
#[derive(Clone, PartialEq)]
pub struct TaskBudget(Value);
impl fmt::Debug for TaskBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TaskBudget { .. }")
    }
}
impl TaskBudget {
    /// Validates an object, rejecting unknown fields and nonpositive/nonfinite limits.
    pub fn from_json(value: &Value) -> Result<Self, BudgetValidationError> {
        let object = value.as_object().ok_or(BudgetValidationError)?;
        if object
            .keys()
            .any(|k| !matches!(k.as_str(), "limits" | "warning_threshold"))
        {
            return Err(BudgetValidationError);
        }
        let limits = object
            .get("limits")
            .and_then(Value::as_object)
            .filter(|m| !m.is_empty())
            .ok_or(BudgetValidationError)?;
        for (key, value) in limits {
            if !BUDGET_USAGE_FIELDS.contains(&key.as_str()) || !positive(value) {
                return Err(BudgetValidationError);
            }
        }
        let threshold = object
            .get("warning_threshold")
            .cloned()
            .unwrap_or_else(|| json!(DEFAULT_BUDGET_WARNING_THRESHOLD));
        if !positive(&threshold) || threshold.as_f64().is_none_or(|t| t > 1.0) {
            return Err(BudgetValidationError);
        }
        Ok(Self(json!({"limits":limits,"warning_threshold":threshold})))
    }
    pub fn as_json(&self) -> &Value {
        &self.0
    }
    pub fn warning_threshold(&self) -> f64 {
        self.0["warning_threshold"]
            .as_f64()
            .expect("validated threshold")
    }
}
fn positive(v: &Value) -> bool {
    v.as_f64().is_some_and(|n| n.is_finite() && n > 0.0)
}
/// Fixed error text never renders caller keys or JSON payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetValidationError;
impl fmt::Display for BudgetValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("task budget is invalid")
    }
}
impl Error for BudgetValidationError {}
/// Public submission contract: JSON null means no optional budget.
pub fn validate_budget(value: &Value) -> Result<Option<TaskBudget>, BudgetValidationError> {
    if value.is_null() {
        Ok(None)
    } else {
        TaskBudget::from_json(value).map(Some)
    }
}
/// SQL NULL means no budget; every non-NULL value must decode to a valid object.
pub fn normalize_persisted_budget(
    raw: Option<&str>,
) -> Result<Option<TaskBudget>, BudgetValidationError> {
    raw.map(|s| {
        serde_json::from_str::<Value>(s)
            .map_err(|_| BudgetValidationError)
            .and_then(|v| TaskBudget::from_json(&v))
    })
    .transpose()
}

#[non_exhaustive]
pub enum BudgetReadError {
    IncompatibleState,
    TaskRow(TaskRowError),
    Database(rusqlite::Error),
}
impl fmt::Display for BudgetReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IncompatibleState => "budget state is unreadable or incompatible",
            Self::TaskRow(_) => "budget task row is invalid",
            Self::Database(_) => "budget storage read failed",
        })
    }
}
impl fmt::Debug for BudgetReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl Error for BudgetReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TaskRow(e) => Some(e),
            Self::Database(e) => Some(e),
            _ => None,
        }
    }
}
/// Reads through mode=ro and one consistent snapshot, without creating or migrating.
/// Outer None is a missing task/database; Some(None) is a task without a budget.
/// A malformed stored budget is an error. SQLite may touch transient WAL/SHM files.
pub fn read_task_budget_readonly(
    path: &Path,
    task: TaskId,
    project: &ProjectId,
) -> Result<Option<Option<TaskBudget>>, BudgetReadError> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(BudgetReadError::IncompatibleState),
        Ok(_) => {}
    }
    let mut c = open_read_only_current(path).map_err(|_| BudgetReadError::IncompatibleState)?;
    let tx = c.transaction().map_err(BudgetReadError::Database)?;
    validate_database(&tx).map_err(|_| BudgetReadError::IncompatibleState)?;
    let result = tx
        .query_row(
            "SELECT * FROM tasks WHERE task_id=?1 AND project_id=?2",
            rusqlite::params![task.to_string(), project.as_str()],
            |r| Ok(Task::from_row(r)),
        )
        .optional()
        .map_err(BudgetReadError::Database)?
        .transpose()
        .map_err(BudgetReadError::TaskRow)?
        .map(|t| t.budget);
    tx.commit().map_err(BudgetReadError::Database)?;
    Ok(result)
}
