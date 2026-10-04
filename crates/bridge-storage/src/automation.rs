//! Durable automation run storage. Scheduling and plan validation are consumers.
use crate::{RustStateLayout, TaskId, utc_now_rfc3339_millis};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use std::{error::Error, fmt, str::FromStr};

/// Canonical UUID distinct from a task identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(TaskId);
impl FromStr for RunId {
    type Err = AutomationStoreError;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let id: TaskId = text
            .parse()
            .map_err(|_| AutomationStoreError::InvalidIdentity)?;
        if id.to_string() != text {
            return Err(AutomationStoreError::InvalidIdentity);
        }
        Ok(Self(id))
    }
}
impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
impl fmt::Debug for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RunId { .. }")
    }
}

macro_rules! vocabulary {
    ($name:ident {$($variant:ident => $wire:literal),+ $(,)?}) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $($variant),+ }
        impl $name {
            #[must_use]
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } }
        }
        impl FromStr for $name {
            type Err = AutomationStoreError;
            fn from_str(text: &str) -> Result<Self, Self::Err> {
                match text { $($wire => Ok(Self::$variant)),+, _ => Err(AutomationStoreError::CorruptState) }
            }
        }
    }
}
vocabulary!(RunStatus { Running=>"running", Paused=>"paused", Blocked=>"blocked", Completed=>"completed", Ready=>"ready", Stopped=>"stopped" });
vocabulary!(RunControl { Run=>"run", Pause=>"pause", Stop=>"stop" });
impl RunStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Ready | Self::Stopped)
    }
}

/// A validated row; status and control columns override document copies.
#[derive(Clone, PartialEq)]
pub struct AutomationRun {
    id: RunId,
    status: RunStatus,
    control: RunControl,
    document: Value,
    created_at: String,
    updated_at: String,
}
impl AutomationRun {
    #[must_use]
    pub const fn id(&self) -> RunId {
        self.id
    }
    #[must_use]
    pub const fn status(&self) -> RunStatus {
        self.status
    }
    #[must_use]
    pub const fn control(&self) -> RunControl {
        self.control
    }
    #[must_use]
    pub fn document(&self) -> &Value {
        &self.document
    }
    #[must_use]
    pub fn created_at(&self) -> &str {
        &self.created_at
    }
    #[must_use]
    pub fn updated_at(&self) -> &str {
        &self.updated_at
    }
}
impl fmt::Debug for AutomationRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AutomationRun")
            .field("status", &self.status)
            .field("control", &self.control)
            .finish_non_exhaustive()
    }
}

/// Fixed messages keep persisted documents, SQL and paths out of diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationStoreError {
    InvalidIdentity,
    CorruptState,
    StateOwnership,
    UnsupportedSchema,
    NotFound,
    UnfinishedRun,
    AlreadyExists,
    TerminalRun,
    Database,
}
impl fmt::Display for AutomationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidIdentity => "invalid automation run identity",
            Self::CorruptState => "automation state is corrupt",
            Self::StateOwnership => "automation state ownership could not be verified",
            Self::UnsupportedSchema => "automation schema is unsupported",
            Self::NotFound => "automation run not found",
            Self::UnfinishedRun => "another unfinished automation run exists",
            Self::AlreadyExists => "automation run already exists",
            Self::TerminalRun => "automation run is terminal",
            Self::Database => "automation state operation failed",
        })
    }
}
impl Error for AutomationStoreError {}

/// Uses an explicit Rust-owned namespace. Read-only load never initializes it.
#[derive(Clone)]
pub struct AutomationRunStore {
    layout: RustStateLayout,
}
impl AutomationRunStore {
    #[must_use]
    pub const fn new(layout: RustStateLayout) -> Self {
        Self { layout }
    }
    /// Creates one running run and initializes owned state only after input validation.
    /// # Errors
    /// Invalid identity, ownership, schema, duplicate identity and occupied unfinished slot fail closed.
    pub fn create(&self, document: &Value) -> Result<AutomationRun, AutomationStoreError> {
        let id = document_identity(document)?;
        self.layout
            .initialize()
            .map_err(|_| AutomationStoreError::StateOwnership)?;
        let mut storage = self
            .layout
            .open()
            .map_err(|_| AutomationStoreError::StateOwnership)?;
        let transaction = storage
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| AutomationStoreError::Database)?;
        guard(&transaction)?;
        if read(&transaction, Some(id))?.is_some() {
            return Err(AutomationStoreError::AlreadyExists);
        }
        let unfinished: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM automation_runs WHERE status NOT IN ('completed','ready','stopped'))", [], |row| row.get(0)).map_err(|_| AutomationStoreError::Database)?;
        if unfinished {
            return Err(AutomationStoreError::UnfinishedRun);
        }
        let now = utc_now_rfc3339_millis();
        transaction.execute("INSERT INTO automation_runs(run_id,status,document,created_at,updated_at) VALUES (?1,'running',?2,?3,?3)", params![id.to_string(),document.to_string(),now]).map_err(|_| AutomationStoreError::Database)?;
        let run = read(&transaction, Some(id))?.ok_or(AutomationStoreError::NotFound)?;
        transaction
            .commit()
            .map_err(|_| AutomationStoreError::Database)?;
        Ok(run)
    }
    /// Loads an explicit run or the latest by creation timestamp and row order.
    /// # Errors
    /// Missing, foreign, unsupported or corrupt state is rejected without creating files.
    pub fn load(&self, id: Option<RunId>) -> Result<AutomationRun, AutomationStoreError> {
        if !self.layout.database().is_file() {
            return Err(AutomationStoreError::NotFound);
        }
        match crate::read_owned_marker(
            &self.layout.marker(),
            self.layout.project_id(),
            self.layout.state_root(),
        ) {
            crate::MarkerState::Owned => {}
            _ => return Err(AutomationStoreError::StateOwnership),
        }
        let connection = crate::open_read_only_current(&self.layout.database())
            .map_err(|_| AutomationStoreError::Database)?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|_| AutomationStoreError::Database)?;
        guard(&transaction)?;
        let run = read(&transaction, id)?.ok_or(AutomationStoreError::NotFound)?;
        transaction
            .commit()
            .map_err(|_| AutomationStoreError::Database)?;
        Ok(run)
    }
    /// Saves document/status atomically, preserving the latest external control.
    /// # Errors
    /// Invalid document, unknown row, corrupt state and unique slot conflicts are rejected.
    pub fn save(
        &self,
        document: &Value,
        status: RunStatus,
    ) -> Result<AutomationRun, AutomationStoreError> {
        let id = document_identity(document)?;
        let mut storage = self
            .layout
            .open()
            .map_err(|_| AutomationStoreError::StateOwnership)?;
        let transaction = storage
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| AutomationStoreError::Database)?;
        guard(&transaction)?;
        let old = read(&transaction, Some(id))?.ok_or(AutomationStoreError::NotFound)?;
        // The store is not a coordinator FSM, but terminal rows cannot be revived.
        if old.status.is_terminal() && !status.is_terminal() {
            return Err(AutomationStoreError::TerminalRun);
        }
        let mut document = document.clone();
        document["status"] = Value::String(status.as_str().into());
        transaction
            .execute(
                "UPDATE automation_runs SET status=?1,document=?2,updated_at=?3 WHERE run_id=?4",
                params![
                    status.as_str(),
                    document.to_string(),
                    utc_now_rfc3339_millis(),
                    id.to_string()
                ],
            )
            .map_err(|_| AutomationStoreError::Database)?;
        let run = read(&transaction, Some(id))?.ok_or(AutomationStoreError::NotFound)?;
        transaction
            .commit()
            .map_err(|_| AutomationStoreError::Database)?;
        Ok(run)
    }
    /// Changes control under the same transaction that checks terminal status.
    /// # Errors
    /// Missing/corrupt/terminal runs and invalid owned state are rejected.
    pub fn set_control(
        &self,
        id: RunId,
        control: RunControl,
    ) -> Result<AutomationRun, AutomationStoreError> {
        let mut storage = self
            .layout
            .open()
            .map_err(|_| AutomationStoreError::StateOwnership)?;
        let transaction = storage
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| AutomationStoreError::Database)?;
        guard(&transaction)?;
        let run = read(&transaction, Some(id))?.ok_or(AutomationStoreError::NotFound)?;
        if run.status.is_terminal() {
            return Err(AutomationStoreError::TerminalRun);
        }
        transaction
            .execute(
                "UPDATE automation_runs SET control=?1,updated_at=?2 WHERE run_id=?3",
                params![control.as_str(), utc_now_rfc3339_millis(), id.to_string()],
            )
            .map_err(|_| AutomationStoreError::Database)?;
        let run = read(&transaction, Some(id))?.ok_or(AutomationStoreError::NotFound)?;
        transaction
            .commit()
            .map_err(|_| AutomationStoreError::Database)?;
        Ok(run)
    }
}
fn guard(connection: &Connection) -> Result<(), AutomationStoreError> {
    let schema = crate::validate_database(connection)
        .map_err(|_| AutomationStoreError::UnsupportedSchema)?;
    if schema.user_version() != 17 {
        return Err(AutomationStoreError::UnsupportedSchema);
    }
    crate::require_runtime_owner(connection).map_err(|_| AutomationStoreError::StateOwnership)
}
fn document_identity(document: &Value) -> Result<RunId, AutomationStoreError> {
    document
        .as_object()
        .and_then(|object| object.get("run_id"))
        .and_then(Value::as_str)
        .ok_or(AutomationStoreError::InvalidIdentity)?
        .parse()
}
fn read(
    connection: &Connection,
    id: Option<RunId>,
) -> Result<Option<AutomationRun>, AutomationStoreError> {
    let decode =
        |row: &rusqlite::Row<'_>| -> rusqlite::Result<Result<AutomationRun, AutomationStoreError>> {
            let raw: String = row.get("run_id")?;
            let status: String = row.get("status")?;
            let control: String = row.get("control")?;
            let text: String = row.get("document")?;
            Ok((|| {
                let id = raw
                    .parse()
                    .map_err(|_| AutomationStoreError::CorruptState)?;
                let status: RunStatus = status.parse()?;
                let control: RunControl = control.parse()?;
                let mut document: Value =
                    serde_json::from_str(&text).map_err(|_| AutomationStoreError::CorruptState)?;
                if document_identity(&document).map_err(|_| AutomationStoreError::CorruptState)?
                    != id
                {
                    return Err(AutomationStoreError::CorruptState);
                }
                document["status"] = Value::String(status.as_str().into());
                document["control"] = Value::String(control.as_str().into());
                Ok(AutomationRun {
                    id,
                    status,
                    control,
                    document,
                    created_at: row
                        .get("created_at")
                        .map_err(|_| AutomationStoreError::CorruptState)?,
                    updated_at: row
                        .get("updated_at")
                        .map_err(|_| AutomationStoreError::CorruptState)?,
                })
            })())
        };
    let result = if let Some(id) = id {
        connection.query_row(
            "SELECT * FROM automation_runs WHERE run_id=?1",
            [id.to_string()],
            decode,
        )
    } else {
        connection.query_row(
            "SELECT * FROM automation_runs ORDER BY created_at DESC,rowid DESC LIMIT 1",
            [],
            decode,
        )
    }
    .optional()
    .map_err(|_| AutomationStoreError::CorruptState)?;
    result.transpose()
}
