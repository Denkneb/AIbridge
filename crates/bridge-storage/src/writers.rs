//! Atomic admission and fail-closed writer scope identity.
use super::{StorageConnection, Task, TaskRowError, utc_now_rfc3339_millis};
use bridge_domain::{DeliveryMode, ExecutionMode, ProjectId, TaskId, TaskStatus};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
};

/// Validated project settings; defaults preserve the historical single task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionSettings {
    max_active_tasks: u64,
    parallel: bool,
    mode: ExecutionMode,
    delivery_mode: DeliveryMode,
}
impl Default for AdmissionSettings {
    fn default() -> Self {
        Self {
            max_active_tasks: 1,
            parallel: false,
            mode: ExecutionMode::Direct,
            delivery_mode: DeliveryMode::Manual,
        }
    }
}
impl AdmissionSettings {
    /// Builds settings from validated config values, retaining the worktree gate.
    /// # Errors
    /// Rejects zero task bounds and parallel writers outside worktree mode.
    pub fn new(
        max_active_tasks: u64,
        allow_parallel_writers: bool,
        mode: ExecutionMode,
    ) -> Result<Self, WriterError> {
        if max_active_tasks == 0 || (allow_parallel_writers && mode != ExecutionMode::Worktree) {
            return Err(WriterError::InvalidSettings);
        }
        Ok(Self {
            max_active_tasks,
            parallel: allow_parallel_writers,
            mode,
            delivery_mode: DeliveryMode::Manual,
        })
    }
    /// Pins submit-time delivery policy; automatic delivery requires worktrees.
    /// # Errors
    /// Rejects on-accept delivery outside worktree execution.
    pub fn with_delivery_mode(mut self, mode: DeliveryMode) -> Result<Self, WriterError> {
        if mode == DeliveryMode::OnAccept && self.mode != ExecutionMode::Worktree {
            return Err(WriterError::InvalidSettings);
        }
        self.delivery_mode = mode;
        Ok(self)
    }
    #[must_use]
    pub const fn delivery_mode(self) -> DeliveryMode {
        self.delivery_mode
    }
    #[must_use]
    pub const fn max_active_tasks(self) -> u64 {
        self.max_active_tasks
    }
    #[must_use]
    pub const fn allow_parallel_writers(self) -> bool {
        self.parallel
    }
    #[must_use]
    pub const fn execution_mode(self) -> ExecutionMode {
        self.mode
    }
}

/// A strictly decoded reservation, ordered by created timestamp and task id.
#[derive(Clone, PartialEq, Eq)]
pub struct WriterReservation {
    pub task_id: TaskId,
    pub project_id: ProjectId,
    pub scopes: Vec<String>,
    pub created_at: String,
    pub parallel: bool,
}
impl fmt::Debug for WriterReservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriterReservation").finish_non_exhaustive()
    }
}

/// Safe admission/persistence errors. Details are accessible only via source.
#[non_exhaustive]
pub enum WriterError {
    InvalidSettings,
    ProjectBusy,
    ScopeOverlap,
    ScopeDataError,
    InvalidTransition,
    TaskRow(TaskRowError),
    Database(rusqlite::Error),
}
impl fmt::Display for WriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidSettings => "writer admission settings are invalid",
            Self::ProjectBusy => "project writer or task limit is occupied",
            Self::ScopeOverlap => "scope overlaps an active writer",
            Self::ScopeDataError => "writer scope data is invalid or cannot be resolved",
            Self::InvalidTransition => "writer task transition is invalid",
            Self::TaskRow(_) => "writer task row could not be mapped",
            Self::Database(_) => "writer storage update failed",
        })
    }
}
impl fmt::Debug for WriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl Error for WriterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TaskRow(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

pub(super) fn has_ledger(connection: &Connection) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='active_writers')",
        [],
        |row| row.get(0),
    )
}

pub(super) fn is_writer(status: TaskStatus) -> bool {
    status.is_active() && status != TaskStatus::WaitingDependencies
}

pub(super) fn parse_scopes(raw: &str) -> Result<Vec<String>, WriterError> {
    let scopes: Vec<String> = serde_json::from_str(raw).map_err(|_| WriterError::ScopeDataError)?;
    for entry in &scopes {
        if entry.is_empty() || entry.contains(['\\', '\0']) {
            return Err(WriterError::ScopeDataError);
        }
        let body = entry.strip_suffix('/').unwrap_or(entry);
        let leading = body.len() - body.trim_start_matches('/').len();
        if body.is_empty()
            || leading > 2
            || body[leading..].is_empty()
            || body[leading..]
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(WriterError::ScopeDataError);
        }
    }
    Ok(scopes)
}

fn canonical(entry: &str, workspace: &Path) -> Result<(PathBuf, bool), WriterError> {
    let is_dir = entry.ends_with('/');
    let entry = Path::new(entry.trim_end_matches('/'));
    let mut prefix = if entry.is_absolute() {
        entry.to_owned()
    } else {
        workspace.join(entry)
    };
    // An absolute workspace is supplied by project configuration. Relative
    // inputs have no stable identity and must never be compared lexically.
    if !prefix.is_absolute() {
        return Err(WriterError::ScopeDataError);
    }
    let mut suffix = Vec::new();
    loop {
        match fs::symlink_metadata(&prefix) {
            Ok(_) => break,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                let name = prefix
                    .file_name()
                    .ok_or(WriterError::ScopeDataError)?
                    .to_owned();
                suffix.push(name);
                if !prefix.pop() {
                    return Err(WriterError::ScopeDataError);
                }
            }
            Err(_) => return Err(WriterError::ScopeDataError),
        }
    }
    let mut resolved = fs::canonicalize(prefix).map_err(|_| WriterError::ScopeDataError)?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok((resolved, is_dir))
}

fn conflict(left: &[String], right: &[String], workspace: &Path) -> Result<bool, WriterError> {
    for a in left {
        for b in right {
            let (a, a_dir) = canonical(a, workspace)?;
            let (b, b_dir) = canonical(b, workspace)?;
            if a == b || (a_dir && b.starts_with(&a)) || (b_dir && a.starts_with(&b)) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn check_admission(
    connection: &Connection,
    project: &str,
    scopes: &[String],
    workspace: &Path,
    parallel: bool,
    exclude: &str,
) -> Result<(), WriterError> {
    if parallel {
        for entry in scopes {
            canonical(entry, workspace)?;
        }
    }
    let mut existing = Vec::new();
    if has_ledger(connection).map_err(WriterError::Database)? {
        let mut statement = connection
            .prepare("SELECT scopes_json FROM active_writers WHERE project_id=?1 AND task_id!=?2")
            .map_err(WriterError::Database)?;
        let rows = statement
            .query_map(params![project, exclude], |row| row.get::<_, String>(0))
            .map_err(WriterError::Database)?;
        for row in rows {
            existing.push(parse_scopes(
                &row.map_err(|_| WriterError::ScopeDataError)?,
            )?);
        }
    }
    let mut statement = connection.prepare("SELECT allowed_paths FROM tasks WHERE project_id=?1 AND task_id!=?2 AND status IN ('implementing','awaiting_review','revising','needs_user','failed','delivery_unknown')").map_err(WriterError::Database)?;
    let rows = statement
        .query_map(params![project, exclude], |row| row.get::<_, String>(0))
        .map_err(WriterError::Database)?;
    for row in rows {
        existing.push(parse_scopes(
            &row.map_err(|_| WriterError::ScopeDataError)?,
        )?);
    }
    if !parallel && !existing.is_empty() {
        return Err(WriterError::ProjectBusy);
    }
    if parallel {
        for other in existing {
            if conflict(scopes, &other, workspace)? {
                return Err(WriterError::ScopeOverlap);
            }
        }
    }
    Ok(())
}

pub(super) fn reserve(
    connection: &Connection,
    task: &str,
    project: &str,
    scopes: &str,
    now: &str,
    parallel: bool,
) -> Result<(), WriterError> {
    if !has_ledger(connection).map_err(WriterError::Database)? {
        return Ok(());
    }
    // Reuse a crash-window reservation only after validating its identity/data.
    let existing: Option<(String, String, i64)> = connection
        .query_row(
            "SELECT project_id,scopes_json,parallel FROM active_writers WHERE task_id=?1",
            [task],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(WriterError::Database)?;
    if let Some((owner, raw, flag)) = existing {
        if !matches!(flag, 0 | 1)
            || owner != project
            || parse_scopes(&raw)? != parse_scopes(scopes)?
        {
            return Err(WriterError::ScopeDataError);
        }
        return Ok(());
    }
    connection.execute("INSERT INTO active_writers(task_id,project_id,scopes_json,created_at,parallel) VALUES (?1,?2,?3,?4,?5)", params![task,project,scopes,now,parallel]).map_err(|error| {
        if matches!(&error, rusqlite::Error::SqliteFailure(inner, _) if inner.code == rusqlite::ErrorCode::ConstraintViolation) { WriterError::ProjectBusy } else { WriterError::Database(error) }
    })?;
    Ok(())
}

pub(super) fn release_terminal(
    connection: &Connection,
    task: TaskId,
    status: TaskStatus,
) -> rusqlite::Result<()> {
    if status.is_terminal() && has_ledger(connection)? {
        connection.execute(
            "DELETE FROM active_writers WHERE task_id=?1",
            [task.to_string()],
        )?;
    }
    Ok(())
}

pub(super) fn reconcile(
    connection: &Connection,
    project: &str,
    settings: &AdmissionSettings,
) -> Result<u64, WriterError> {
    let removed = connection.execute("DELETE FROM active_writers WHERE project_id=?1 AND (task_id NOT IN (SELECT task_id FROM tasks WHERE project_id=?1) OR task_id IN (SELECT task_id FROM tasks WHERE project_id=?1 AND status IN ('accepted','closed')))", [project]).map_err(WriterError::Database)?;
    let mut ledger = connection
        .prepare("SELECT scopes_json,parallel FROM active_writers WHERE project_id=?1")
        .map_err(WriterError::Database)?;
    let retained = ledger
        .query_map([project], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(WriterError::Database)?;
    for row in retained {
        let (raw, flag) = row.map_err(|_| WriterError::ScopeDataError)?;
        if !matches!(flag, 0 | 1) {
            return Err(WriterError::ScopeDataError);
        }
        parse_scopes(&raw)?;
    }
    let mut statement = connection.prepare("SELECT task_id,allowed_paths FROM tasks WHERE project_id=?1 AND status IN ('implementing','awaiting_review','revising','needs_user','failed','delivery_unknown')").map_err(WriterError::Database)?;
    let rows = statement
        .query_map([project], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(WriterError::Database)?;
    let mut changed = removed as u64;
    let now = utc_now_rfc3339_millis();
    for row in rows {
        let (task, raw) = row.map_err(WriterError::Database)?;
        parse_scopes(&raw)?;
        // Preserve existing rows and flags, including a parallel-config downgrade.
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM active_writers WHERE task_id=?1)",
                [&task],
                |row| row.get(0),
            )
            .map_err(WriterError::Database)?;
        reserve(connection, &task, project, &raw, &now, settings.parallel)?;
        if !exists {
            changed += 1;
        }
    }
    Ok(changed)
}

/// Legacy upgrades repair reservations before committing v15. Current v15
/// initialization remains read-only; configured startup recovery calls the
/// public repair API explicitly instead of guessing parallel settings.
pub(super) fn reconcile_legacy(connection: &Connection) -> Result<(), WriterError> {
    let mut statement = connection.prepare("SELECT DISTINCT project_id FROM tasks UNION SELECT DISTINCT project_id FROM active_writers").map_err(WriterError::Database)?;
    let projects = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(WriterError::Database)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(WriterError::Database)?;
    for project in projects {
        reconcile(connection, &project, &AdmissionSettings::default())?;
    }
    Ok(())
}

impl StorageConnection {
    /// Checks a saved writer against both ledger and real task statuses, and
    /// repairs its missing reservation atomically. Existing parallel flags are
    /// immutable across config changes. Non-writers are not admitted here.
    /// # Errors
    /// Corrupt rows/scopes, overlap and inconsistent mode fail closed.
    pub fn admit_saved_writer(
        &mut self,
        task_id: TaskId,
        project: &ProjectId,
        settings: &AdmissionSettings,
    ) -> Result<bool, WriterError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(WriterError::Database)?;
        let task = tx
            .query_row(
                "SELECT * FROM tasks WHERE task_id=?1 AND project_id=?2",
                params![task_id.to_string(), project.as_str()],
                |r| Ok(Task::from_row(r)),
            )
            .optional()
            .map_err(WriterError::Database)?
            .ok_or(WriterError::InvalidTransition)?
            .map_err(WriterError::TaskRow)?;
        if !is_writer(task.status) || task.close_requested_at.is_some() {
            return Err(WriterError::InvalidTransition);
        }
        let mode: String = tx
            .query_row(
                "SELECT execution_mode FROM tasks WHERE task_id=?1",
                [task_id.to_string()],
                |r| r.get(0),
            )
            .map_err(WriterError::Database)?;
        if !matches!(mode.as_str(), "direct" | "worktree") {
            return Err(WriterError::InvalidSettings);
        }
        let saved: Option<i64> = tx
            .query_row(
                "SELECT parallel FROM active_writers WHERE task_id=?1",
                [task_id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(WriterError::Database)?;
        if saved.is_some_and(|v| !matches!(v, 0 | 1)) {
            return Err(WriterError::ScopeDataError);
        }
        let parallel = saved.map_or(settings.parallel && mode == "worktree", |v| v == 1);
        if parallel && mode != "worktree" {
            return Err(WriterError::InvalidSettings);
        }
        let raw =
            serde_json::to_string(&task.allowed_paths).map_err(|_| WriterError::ScopeDataError)?;
        let scopes = parse_scopes(&raw)?;
        check_admission(
            &tx,
            project.as_str(),
            &scopes,
            Path::new(&task.workspace),
            parallel,
            &task_id.to_string(),
        )?;
        reserve(
            &tx,
            &task_id.to_string(),
            project.as_str(),
            &raw,
            &utc_now_rfc3339_millis(),
            parallel,
        )?;
        tx.commit().map_err(WriterError::Database)?;
        Ok(parallel)
    }

    /// Returns strict reservations for this project, without changing state.
    /// # Errors
    /// Corrupt scopes/identifiers/flags and SQLite errors fail closed.
    pub fn get_active_writers(
        &self,
        project: &ProjectId,
    ) -> Result<Vec<WriterReservation>, WriterError> {
        let mut statement = self.connection.prepare("SELECT task_id,project_id,scopes_json,created_at,parallel FROM active_writers WHERE project_id=?1 ORDER BY created_at,task_id").map_err(WriterError::Database)?;
        let rows = statement
            .query_map([project.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(WriterError::Database)?;
        let mut out = Vec::new();
        for row in rows {
            let (task, project, raw, created_at, parallel) =
                row.map_err(|_| WriterError::ScopeDataError)?;
            if !matches!(parallel, 0 | 1) {
                return Err(WriterError::ScopeDataError);
            }
            out.push(WriterReservation {
                task_id: TaskId::from_str(&task).map_err(|_| WriterError::ScopeDataError)?,
                project_id: ProjectId::from_str(&project)
                    .map_err(|_| WriterError::ScopeDataError)?,
                scopes: parse_scopes(&raw)?,
                created_at,
                parallel: parallel == 1,
            });
        }
        Ok(out)
    }
    /// Conservative activity fence, independent of current config and scopes.
    /// # Errors
    /// SQLite failures return an error, never an assumed idle project.
    pub fn writer_activity_present(&self, project: &ProjectId) -> Result<bool, WriterError> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(WriterError::Database)?;
        let ledger = if has_ledger(&transaction).map_err(WriterError::Database)? {
            transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM active_writers WHERE project_id=?1)",
                    [project.as_str()],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(WriterError::Database)?
        } else {
            false
        };
        let tasks: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE project_id=?1 AND status IN ('implementing','awaiting_review','revising','needs_user','failed','delivery_unknown'))", [project.as_str()], |row| row.get(0)).map_err(WriterError::Database)?;
        transaction.commit().map_err(WriterError::Database)?;
        Ok(ledger || tasks)
    }
    /// Idempotent crash repair: remove orphan/terminal reservations and restore writers.
    /// # Errors
    /// Invalid scope data or an incompatible writer set rolls back every repair.
    pub fn reconcile_active_writers(
        &mut self,
        project: &ProjectId,
        settings: &AdmissionSettings,
    ) -> Result<u64, WriterError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(WriterError::Database)?;
        let changed = reconcile(&transaction, project.as_str(), settings)?;
        transaction.commit().map_err(WriterError::Database)?;
        Ok(changed)
    }
    /// Updates an ordinary task status; terminal transitions release its reservation.
    /// # Errors
    /// Invalid domain transitions, corrupt rows or SQLite failures roll back together.
    pub fn update_task_status(
        &mut self,
        task_id: TaskId,
        project: &ProjectId,
        status: TaskStatus,
        revision_count: Option<u32>,
    ) -> Result<bool, WriterError> {
        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(WriterError::Database)?;
        let task = transaction
            .query_row(
                "SELECT * FROM tasks WHERE task_id=?1 AND project_id=?2",
                params![task_id.to_string(), project.as_str()],
                |row| Ok(Task::from_row(row)),
            )
            .optional()
            .map_err(WriterError::Database)?;
        let Some(task) = task else {
            return Ok(false);
        };
        let task = task.map_err(WriterError::TaskRow)?;
        if task.status != status {
            task.status
                .require_transition(status)
                .map_err(|_| WriterError::InvalidTransition)?;
        }
        transaction.execute("UPDATE tasks SET status=?1, updated_at=?2, revision_count=COALESCE(?3,revision_count) WHERE task_id=?4 AND project_id=?5", params![status.as_str(),now,revision_count,task_id.to_string(),project.as_str()]).map_err(WriterError::Database)?;
        release_terminal(&transaction, task_id, status).map_err(WriterError::Database)?;
        transaction.commit().map_err(WriterError::Database)?;
        Ok(true)
    }
}
