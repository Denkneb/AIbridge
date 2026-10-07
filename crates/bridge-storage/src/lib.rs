//! Read-only inspection of an existing `agent-bridge` SQLite state database.
//!
//! This crate opens an existing database strictly read-only (SQLite URI
//! `mode=ro&immutable=1`) and checks the frozen v6/v15 contracts in
//! `docs/fixtures/sqlite/expected.json` and `expected-v15.json`, or the
//! historical v11/v14 contracts transcribed from reference migration rules:
//!
//! * `PRAGMA user_version` and `meta.schema_version` as a consistent supported
//!   version pair;
//! * the required user tables with no unexpected ones;
//! * every column's name, declared type, `NOT NULL` flag and primary-key
//!   position (physical column order is not part of the contract);
//! * every named index's table, ordered columns, uniqueness and partial flag
//!   (internal `sqlite_autoindex_*` indexes are not part of the contract);
//! * every foreign key's source table, columns and `ON UPDATE`/`ON DELETE`
//!   actions.
//!
//! Inspection never creates, migrates or modifies the database and never
//! leaves `-wal`/`-shm` sidecars behind. A missing path is reported as
//! [`InspectError::NotFound`] and is never created. Errors carry only safe,
//! non-sensitive information: schema object names and version numbers, never
//! row data, secrets or machine-specific paths.
//!
//! The crate also provides a runtime read-write connection ([`connect`]) that
//! applies and verifies `PRAGMA journal_mode=WAL`, `PRAGMA foreign_keys=ON` and
//! `PRAGMA busy_timeout=30000`, mirroring Python `Storage.connect`, without
//! creating or migrating any schema.
//!
//! Task row mapping (task 3.3) is provided by [`Task::from_row`], which turns a
//! schema v6 `tasks` row into a fully typed [`Task`] and fails closed on
//! corrupted persisted data. Round row mapping (task 3.4) is provided by
//! [`RoundRow::from_row`], which turns a schema v6 `rounds` row into a complete
//! storage-owned [`RoundRow`] (all twenty-one columns) and likewise fails closed
//! on corrupted persisted data, including an inconsistent
//! `verifier_state`/`verifier_json` pair. Atomic, idempotent, fail-closed
//! initialization of a compatible empty schema v6 database (task 3.5) is
//! provided by [`initialize`].
//!
//! Read-only task query APIs (task 3.6) are provided as methods on
//! [`StorageConnection`]: [`StorageConnection::get_task`],
//! [`StorageConnection::get_active_task`], [`StorageConnection::list_tasks`] and
//! [`StorageConnection::count_tasks`]. They use one production source for the
//! exact fifteen `tasks` columns, map every found row through [`Task::from_row`],
//! isolate data strictly by project, filter active tasks with the exact
//! [`TaskStatus::is_active`] vocabulary and never write.
//!
//! Atomic task creation (task 3.7) is provided by
//! [`StorageConnection::create_task`]. In one `BEGIN IMMEDIATE` transaction it
//! writes exactly one `tasks` row (`implementing`, `revision_count=0`), one
//! `rounds` row (`round_number=1`, `implement`, `pending`, `attempted=0`) and
//! one `events` row (`created`, `task created (implement)`), all sharing a
//! single UTC RFC3339 millisecond timestamp, and returns the created [`Task`]
//! through the existing [`Task::from_row`] contract.
//!
//! Request idempotency (task 3.8) extends the same call: inside the very same
//! `BEGIN IMMEDIATE` transaction the exact `project_id`+`request_id` round is
//! looked up through [`RoundRow::from_row`] *before* any insert. A matching
//! `implement`/`round_number=1`/`payload_hash` request whose linked task exists
//! in the same project returns [`CreateTaskOutcome::Replayed`] with the original
//! [`Task`] and writes nothing; a different hash or a non-`implement` round is a
//! [`CreateTaskError::RequestConflict`]; an unmappable round, a non-initial
//! `implement` round, a missing/unmappable linked task or a task/project
//! mismatch is a fail-closed [`CreateTaskError::InvalidPersistedState`]. The
//! check and the inserts share one writer transaction, so concurrent identical
//! requests yield exactly one [`CreateTaskOutcome::Created`] and one
//! [`CreateTaskOutcome::Replayed`]. Revision rounds, round/verifier updates and
//! schema migrations remain out of scope.
//!
//! Atomic round/task lifecycle transitions (task 3.9a) are provided as methods
//! on [`StorageConnection`]: [`StorageConnection::create_revision_round`],
//! [`StorageConnection::bind_round_session`], [`StorageConnection::prepare_round`],
//! [`StorageConnection::mark_round_sent`],
//! [`StorageConnection::mark_round_observing`],
//! [`StorageConnection::mark_worker_started`] and
//! [`StorageConnection::finish_round`]. Every method runs in one
//! `BEGIN IMMEDIATE` transaction, validates the round status change against the
//! local table-driven [`ROUND_TRANSITIONS`] contract and validates every task
//! status change through [`TaskStatus::require_transition`] before the UPDATE.
//! Paired round/task writes and their event share one timestamp, so an observer
//! never sees a partially applied lifecycle step.
//!
//! Verifier persist-once (task 3.9b) extends the same connection with
//! [`StorageConnection::begin_verifier`] and
//! [`StorageConnection::complete_verifier`]. Both run in one `BEGIN IMMEDIATE`
//! transaction, validate the round through the 3.9a current-round/project check
//! (which also enforces the `verifier_state`/`verifier_json` pair through
//! [`RoundRow::from_row`]) and never overwrite a persisted `done` result: an
//! identical completed result is replayed without any write, a different one is
//! a fail-closed conflict, and a stale `begin` cannot undo a finished run.
//!
//! Reopen of a recoverable failed round (task 3.9c) is provided by
//! [`StorageConnection::reopen_failed_round`]. In one `BEGIN IMMEDIATE`
//! transaction it reopens the current round only when the task is `failed`, no
//! cooperative close is pending and the current round is `failed` with
//! [`RECOVERABLE_FAILED_ERROR_CODE`], flipping the round to `observing`,
//! clearing `error_code`, restoring the task to `implementing`/`revising` and
//! writing exactly one `reopened` event with one shared timestamp. The
//! `failed -> observing` recovery transition is validated locally inside this
//! method and is deliberately absent from [`ROUND_TRANSITIONS`], so the generic
//! lifecycle methods can never perform it. Non-eligible and repeated calls are a
//! typed no-op; corrupted rows fail closed.
//!
//! Cooperative close (task 3.9d) is provided by
//! [`StorageConnection::request_task_close`] and
//! [`StorageConnection::complete_requested_close`], and is also honoured inside
//! [`StorageConnection::finish_round`]. `request_task_close` persists a pending
//! `close_requested_at`/`close_reason` (truncated to 300 Unicode scalar values
//! like Python `reason[:300]`) with exactly one `close_requested` event
//! (`round_number = NULL`) and is idempotent on repeat. `complete_requested_close`
//! atomically moves a non-terminal task with a pending request to `closed` with
//! exactly one `closed` event. `finish_round` always writes the requested round
//! fields/status but, when a close is pending, applies the *effective*
//! `closed` task transition (validated through [`TaskStatus::require_transition`])
//! and writes the `closed` event instead of the round-finished event, so a
//! caller-supplied `task_status` can never bypass a pending close. All paired
//! writes and events share one timestamp and every failure rolls the whole
//! transaction back. Schema changes remain out of scope.
//!
//! Rust state isolation (task 3.10) is expressed by [`RustStateLayout`], the
//! typed storage-level contract of one Rust-owned project state. It derives the
//! `state.sqlite`, lock, PID/ownership, log, token and endpoint paths of a
//! project exclusively from an explicitly passed Rust state root, so Rust
//! runtime artifacts can never overlap a Python state root.
//! [`RustStateLayout::initialize`] creates the Rust-owned empty schema v17
//! database in that root, and there is deliberately no API that reads, copies
//! or imports a Python SQLite database or history.
//!
//! Ownership/format markers (task 3.11) extend that layout with a versioned
//! sidecar marker ([`RustStateLayout::marker`]) and the additive
//! `meta.runtime_owner='rust'` row. [`RustStateLayout::initialize`] writes both
//! markers only for a new, isolated Rust state, and [`RustStateLayout::open`]
//! refuses to hand out a writable [`StorageConnection`] until the sidecar
//! marker, its supported `format_version`, its implementation, its project
//! namespace, its normalized state root, `meta.runtime_owner` and the frozen
//! supported v6/v11/v14/v15 schema contract all agree. Initialization upgrades owned
//! v6/v11/v14 state to v15 atomically; opening a legacy state alone never migrates it.
//! Every missing, malformed, unsupported, foreign or contradictory state fails
//! closed without writing anything, so a Python state, a foreign namespace or a
//! partially created state is never adopted.
//!
//! Dependency activation (task 3.12c) is explicit through
//! [`StorageConnection::activate_waiting_dependencies`]. A domain
//! dependencies-satisfied event permits the waiting-to-implementing transition;
//! its conditional UPDATE and persisted event share one writer transaction.
//! [`StorageConnection::refresh_task_baseline`] fences snapshot updates to a
//! still-waiting task in the requested project. Neither operation starts a
//! worker or creates/attempts a round. Dependency acceptance is established by
//! the caller. Activation checks actual writer tasks plus the reservation
//! ledger and reserves the slot atomically; validated [`AdmissionSettings`]
//! enable disjoint parallel scopes only in worktree mode.
//!
//! Writer admission (task 3.12d) extends creation through
//! [`StorageConnection::create_task_with_admission`]. Defaults remain one
//! unfinished task and no parallel writers. Canonical scope identities follow
//! symlinks and missing path tails, refusing corrupt or unresolvable data.
//! Terminal status changes, round completion and cooperative close release
//! reservations in the same transaction. [`StorageConnection::get_active_writers`],
//! [`StorageConnection::writer_activity_present`] and explicit
//! [`StorageConnection::reconcile_active_writers`] expose strict reads,
//! conservative activity fencing and crash repair. Legacy Rust-owned upgrades
//! backfill reservations; current v15 initialization never guesses new config
//! or mutates existing state. Scope authorization and runtime parallel workers
//! remain the responsibility of higher-level consumers.

pub mod prune;

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use bridge_domain::{
    ProjectId, RoundKind, RoundStatus, TaskId, TaskStatus, Verification, VerifierState,
};
use rusqlite::types::{Null, Value as SqlValue};
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Row, TransactionBehavior, params,
    params_from_iter,
};

/// Historical v6 schema retained by the generic fixture initializer.
pub const SCHEMA_VERSION: i64 = 6;

/// Schema created and upgraded by the guarded Rust state initializer.
pub const RUST_SCHEMA_VERSION: i64 = 17;

mod budgets;
pub mod usage;
mod verifier_progress;
pub use budgets::{
    BUDGET_USAGE_FIELDS, BudgetReadError, BudgetValidationError, DEFAULT_BUDGET_WARNING_THRESHOLD,
    TaskBudget, normalize_persisted_budget, read_task_budget_readonly, validate_budget,
};
pub mod active_set;
pub mod automation;
mod dependencies;
mod manual_status;
mod mcp_lifecycle;
pub mod profiles;
pub mod recovery;
mod schema_v15;
mod worktrees;
mod writers;
pub use dependencies::DependencyUpdateError;
pub use worktrees::{
    WorktreeDeliveryState, WorktreeQuarantineEntry, WorktreeQuarantineRegistration,
    WorktreeQuarantineStatus, WorktreeRecord, WorktreeRegistration, WorktreeStatus,
    WorktreeStorageError, has_worktree_quarantine_table, list_worktree_quarantine_readonly,
    read_worktree_readonly_strict,
};
pub use writers::{AdmissionSettings, WriterError, WriterReservation};

/// A single column of a user table.
///
/// Physical column order is deliberately not represented: the contract is the
/// set of `(name, declared type, NOT NULL, primary-key position)` tuples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// Declared type exactly as reported by `PRAGMA table_info`.
    pub declared_type: String,
    /// Whether the column is declared `NOT NULL`.
    pub not_null: bool,
    /// Zero-based position within the primary key, or `0` when not part of it.
    pub primary_key: i64,
}

/// A user table and its columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Table name.
    pub name: String,
    /// Columns of the table.
    pub columns: Vec<Column>,
}

/// A named index (internal `sqlite_autoindex_*` indexes are excluded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// Index name.
    pub name: String,
    /// Table the index belongs to.
    pub table: String,
    /// Indexed columns in index order.
    pub columns: Vec<String>,
    /// Whether the index is unique.
    pub unique: bool,
    /// Whether the index is partial (has a `WHERE` clause).
    pub partial: bool,
}

/// A foreign-key column reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// Source table.
    pub table: String,
    /// Source column.
    pub from: String,
    /// Referenced table.
    pub to_table: String,
    /// Referenced column.
    pub to_column: String,
    /// `ON UPDATE` action as reported by SQLite.
    pub on_update: String,
    /// `ON DELETE` action as reported by SQLite.
    pub on_delete: String,
}

/// The observed, contract-validated schema of a database.
///
/// A value of this type can only be produced by [`inspect`], which guarantees
/// that both version markers and the supported v6/v11/v14/v15/v16/v17 contract matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    user_version: i64,
    meta_schema_version: String,
    tables: Vec<Table>,
    indexes: Vec<Index>,
    foreign_keys: Vec<ForeignKey>,
}

impl Inspection {
    /// The validated `PRAGMA user_version`.
    #[must_use]
    pub const fn user_version(&self) -> i64 {
        self.user_version
    }

    /// The validated `meta.schema_version` value.
    #[must_use]
    pub fn meta_schema_version(&self) -> &str {
        &self.meta_schema_version
    }

    /// The user tables, sorted by name.
    #[must_use]
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    /// The named indexes, sorted by name.
    #[must_use]
    pub fn indexes(&self) -> &[Index] {
        &self.indexes
    }

    /// The foreign keys, sorted by source table and column.
    #[must_use]
    pub fn foreign_keys(&self) -> &[ForeignKey] {
        &self.foreign_keys
    }
}

/// A structural schema mismatch against a frozen supported contract.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemaMismatch {
    /// A required table is absent.
    MissingTable { table: String },
    /// A table that is not part of the contract is present.
    UnexpectedTable { table: String },
    /// A required column is absent.
    MissingColumn { table: String, column: String },
    /// A column that is not part of the contract is present.
    UnexpectedColumn { table: String, column: String },
    /// A column exists but its type, `NOT NULL` flag or primary-key position differs.
    ColumnDefinition { table: String, column: String },
    /// A required named index is absent.
    MissingIndex { index: String },
    /// A named index that is not part of the contract is present.
    UnexpectedIndex { index: String },
    /// An index exists but its table, columns, uniqueness or partial flag differs.
    IndexDefinition { index: String },
    /// A required foreign key is absent.
    MissingForeignKey { table: String, from: String },
    /// A foreign key that is not part of the contract is present.
    UnexpectedForeignKey { table: String, from: String },
    /// A foreign key exists but its target or referential actions differ.
    ForeignKeyDefinition { table: String, from: String },
}

impl fmt::Display for SchemaMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTable { table } => write!(f, "missing table {table}"),
            Self::UnexpectedTable { table } => write!(f, "unexpected table {table}"),
            Self::MissingColumn { table, column } => {
                write!(f, "missing column {table}.{column}")
            }
            Self::UnexpectedColumn { table, column } => {
                write!(f, "unexpected column {table}.{column}")
            }
            Self::ColumnDefinition { table, column } => {
                write!(f, "column definition mismatch {table}.{column}")
            }
            Self::MissingIndex { index } => write!(f, "missing index {index}"),
            Self::UnexpectedIndex { index } => write!(f, "unexpected index {index}"),
            Self::IndexDefinition { index } => write!(f, "index definition mismatch {index}"),
            Self::MissingForeignKey { table, from } => {
                write!(f, "missing foreign key on {table}({from})")
            }
            Self::UnexpectedForeignKey { table, from } => {
                write!(f, "unexpected foreign key on {table}({from})")
            }
            Self::ForeignKeyDefinition { table, from } => {
                write!(f, "foreign key definition mismatch on {table}({from})")
            }
        }
    }
}

impl Error for SchemaMismatch {}

/// A typed, safe error raised while opening or inspecting a database.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that never contains row data, secrets or machine-specific paths.
/// Schema object names may appear in [`SchemaMismatch`]. The underlying SQLite
/// error, when present, is reachable only through [`Error::source`].
#[derive(Debug)]
#[non_exhaustive]
pub enum InspectError {
    /// The database file does not exist (and was not created).
    NotFound,
    /// The file exists but cannot be opened read-only.
    NotReadable,
    /// The file is not a SQLite database.
    NotADatabase,
    /// `PRAGMA user_version` is not v6, v11, v14, v15 or [`RUST_SCHEMA_VERSION`].
    UnsupportedUserVersion { found: i64 },
    /// The `meta` table or the `schema_version` key is missing.
    MissingSchemaVersion,
    /// `meta.schema_version` is not an integer.
    MalformedSchemaVersion,
    /// `meta.schema_version` does not agree with `PRAGMA user_version`.
    MismatchedSchemaVersion { user_version: i64, meta: String },
    /// The database does not match its supported schema contract.
    IncompatibleSchema(SchemaMismatch),
    /// An unexpected SQLite failure.
    Database(rusqlite::Error),
}

impl fmt::Display for InspectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("database file does not exist"),
            Self::NotReadable => f.write_str("database file cannot be opened read-only"),
            Self::NotADatabase => f.write_str("file is not a SQLite database"),
            Self::UnsupportedUserVersion { found } => {
                write!(f, "unsupported schema version {found}")
            }
            Self::MissingSchemaVersion => f.write_str("meta.schema_version is missing"),
            Self::MalformedSchemaVersion => f.write_str("meta.schema_version is malformed"),
            Self::MismatchedSchemaVersion { .. } => {
                f.write_str("schema version markers are inconsistent")
            }
            Self::IncompatibleSchema(mismatch) => {
                write!(f, "database schema is not compatible: {mismatch}")
            }
            Self::Database(_) => f.write_str("storage database error"),
        }
    }
}

impl Error for InspectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::IncompatibleSchema(mismatch) => Some(mismatch),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// Opens `path` strictly read-only and validates it against schema v6, v11, v14, v15, v16 or v17.
///
/// The database is opened with the SQLite URI `mode=ro&immutable=1`, so a
/// missing file is never created and no `-wal`/`-shm` sidecars are produced.
///
/// # Errors
///
/// Returns [`InspectError::NotFound`] when `path` does not exist, and a typed
/// category for every other open/version/schema problem (see [`InspectError`]
/// and [`SchemaMismatch`]). No error message contains row data, secrets or
/// machine-specific paths.
pub fn inspect(path: impl AsRef<Path>) -> Result<Inspection, InspectError> {
    let path = path.as_ref();
    if !path.exists() {
        return Err(InspectError::NotFound);
    }

    let connection = open_read_only(path)?;
    validate_database(&connection)
}

/// Validates an open connection against its frozen v6/v11/v14/v15/v16/v17 contract.
///
/// This is the shared core of [`inspect`] and [`initialize`]: it reads the
/// version markers, the user tables, the named indexes and the foreign keys and
/// checks all of them against the contract. It performs no writes.
fn validate_database(connection: &Connection) -> Result<Inspection, InspectError> {
    let user_version = read_user_version(connection)?;
    let tables = read_tables(connection)?;

    if !matches!(
        user_version,
        SCHEMA_VERSION | 11 | 14 | 15 | 16 | RUST_SCHEMA_VERSION
    ) {
        return Err(InspectError::UnsupportedUserVersion {
            found: user_version,
        });
    }

    let meta_schema_version = read_meta_schema_version(connection, &tables)?;
    validate_version_pair(user_version, &meta_schema_version)?;

    let indexes = read_indexes(connection)?;
    let foreign_keys = read_foreign_keys(connection, &tables)?;

    if user_version != SCHEMA_VERSION {
        schema_v15::validate(connection, &tables, &indexes, &foreign_keys, user_version)?;
    } else {
        validate_schema(&tables, &indexes, &foreign_keys)
            .map_err(InspectError::IncompatibleSchema)?;
    }

    Ok(Inspection {
        user_version,
        meta_schema_version,
        tables,
        indexes,
        foreign_keys,
    })
}

fn open_read_only(path: &Path) -> Result<Connection, InspectError> {
    let uri = read_only_uri(path);
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(uri, flags).map_err(classify_error)
}

/// Builds a `file:` URI with `mode=ro&immutable=1`.
///
/// The path is percent-encoded so that `%`, `?` and `#` (and any non-ASCII
/// byte) cannot be mistaken for URI syntax.
fn read_only_uri(path: &Path) -> String {
    encoded_file_uri(path, "mode=ro&immutable=1")
}

/// Builds a `file:` URI with a plain `mode=ro` query.
///
/// Unlike [`read_only_uri`], the connection sees the current committed content
/// (including a live WAL) instead of an immutable snapshot.
fn read_only_current_uri(path: &Path) -> String {
    encoded_file_uri(path, "mode=ro")
}

/// Percent-encodes `path` into a `file:` URI with the given query string.
fn encoded_file_uri(path: &Path, query: &str) -> String {
    let mut encoded = String::new();
    for &byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(char::from(byte))
            }
            _ => {
                encoded.push('%');
                encoded.push(hex_digit(byte >> 4));
                encoded.push(hex_digit(byte & 0x0f));
            }
        }
    }
    format!("file:{encoded}?{query}")
}

fn hex_digit(value: u8) -> char {
    char::from(match value {
        0..=9 => b'0' + value,
        _ => b'A' + (value - 10),
    })
}

fn classify_error(error: rusqlite::Error) -> InspectError {
    if let rusqlite::Error::SqliteFailure(inner, _) = &error {
        match inner.code {
            ErrorCode::CannotOpen => return InspectError::NotReadable,
            ErrorCode::NotADatabase => return InspectError::NotADatabase,
            _ => {}
        }
    }
    InspectError::Database(error)
}

fn read_user_version(connection: &Connection) -> Result<i64, InspectError> {
    query_user_version(connection).map_err(classify_error)
}

fn query_user_version(connection: &Connection) -> rusqlite::Result<i64> {
    connection.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn read_meta_schema_version(
    connection: &Connection,
    tables: &[Table],
) -> Result<String, InspectError> {
    if !tables.iter().any(|table| table.name == "meta") {
        return Err(InspectError::MissingSchemaVersion);
    }
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(classify_error)?;
    value.ok_or(InspectError::MissingSchemaVersion)
}

fn validate_version_pair(user_version: i64, meta: &str) -> Result<(), InspectError> {
    match meta.parse::<i64>() {
        Ok(parsed) if parsed == user_version => Ok(()),
        Ok(_) => Err(InspectError::MismatchedSchemaVersion {
            user_version,
            meta: meta.to_owned(),
        }),
        Err(_) => Err(InspectError::MalformedSchemaVersion),
    }
}

fn read_tables(connection: &Connection) -> Result<Vec<Table>, InspectError> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .map_err(classify_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(classify_error)?;
    let mut names = Vec::new();
    for row in rows {
        names.push(row.map_err(classify_error)?);
    }

    let mut tables = Vec::with_capacity(names.len());
    for name in names {
        let columns = read_columns(connection, &name)?;
        tables.push(Table { name, columns });
    }
    Ok(tables)
}

fn read_columns(connection: &Connection, table: &str) -> Result<Vec<Column>, InspectError> {
    let mut statement = connection
        .prepare("SELECT name, type, \"notnull\", pk FROM pragma_table_info(?1)")
        .map_err(classify_error)?;
    let rows = statement
        .query_map(params![table], |row| {
            Ok(Column {
                name: row.get(0)?,
                declared_type: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                not_null: row.get::<_, i64>(2)? != 0,
                primary_key: row.get(3)?,
            })
        })
        .map_err(classify_error)?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row.map_err(classify_error)?);
    }
    Ok(columns)
}

fn read_indexes(connection: &Connection) -> Result<Vec<Index>, InspectError> {
    let mut statement = connection
        .prepare(
            "SELECT name, tbl_name FROM sqlite_master \
             WHERE type = 'index' AND sql IS NOT NULL ORDER BY name",
        )
        .map_err(classify_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(classify_error)?;
    let mut raw = Vec::new();
    for row in rows {
        raw.push(row.map_err(classify_error)?);
    }

    let mut indexes = Vec::with_capacity(raw.len());
    for (name, table) in raw {
        let columns = read_index_columns(connection, &name)?;
        let (unique, partial) = read_index_flags(connection, &table, &name)?;
        indexes.push(Index {
            name,
            table,
            columns,
            unique,
            partial,
        });
    }
    Ok(indexes)
}

fn read_index_columns(connection: &Connection, index: &str) -> Result<Vec<String>, InspectError> {
    let mut statement = connection
        .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
        .map_err(classify_error)?;
    let rows = statement
        .query_map(params![index], |row| row.get::<_, Option<String>>(0))
        .map_err(classify_error)?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row.map_err(classify_error)?.unwrap_or_default());
    }
    Ok(columns)
}

fn read_index_flags(
    connection: &Connection,
    table: &str,
    index: &str,
) -> Result<(bool, bool), InspectError> {
    connection
        .query_row(
            "SELECT \"unique\", partial FROM pragma_index_list(?1) WHERE name = ?2",
            params![table, index],
            |row| Ok((row.get::<_, i64>(0)? != 0, row.get::<_, i64>(1)? != 0)),
        )
        .map_err(classify_error)
}

fn read_foreign_keys(
    connection: &Connection,
    tables: &[Table],
) -> Result<Vec<ForeignKey>, InspectError> {
    let mut foreign_keys = Vec::new();
    for table in tables {
        let mut statement = connection
            .prepare(
                "SELECT \"table\", \"from\", \"to\", on_update, on_delete \
                 FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
            )
            .map_err(classify_error)?;
        let rows = statement
            .query_map(params![table.name], |row| {
                Ok(ForeignKey {
                    table: table.name.clone(),
                    from: row.get(1)?,
                    to_table: row.get(0)?,
                    to_column: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    on_update: row.get(3)?,
                    on_delete: row.get(4)?,
                })
            })
            .map_err(classify_error)?;
        for row in rows {
            foreign_keys.push(row.map_err(classify_error)?);
        }
    }
    foreign_keys.sort_by(|left, right| {
        (left.table.as_str(), left.from.as_str()).cmp(&(right.table.as_str(), right.from.as_str()))
    });
    Ok(foreign_keys)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Contract {
    tables: Vec<Table>,
    indexes: Vec<Index>,
    foreign_keys: Vec<ForeignKey>,
}

fn column(name: &str, declared_type: &str, not_null: bool, primary_key: i64) -> Column {
    Column {
        name: name.to_owned(),
        declared_type: declared_type.to_owned(),
        not_null,
        primary_key,
    }
}

fn index(name: &str, table: &str, columns: &[&str], unique: bool, partial: bool) -> Index {
    Index {
        name: name.to_owned(),
        table: table.to_owned(),
        columns: columns.iter().map(|column| (*column).to_owned()).collect(),
        unique,
        partial,
    }
}

/// The exact schema v6 DDL emitted by Python `Storage.initialize`.
///
/// This is the single production source of truth for the schema that
/// [`initialize`] creates: it mirrors the frozen Python DDL (including
/// `IF NOT EXISTS`) and produces exactly the tables, columns, indexes and
/// foreign keys described by [`v6_contract`]. The tests create their synthetic
/// databases from this same constant, so the DDL and the structural contract
/// can never drift apart.
const V6_SCHEMA_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS tasks (
    task_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    workspace TEXT NOT NULL,
    status TEXT NOT NULL,
    session_id TEXT,
    task TEXT NOT NULL,
    allowed_paths TEXT NOT NULL,
    test_commands TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    base_head TEXT,
    snapshot TEXT,
    revision_count INTEGER NOT NULL DEFAULT 0,
    close_requested_at TEXT,
    close_reason TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_tasks_active
    ON tasks(project_id) WHERE status IN
    ('implementing','awaiting_review','revising','needs_user','failed','delivery_unknown');
CREATE TABLE IF NOT EXISTS rounds (
    task_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    round_number INTEGER NOT NULL,
    request_id TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    outbound_message_id TEXT,
    attempted INTEGER NOT NULL DEFAULT 0,
    response_message_id TEXT,
    response TEXT,
    error_code TEXT,
    result_json TEXT,
    findings TEXT,
    session_id TEXT,
    worker_started_at TEXT,
    worker_deadline_at TEXT,
    verifier_state TEXT,
    verifier_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (task_id, round_number),
    FOREIGN KEY (task_id) REFERENCES tasks(task_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_rounds_request ON rounds(project_id, request_id);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL,
    round_number INTEGER,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_events_task ON events(task_id, id);
"#;

/// The frozen schema v6 contract, transcribed from
/// `docs/fixtures/sqlite/expected.json` and verified against it by the tests.
fn v6_contract() -> Contract {
    Contract {
        tables: vec![
            Table {
                name: "events".to_owned(),
                columns: vec![
                    column("id", "INTEGER", false, 1),
                    column("task_id", "TEXT", true, 0),
                    column("round_number", "INTEGER", false, 0),
                    column("kind", "TEXT", true, 0),
                    column("message", "TEXT", true, 0),
                    column("created_at", "TEXT", true, 0),
                ],
            },
            Table {
                name: "meta".to_owned(),
                columns: vec![
                    column("key", "TEXT", false, 1),
                    column("value", "TEXT", true, 0),
                ],
            },
            Table {
                name: "rounds".to_owned(),
                columns: vec![
                    column("task_id", "TEXT", true, 1),
                    column("project_id", "TEXT", true, 0),
                    column("round_number", "INTEGER", true, 2),
                    column("request_id", "TEXT", true, 0),
                    column("payload_hash", "TEXT", true, 0),
                    column("kind", "TEXT", true, 0),
                    column("status", "TEXT", true, 0),
                    column("outbound_message_id", "TEXT", false, 0),
                    column("attempted", "INTEGER", true, 0),
                    column("response_message_id", "TEXT", false, 0),
                    column("response", "TEXT", false, 0),
                    column("error_code", "TEXT", false, 0),
                    column("result_json", "TEXT", false, 0),
                    column("findings", "TEXT", false, 0),
                    column("session_id", "TEXT", false, 0),
                    column("worker_started_at", "TEXT", false, 0),
                    column("worker_deadline_at", "TEXT", false, 0),
                    column("verifier_state", "TEXT", false, 0),
                    column("verifier_json", "TEXT", false, 0),
                    column("created_at", "TEXT", true, 0),
                    column("updated_at", "TEXT", true, 0),
                ],
            },
            Table {
                name: "tasks".to_owned(),
                columns: vec![
                    column("task_id", "TEXT", false, 1),
                    column("project_id", "TEXT", true, 0),
                    column("workspace", "TEXT", true, 0),
                    column("status", "TEXT", true, 0),
                    column("session_id", "TEXT", false, 0),
                    column("task", "TEXT", true, 0),
                    column("allowed_paths", "TEXT", true, 0),
                    column("test_commands", "TEXT", true, 0),
                    column("created_at", "TEXT", true, 0),
                    column("updated_at", "TEXT", true, 0),
                    column("base_head", "TEXT", false, 0),
                    column("snapshot", "TEXT", false, 0),
                    column("revision_count", "INTEGER", true, 0),
                    column("close_requested_at", "TEXT", false, 0),
                    column("close_reason", "TEXT", false, 0),
                ],
            },
        ],
        indexes: vec![
            index("ix_events_task", "events", &["task_id", "id"], false, false),
            index(
                "ux_rounds_request",
                "rounds",
                &["project_id", "request_id"],
                true,
                false,
            ),
            index("ux_tasks_active", "tasks", &["project_id"], true, true),
        ],
        foreign_keys: vec![ForeignKey {
            table: "rounds".to_owned(),
            from: "task_id".to_owned(),
            to_table: "tasks".to_owned(),
            to_column: "task_id".to_owned(),
            on_update: "NO ACTION".to_owned(),
            on_delete: "NO ACTION".to_owned(),
        }],
    }
}

fn validate_schema(
    tables: &[Table],
    indexes: &[Index],
    foreign_keys: &[ForeignKey],
) -> Result<(), SchemaMismatch> {
    let contract = v6_contract();
    validate_tables(&contract.tables, tables)?;
    validate_indexes(&contract.indexes, indexes)?;
    validate_foreign_keys(&contract.foreign_keys, foreign_keys)
}

fn validate_tables(expected: &[Table], observed: &[Table]) -> Result<(), SchemaMismatch> {
    for expected_table in expected {
        if !observed
            .iter()
            .any(|table| table.name == expected_table.name)
        {
            return Err(SchemaMismatch::MissingTable {
                table: expected_table.name.clone(),
            });
        }
    }
    for observed_table in observed {
        if !expected
            .iter()
            .any(|table| table.name == observed_table.name)
        {
            return Err(SchemaMismatch::UnexpectedTable {
                table: observed_table.name.clone(),
            });
        }
    }
    for expected_table in expected {
        let Some(observed_table) = observed
            .iter()
            .find(|table| table.name == expected_table.name)
        else {
            return Err(SchemaMismatch::MissingTable {
                table: expected_table.name.clone(),
            });
        };
        validate_columns(expected_table, observed_table)?;
    }
    Ok(())
}

fn validate_columns(expected: &Table, observed: &Table) -> Result<(), SchemaMismatch> {
    for expected_column in &expected.columns {
        match observed
            .columns
            .iter()
            .find(|column| column.name == expected_column.name)
        {
            None => {
                return Err(SchemaMismatch::MissingColumn {
                    table: expected.name.clone(),
                    column: expected_column.name.clone(),
                });
            }
            Some(observed_column) if observed_column != expected_column => {
                return Err(SchemaMismatch::ColumnDefinition {
                    table: expected.name.clone(),
                    column: expected_column.name.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for observed_column in &observed.columns {
        if !expected
            .columns
            .iter()
            .any(|column| column.name == observed_column.name)
        {
            return Err(SchemaMismatch::UnexpectedColumn {
                table: expected.name.clone(),
                column: observed_column.name.clone(),
            });
        }
    }
    Ok(())
}

fn validate_indexes(expected: &[Index], observed: &[Index]) -> Result<(), SchemaMismatch> {
    let mut matched = vec![false; observed.len()];
    for expected_index in expected {
        if let Some(position) = observed
            .iter()
            .enumerate()
            .find_map(|(position, candidate)| {
                (candidate == expected_index && !matched[position]).then_some(position)
            })
        {
            matched[position] = true;
        } else if observed
            .iter()
            .any(|candidate| candidate.name == expected_index.name)
        {
            return Err(SchemaMismatch::IndexDefinition {
                index: expected_index.name.clone(),
            });
        } else {
            return Err(SchemaMismatch::MissingIndex {
                index: expected_index.name.clone(),
            });
        }
    }
    for (position, observed_index) in observed.iter().enumerate() {
        if !matched[position] {
            return Err(SchemaMismatch::UnexpectedIndex {
                index: observed_index.name.clone(),
            });
        }
    }
    Ok(())
}

fn validate_foreign_keys(
    expected: &[ForeignKey],
    observed: &[ForeignKey],
) -> Result<(), SchemaMismatch> {
    let mut matched = vec![false; observed.len()];
    for expected_key in expected {
        if let Some(position) = observed
            .iter()
            .enumerate()
            .find_map(|(position, candidate)| {
                (candidate == expected_key && !matched[position]).then_some(position)
            })
        {
            matched[position] = true;
        } else if observed.iter().any(|candidate| {
            candidate.table == expected_key.table && candidate.from == expected_key.from
        }) {
            return Err(SchemaMismatch::ForeignKeyDefinition {
                table: expected_key.table.clone(),
                from: expected_key.from.clone(),
            });
        } else {
            return Err(SchemaMismatch::MissingForeignKey {
                table: expected_key.table.clone(),
                from: expected_key.from.clone(),
            });
        }
    }
    for (position, observed_key) in observed.iter().enumerate() {
        if !matched[position] {
            return Err(SchemaMismatch::UnexpectedForeignKey {
                table: observed_key.table.clone(),
                from: observed_key.from.clone(),
            });
        }
    }
    Ok(())
}

/// The `PRAGMA journal_mode` required for every runtime connection.
pub const JOURNAL_MODE: &str = "wal";

/// The `PRAGMA busy_timeout` (milliseconds) required for every runtime
/// connection, matching Python `Storage.connect`.
pub const BUSY_TIMEOUT_MS: i64 = 30_000;

/// The exact, ordered list of the fifteen schema v6 `tasks` columns.
///
/// Used for historical task inserts. Reads select all available columns and map
/// through [`Task::from_row`] so modern optional budget validation is included.
const TASK_COLUMNS: &str = "task_id, project_id, workspace, status, session_id, task, \
     allowed_paths, test_commands, created_at, updated_at, base_head, snapshot, \
     revision_count, close_requested_at, close_reason";

/// The exact, ordered list of the twenty-one schema v6 `rounds` columns.
///
/// This is the single production source for the round column list used by
/// atomic task creation; it mirrors the frozen Python `_ROUND_COLUMNS` and the
/// committed fixture rows.
const ROUND_COLUMNS: &str = "task_id, project_id, round_number, request_id, payload_hash, \
     kind, status, outbound_message_id, attempted, response_message_id, response, error_code, \
     result_json, findings, session_id, worker_started_at, worker_deadline_at, verifier_state, \
     verifier_json, created_at, updated_at";

/// A read-write SQLite connection already configured exactly like Python
/// `Storage.connect`.
///
/// A value of this type can only be produced by [`connect`], which applies and
/// then verifies `PRAGMA journal_mode=WAL`, `PRAGMA foreign_keys=ON` and
/// `PRAGMA busy_timeout=30000`. The wrapped [`Connection`] is closed when this
/// value is dropped.
#[derive(Debug)]
pub struct StorageConnection {
    connection: Connection,
}

impl StorageConnection {
    /// The configured connection, for running queries.
    ///
    /// The connection already has WAL, foreign keys and the busy timeout in
    /// effect; callers must not reconfigure them.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The configured connection mutably, for statements that need `&mut`.
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    /// Returns the task with the exact `task_id`, or `None` when it is absent.
    ///
    /// This mirrors Python `Storage.get_task`: it is not scoped by project and
    /// matches the primary key exactly. The found row is mapped through
    /// [`Task::from_row`].
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::TaskRow`] when the found row is corrupted, and
    /// [`QueryError::Database`] for an unexpected SQLite failure. No error
    /// message contains row data, identifiers, SQL or paths.
    pub fn get_task(&self, task_id: TaskId) -> Result<Option<Task>, QueryError> {
        let sql = "SELECT * FROM tasks WHERE task_id = ?";
        let row = self
            .connection
            .query_row(sql, params![task_id.to_string()], |row| {
                Ok(map_task_runtime(&self.connection, row))
            })
            .optional()
            .map_err(QueryError::Database)?;
        match row {
            Some(Ok(task)) => Ok(Some(task)),
            Some(Err(error)) => Err(QueryError::TaskRow(error)),
            None => Ok(None),
        }
    }

    /// Returns the single active task of `project_id`, or `None` when the
    /// project has no active task.
    ///
    /// Active statuses are exactly the [`TaskStatus::is_active`] vocabulary
    /// (`implementing`, `awaiting_review`, `revising`, `needs_user`, `failed`,
    /// `delivery_unknown`). The project invariant `ux_tasks_active` guarantees
    /// at most one such row. The found row is mapped through [`Task::from_row`].
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::TaskRow`] when the found row is corrupted, and
    /// [`QueryError::Database`] for an unexpected SQLite failure. No error
    /// message contains row data, identifiers, SQL or paths.
    pub fn get_active_task(&self, project_id: &ProjectId) -> Result<Option<Task>, QueryError> {
        let (filter, statuses) = active_status_filter();
        let sql = format!("SELECT * FROM tasks WHERE project_id = ? AND {filter}");
        let mut parameters = Vec::with_capacity(statuses.len() + 1);
        parameters.push(SqlValue::Text(project_id.as_str().to_owned()));
        parameters.extend(statuses);
        let row = self
            .connection
            .query_row(&sql, params_from_iter(parameters), |row| {
                Ok(Task::from_row(row))
            })
            .optional()
            .map_err(QueryError::Database)?;
        match row {
            Some(Ok(task)) => Ok(Some(task)),
            Some(Err(error)) => Err(QueryError::TaskRow(error)),
            None => Ok(None),
        }
    }

    /// Lists tasks of `project_id` in a deterministic, paginated order.
    ///
    /// The order is strictly `updated_at DESC, created_at DESC, task_id DESC`,
    /// so equal timestamps cannot make a task appear on two pages or be skipped
    /// between them. When `active_only` is set, only the exact
    /// [`TaskStatus::is_active`] vocabulary is returned. `limit` must be
    /// positive and `offset` non-negative; an `offset` past the end yields an
    /// empty list. Every returned row is mapped through [`Task::from_row`].
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::InvalidLimit`] when `limit` is not positive,
    /// [`QueryError::InvalidOffset`] when `offset` is negative,
    /// [`QueryError::TaskRow`] when a returned row is corrupted, and
    /// [`QueryError::Database`] for an unexpected SQLite failure. No error
    /// message contains row data, identifiers, SQL or paths.
    pub fn list_tasks(
        &self,
        project_id: &ProjectId,
        active_only: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Task>, QueryError> {
        if limit < 1 {
            return Err(QueryError::InvalidLimit);
        }
        if offset < 0 {
            return Err(QueryError::InvalidOffset);
        }

        let mut sql = String::from("SELECT * FROM tasks WHERE project_id = ?");
        let mut parameters = vec![SqlValue::Text(project_id.as_str().to_owned())];
        if active_only {
            let (filter, statuses) = active_status_filter();
            sql.push_str(" AND ");
            sql.push_str(&filter);
            parameters.extend(statuses);
        }
        sql.push_str(" ORDER BY updated_at DESC, created_at DESC, task_id DESC LIMIT ? OFFSET ?");
        parameters.push(SqlValue::Integer(limit));
        parameters.push(SqlValue::Integer(offset));

        let mut statement = self
            .connection
            .prepare(&sql)
            .map_err(QueryError::Database)?;
        let rows = statement
            .query_map(params_from_iter(parameters), |row| Ok(Task::from_row(row)))
            .map_err(QueryError::Database)?;
        let mut tasks = Vec::new();
        for row in rows {
            let mapped = row.map_err(QueryError::Database)?;
            tasks.push(mapped.map_err(QueryError::TaskRow)?);
        }
        Ok(tasks)
    }

    /// Counts tasks of `project_id`, optionally restricted to active tasks.
    ///
    /// This uses the same project and active-status filter as
    /// [`StorageConnection::list_tasks`]. The result is the exact row count and
    /// is therefore never negative. It does not map rows, so a corrupted row
    /// does not turn a count into an error.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::Database`] for an unexpected SQLite failure. No
    /// error message contains row data, identifiers, SQL or paths.
    pub fn count_tasks(
        &self,
        project_id: &ProjectId,
        active_only: bool,
    ) -> Result<i64, QueryError> {
        let mut sql = String::from("SELECT COUNT(*) FROM tasks WHERE project_id = ?");
        let mut parameters = vec![SqlValue::Text(project_id.as_str().to_owned())];
        if active_only {
            let (filter, statuses) = active_status_filter();
            sql.push_str(" AND ");
            sql.push_str(&filter);
            parameters.extend(statuses);
        }
        self.connection
            .query_row(&sql, params_from_iter(parameters), |row| row.get(0))
            .map_err(QueryError::Database)
    }

    /// Atomically creates a task, its initial implementation round and its
    /// `created` event, or replays an already-committed request.
    ///
    /// The whole operation runs inside one `BEGIN IMMEDIATE` transaction, so the
    /// check for an existing request and the three inserts are serialized with
    /// every other writer. On a fresh request exactly these rows are written:
    ///
    /// * one `tasks` row with `status = implementing` and `revision_count = 0`;
    /// * one `rounds` row with `round_number = 1`, `kind = implement`,
    ///   `status = pending` and `attempted = 0`;
    /// * one `events` row with `kind = created` and message
    ///   `task created (implement)`.
    ///
    /// All three rows share the exact same UTC RFC3339 timestamp (millisecond
    /// precision) for `created_at`/`updated_at`. The initial round kind is never
    /// caller-controlled: it is always [`RoundKind::Implement`].
    ///
    /// When the project already has a round for the exact `request_id`, the
    /// existing round is mapped through [`RoundRow::from_row`] and the linked
    /// task through [`Task::from_row`] *without writing anything*:
    ///
    /// * an `implement`, `round_number = 1` round with the same `payload_hash`
    ///   and a linked task in the same project returns
    ///   [`CreateTaskOutcome::Replayed`] with the original task; the newly
    ///   supplied `task_id` and every other payload field are ignored and never
    ///   replace persisted data;
    /// * a different `payload_hash`, or a round whose kind is not `implement`,
    ///   is a [`CreateTaskError::RequestConflict`];
    /// * a round that does not map, a non-initial `implement` round, a missing
    ///   or unmappable linked task, or a linked task in another project is a
    ///   fail-closed [`CreateTaskError::InvalidPersistedState`].
    ///
    /// A request with no existing round keeps the task-3.7 semantics: input is
    /// validated before the transaction, and an active task for another request
    /// is a [`CreateTaskError::ProjectBusy`].
    ///
    /// The returned [`Task`] is read back inside the same transaction through
    /// the production [`Task::from_row`] contract used by
    /// [`StorageConnection::get_task`], so the value is the exact persisted row.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`CreateTaskError`]). Invalid input and
    /// serialization failures are detected before the transaction starts, so
    /// they never write anything. No error message contains ids, project, task
    /// text, workspace, request id, payload hash, paths, SQL or JSON.
    pub fn create_task(
        &mut self,
        input: CreateTaskInput,
    ) -> Result<CreateTaskOutcome, CreateTaskError> {
        self.create_task_with_admission(
            input,
            &AdmissionSettings::default(),
            TaskStatus::Implementing,
        )
    }

    /// Creates a ready or dependency-waiting task with validated admission settings.
    ///
    /// Settings come from project config; input workspace is that project's
    /// absolute workspace. v15 counts all unfinished tasks and checks scopes
    /// against real writers and reservations. Request replay precedes admission.
    /// Waiting tasks reserve no writer slot until explicit activation.
    /// Legacy v6 supports only historical defaults and implementing status.
    ///
    /// # Errors
    /// Bounds, overlapping/corrupt scopes and SQLite failures roll back task,
    /// round, event and reservation together. Errors never render inputs.
    pub fn create_task_with_admission(
        &mut self,
        input: CreateTaskInput,
        settings: &AdmissionSettings,
        initial_status: TaskStatus,
    ) -> Result<CreateTaskOutcome, CreateTaskError> {
        self.create_task_with_budget(input, settings, initial_status, None)
    }

    /// Creates task, round, event, writer reservation and optional validated budget
    /// in one transaction. Replay returns the existing budget without overwriting it.
    /// A budget requires schema v15; legacy callers without a budget remain supported.
    /// The trusted request payload hash must include the submitted budget.
    /// # Errors
    /// Storage/admission failures roll back every row; invalid legacy budget input
    /// is rejected. Errors never render budget values.
    pub fn create_task_with_budget(
        &mut self,
        input: CreateTaskInput,
        settings: &AdmissionSettings,
        initial_status: TaskStatus,
        budget: Option<&TaskBudget>,
    ) -> Result<CreateTaskOutcome, CreateTaskError> {
        self.create_task_with_profile_inner(
            input,
            settings,
            initial_status,
            budget,
            None,
            None,
            None,
        )
    }

    /// Pins every new task's effective profile, including the historical built-in
    /// implementer. All profile columns commit with task/round/reservation/event.
    /// Caller calculates the submission payload hash including non-historical
    /// profile identity. Replay never updates a previously pinned snapshot.
    pub fn create_task_with_profile(
        &mut self,
        input: CreateTaskInput,
        settings: &AdmissionSettings,
        initial_status: TaskStatus,
        budget: Option<&TaskBudget>,
        profile: &bridge_domain::ProfileSnapshot,
    ) -> Result<CreateTaskOutcome, CreateTaskError> {
        profile
            .validate()
            .map_err(|_| CreateTaskError::InvalidInput)?;
        self.create_task_with_profile_inner(
            input,
            settings,
            initial_status,
            budget,
            Some(profile),
            None,
            None,
        )
    }

    /// Creates the task/profile and its deterministic pending checkout row in
    /// one transaction. Git creation is a later worker operation.
    /// # Errors
    /// Requires frozen worktree mode and a checkout bound to input.base_head.
    pub fn create_task_with_profile_and_checkout(
        &mut self,
        input: CreateTaskInput,
        settings: &AdmissionSettings,
        initial_status: TaskStatus,
        budget: Option<&TaskBudget>,
        profile: &bridge_domain::ProfileSnapshot,
        checkout: &PendingCheckout,
    ) -> Result<CreateTaskOutcome, CreateTaskError> {
        profile
            .validate()
            .map_err(|_| CreateTaskError::InvalidInput)?;
        if settings.execution_mode() != bridge_domain::ExecutionMode::Worktree
            || input.base_head.as_deref() != Some(checkout.base_head.as_str())
            || !Path::new(&checkout.path).is_absolute()
            || !Path::new(&checkout.runtime_dir).is_absolute()
        {
            return Err(CreateTaskError::InvalidInput);
        }
        self.create_task_with_profile_inner(
            input,
            settings,
            initial_status,
            budget,
            Some(profile),
            Some(checkout),
            None,
        )
    }

    /// Atomically stores workflow metadata with task, profile, checkout and first round.
    #[allow(clippy::too_many_arguments)] // Frozen creation inputs share one transaction.
    pub fn create_task_with_workflow(
        &mut self,
        input: CreateTaskInput,
        settings: &AdmissionSettings,
        initial_status: TaskStatus,
        budget: Option<&TaskBudget>,
        profile: &bridge_domain::ProfileSnapshot,
        checkout: Option<&PendingCheckout>,
        workflow: &bridge_domain::WorkflowMetadata,
    ) -> Result<CreateTaskOutcome, CreateTaskError> {
        profile
            .validate()
            .map_err(|_| CreateTaskError::InvalidInput)?;
        workflow
            .validate()
            .map_err(|_| CreateTaskError::InvalidInput)?;
        if !workflow.depends_on.is_empty() && workflow.workflow_id.is_none() {
            return Err(CreateTaskError::InvalidInput);
        }
        if let Some(c) = checkout
            && (settings.execution_mode() != bridge_domain::ExecutionMode::Worktree
                || input.base_head.as_deref() != Some(&c.base_head)
                || !Path::new(&c.path).is_absolute()
                || !Path::new(&c.runtime_dir).is_absolute())
        {
            return Err(CreateTaskError::InvalidInput);
        }
        self.create_task_with_profile_inner(
            input,
            settings,
            initial_status,
            budget,
            Some(profile),
            checkout,
            Some(workflow),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_task_with_profile_inner(
        &mut self,
        input: CreateTaskInput,
        settings: &AdmissionSettings,
        initial_status: TaskStatus,
        budget: Option<&TaskBudget>,
        profile: Option<&bridge_domain::ProfileSnapshot>,
        checkout: Option<&PendingCheckout>,
        workflow: Option<&bridge_domain::WorkflowMetadata>,
    ) -> Result<CreateTaskOutcome, CreateTaskError> {
        let profile_json = profile
            .map(|p| {
                p.canonical_json()
                    .map_err(|_| CreateTaskError::InvalidInput)
            })
            .transpose()?;
        let profile_hash = profile
            .map(|p| {
                p.canonical_hash()
                    .map_err(|_| CreateTaskError::InvalidInput)
            })
            .transpose()?;
        if !initial_status.is_active() {
            return Err(CreateTaskError::InvalidInput);
        }
        let prepared = PreparedCreateTask::new(input)?;
        let now = utc_now_rfc3339_millis();

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(CreateTaskError::Database)?;

        if let Some(existing) = find_existing_request(&transaction, &prepared)? {
            let task = replay_existing_request(&transaction, &prepared, existing)?;
            transaction.commit().map_err(CreateTaskError::Database)?;
            return Ok(CreateTaskOutcome::Replayed(task));
        }

        let v15 = matches!(
            query_user_version(&transaction).map_err(CreateTaskError::Database)?,
            15..=17
        );
        if !v15
            && (*settings != AdmissionSettings::default()
                || initial_status != TaskStatus::Implementing
                || budget.is_some()
                || profile.is_some()
                || workflow.is_some())
        {
            return Err(CreateTaskError::InvalidInput);
        }
        let schema = query_user_version(&transaction).map_err(CreateTaskError::Database)?;
        if schema < 16 && settings.delivery_mode() != bridge_domain::DeliveryMode::Manual {
            return Err(CreateTaskError::InvalidInput);
        }
        let active: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM tasks WHERE project_id=?1 AND status IN ('waiting_dependencies','implementing','awaiting_review','revising','needs_user','failed','delivery_unknown')",
            [&prepared.project_id], |row| row.get(0),
        ).map_err(CreateTaskError::Database)?;
        if u64::try_from(active).map_err(|_| CreateTaskError::InvalidInput)?
            >= settings.max_active_tasks()
        {
            return Err(CreateTaskError::ProjectBusy);
        }
        if v15 {
            let scopes = writers::parse_scopes(&prepared.allowed_paths_json)
                .map_err(classify_writer_create_error)?;
            if writers::is_writer(initial_status) || settings.allow_parallel_writers() {
                writers::check_admission(
                    &transaction,
                    &prepared.project_id,
                    &scopes,
                    Path::new(&prepared.workspace),
                    settings.allow_parallel_writers(),
                    &prepared.task_id.to_string(),
                )
                .map_err(classify_writer_create_error)?;
            }
        }

        transaction
            .execute(
                &format!(
                    "INSERT INTO tasks ({TASK_COLUMNS}) VALUES \
                     (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"
                ),
                params![
                    prepared.task_id.to_string(),
                    prepared.project_id,
                    prepared.workspace,
                    initial_status.as_str(),
                    Null,
                    prepared.text,
                    prepared.allowed_paths_json,
                    prepared.test_commands_json,
                    now,
                    now,
                    prepared.base_head,
                    prepared.snapshot_json,
                    0_i64,
                    Null,
                    Null,
                ],
            )
            .map_err(classify_task_insert_error)?;

        if matches!(schema, 16 | 17) {
            transaction
                .execute(
                    "UPDATE tasks SET delivery_mode=?1 WHERE task_id=?2",
                    params![
                        settings.delivery_mode().as_str(),
                        prepared.task_id.to_string()
                    ],
                )
                .map_err(CreateTaskError::Database)?;
        }
        if v15 {
            transaction
                .execute(
                    "UPDATE tasks SET execution_mode=?1, budget_json=?3 WHERE task_id=?2",
                    params![
                        settings.execution_mode().as_str(),
                        prepared.task_id.to_string(),
                        budget.map(|b| b.as_json().to_string())
                    ],
                )
                .map_err(CreateTaskError::Database)?;
            if let Some(workflow) = workflow {
                transaction
                    .execute(
                        "UPDATE tasks SET workflow_id=?1,depends_on=?2 WHERE task_id=?3",
                        params![
                            workflow.workflow_id.as_ref().map(|v| v.as_str()),
                            serde_json::to_string(&workflow.depends_on)
                                .map_err(|_| CreateTaskError::InvalidInput)?,
                            prepared.task_id.to_string()
                        ],
                    )
                    .map_err(CreateTaskError::Database)?;
            }
            if let Some(profile) = profile {
                transaction.execute("UPDATE tasks SET profile=?1,profile_json=?2,profile_hash=?3,profile_source=?4 WHERE task_id=?5", params![profile.id,profile_json,profile_hash,profile.origin.as_str(),prepared.task_id.to_string()]).map_err(CreateTaskError::Database)?;
            }
            if let Some(checkout) = checkout {
                transaction.execute("INSERT INTO worktrees(task_id,path,runtime_dir,base_head,status,delivery_state,created_at,updated_at) VALUES (?1,?2,?3,?4,'pending','none',?5,?5)",params![prepared.task_id.to_string(),checkout.path,checkout.runtime_dir,checkout.base_head,now]).map_err(CreateTaskError::Database)?;
            }
        }

        transaction
            .execute(
                &format!(
                    "INSERT INTO rounds ({ROUND_COLUMNS}) VALUES \
                     (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)"
                ),
                params![
                    prepared.task_id.to_string(),
                    prepared.project_id,
                    1_i64,
                    prepared.request_id,
                    prepared.payload_hash,
                    RoundKind::Implement.as_str(),
                    RoundStatus::Pending.as_str(),
                    Null,
                    0_i64,
                    Null,
                    Null,
                    Null,
                    Null,
                    Null,
                    Null,
                    Null,
                    Null,
                    Null,
                    Null,
                    now,
                    now,
                ],
            )
            .map_err(classify_round_insert_error)?;

        if v15 && writers::is_writer(initial_status) {
            writers::reserve(
                &transaction,
                &prepared.task_id.to_string(),
                &prepared.project_id,
                &prepared.allowed_paths_json,
                &now,
                settings.allow_parallel_writers(),
            )
            .map_err(classify_writer_create_error)?;
        }

        transaction
            .execute(
                "INSERT INTO events (task_id, round_number, kind, message, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    prepared.task_id.to_string(),
                    1_i64,
                    "created",
                    format!("task created ({})", RoundKind::Implement.as_str()),
                    now,
                ],
            )
            .map_err(CreateTaskError::Database)?;

        let task = transaction
            .query_row(
                "SELECT * FROM tasks WHERE task_id = ?1",
                params![prepared.task_id.to_string()],
                |row| Ok(Task::from_row(row)),
            )
            .map_err(CreateTaskError::Database)?
            .map_err(CreateTaskError::TaskRow)?;

        transaction.commit().map_err(CreateTaskError::Database)?;

        Ok(CreateTaskOutcome::Created(task))
    }
}

/// The outcome of an idempotent [`StorageConnection::create_task`] call.
///
/// [`Created`](Self::Created) means this call inserted the task, its initial
/// round and its `created` event, so the caller owns starting the worker.
/// [`Replayed`](Self::Replayed) means an identical request had already been
/// committed: no rows were written and the caller must *not* start the worker
/// again. Both variants carry the persisted [`Task`], read back through
/// [`Task::from_row`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CreateTaskOutcome {
    /// The request was new and its rows were inserted.
    Created(Task),
    /// The request was already committed and its original task was returned.
    Replayed(Task),
}

impl CreateTaskOutcome {
    /// The persisted task, whether it was created or replayed.
    #[must_use]
    pub fn task(&self) -> &Task {
        match self {
            Self::Created(task) | Self::Replayed(task) => task,
        }
    }

    /// Consumes the outcome, returning the persisted task.
    #[must_use]
    pub fn into_task(self) -> Task {
        match self {
            Self::Created(task) | Self::Replayed(task) => task,
        }
    }

    /// Whether this outcome is a [`CreateTaskOutcome::Created`].
    #[must_use]
    pub fn is_created(&self) -> bool {
        matches!(self, Self::Created(_))
    }

    /// Whether this outcome is a [`CreateTaskOutcome::Replayed`].
    #[must_use]
    pub fn is_replayed(&self) -> bool {
        matches!(self, Self::Replayed(_))
    }
}

/// Deterministic pending checkout metadata; storage never acts on these paths.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingCheckout {
    pub path: String,
    pub runtime_dir: String,
    pub base_head: String,
}

/// Input for [`StorageConnection::create_task`].
///
/// The initial round kind is intentionally not a field: task creation always
/// writes a [`RoundKind::Implement`] round. `snapshot` must be `None` or a JSON
/// object; any other shape is rejected before the transaction starts.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTaskInput {
    /// Primary key of the new task.
    pub task_id: TaskId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Workspace path persisted verbatim (`workspace`).
    pub workspace: String,
    /// Task text persisted verbatim (`task`).
    pub task: String,
    /// Idempotency key stored on the initial round (`request_id`).
    pub request_id: String,
    /// Hash of the request payload (`payload_hash`).
    pub payload_hash: String,
    /// Base Git head, when captured (`base_head`).
    pub base_head: Option<String>,
    /// Normalized workspace-relative or authorized absolute scopes, serialized
    /// as a JSON array of strings; trailing `/` denotes a directory scope.
    pub allowed_paths: Vec<String>,
    /// Verification commands, serialized as a JSON array of strings.
    pub test_commands: Vec<String>,
    /// Workspace snapshot: `None` or a JSON object (`snapshot`).
    pub snapshot: Option<serde_json::Value>,
}

/// A validated, serialized create-task request ready for the transaction.
///
/// This is produced only by [`PreparedCreateTask::new`], which performs every
/// input check and JSON serialization *before* any write, so an invalid request
/// can never touch the database.
struct PreparedCreateTask {
    task_id: TaskId,
    project_id: String,
    workspace: String,
    text: String,
    request_id: String,
    payload_hash: String,
    base_head: Option<String>,
    allowed_paths_json: String,
    test_commands_json: String,
    snapshot_json: Option<String>,
}

impl PreparedCreateTask {
    /// Validates and serializes `input` without touching the database.
    ///
    /// Required text values that would make the persisted row inconsistent
    /// (`workspace`, `task`, `request_id`, `payload_hash`) must be non-empty,
    /// and `snapshot` must be `None` or a JSON object. Path/command safety
    /// policy is deliberately out of scope here.
    fn new(input: CreateTaskInput) -> Result<Self, CreateTaskError> {
        if input.workspace.is_empty() {
            return Err(CreateTaskError::InvalidInput);
        }
        if input.task.trim().is_empty() {
            return Err(CreateTaskError::InvalidInput);
        }
        if input.request_id.is_empty() {
            return Err(CreateTaskError::InvalidInput);
        }
        if input.payload_hash.is_empty() {
            return Err(CreateTaskError::InvalidInput);
        }
        if let Some(snapshot) = &input.snapshot
            && !snapshot.is_object()
        {
            return Err(CreateTaskError::InvalidInput);
        }

        let allowed_paths_json = serde_json::to_string(&input.allowed_paths)
            .map_err(|_| CreateTaskError::Serialization)?;
        let test_commands_json = serde_json::to_string(&input.test_commands)
            .map_err(|_| CreateTaskError::Serialization)?;
        let snapshot_json = match &input.snapshot {
            Some(snapshot) => {
                Some(serde_json::to_string(snapshot).map_err(|_| CreateTaskError::Serialization)?)
            }
            None => None,
        };

        Ok(Self {
            task_id: input.task_id,
            project_id: input.project_id.to_string(),
            workspace: input.workspace,
            text: input.task,
            request_id: input.request_id,
            payload_hash: input.payload_hash,
            base_head: input.base_head,
            allowed_paths_json,
            test_commands_json,
            snapshot_json,
        })
    }
}

/// A typed, safe error raised while atomically creating a task.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that never contains ids, project, task text, workspace, request id,
/// payload hash, paths, SQL or JSON. The underlying task-row or SQLite error,
/// when present, is reachable only through [`Error::source`].
#[non_exhaustive]
pub enum CreateTaskError {
    /// A scope overlaps another active writer under parallel admission.
    ScopeOverlap,
    /// Incoming or persisted scopes are malformed or unresolvable.
    ScopeDataError,
    /// A task bound or the single-writer slot is already occupied.
    ProjectBusy,
    /// A task with the same `task_id` already exists.
    TaskIdConflict,
    /// The `request_id` is already used in this project with a different
    /// payload hash, or with a non-`implement` round kind.
    RequestConflict,
    /// The input is invalid: empty required text or a non-object snapshot.
    InvalidInput,
    /// A JSON field could not be serialized.
    Serialization,
    /// The created `tasks` row could not be mapped.
    TaskRow(TaskRowError),
    /// An existing request maps to a corrupt or inconsistent persisted state.
    InvalidPersistedState(ReplayStateError),
    /// An unexpected SQLite failure.
    Database(rusqlite::Error),
}

impl fmt::Display for CreateTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScopeOverlap => f.write_str("scope overlaps an active writer"),
            Self::ScopeDataError => {
                f.write_str("writer scope data is invalid or cannot be resolved")
            }
            Self::ProjectBusy => f.write_str("project already has an unfinished task"),
            Self::TaskIdConflict => f.write_str("task id already exists"),
            Self::RequestConflict => f.write_str("request id is already used in this project"),
            Self::InvalidInput => f.write_str("task creation input is invalid"),
            Self::Serialization => f.write_str("task creation input could not be serialized"),
            Self::TaskRow(_) => f.write_str("task row could not be mapped"),
            Self::InvalidPersistedState(_) => {
                f.write_str("existing request maps to an invalid persisted state")
            }
            Self::Database(_) => f.write_str("storage database error"),
        }
    }
}

impl fmt::Debug for CreateTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

fn classify_writer_create_error(error: WriterError) -> CreateTaskError {
    match error {
        WriterError::ProjectBusy => CreateTaskError::ProjectBusy,
        WriterError::ScopeOverlap => CreateTaskError::ScopeOverlap,
        WriterError::ScopeDataError => CreateTaskError::ScopeDataError,
        WriterError::Database(error) => CreateTaskError::Database(error),
        _ => CreateTaskError::InvalidInput,
    }
}

impl Error for CreateTaskError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TaskRow(error) => Some(error),
            Self::InvalidPersistedState(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// A typed, safe reason why an existing request could not be replayed.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that never contains ids, project, task text, workspace, request id,
/// payload hash, paths, SQL or JSON. The underlying row-mapping error, when
/// present, is reachable only through [`Error::source`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ReplayStateError {
    /// The stored `rounds` row does not map to a valid [`RoundRow`].
    RoundRow(RoundRowError),
    /// The round is an `implement` round but not round number 1.
    InvalidRoundNumber,
    /// The task referenced by the round does not exist.
    MissingTask,
    /// The referenced `tasks` row does not map to a valid [`Task`].
    TaskRow(TaskRowError),
    /// The referenced task belongs to a different project than the round.
    ProjectMismatch,
}

impl fmt::Display for ReplayStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RoundRow(_) => f.write_str("existing request round row is invalid"),
            Self::InvalidRoundNumber => {
                f.write_str("existing request implement round is not the initial round")
            }
            Self::MissingTask => f.write_str("existing request references a missing task"),
            Self::TaskRow(_) => f.write_str("existing request task row is invalid"),
            Self::ProjectMismatch => {
                f.write_str("existing request task belongs to a different project")
            }
        }
    }
}

impl Error for ReplayStateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RoundRow(error) => Some(error),
            Self::TaskRow(error) => Some(error),
            _ => None,
        }
    }
}

/// Looks up the single existing `rounds` row for the exact project and request.
///
/// The project-scoped `ux_rounds_request` index guarantees at most one row. The
/// found row is mapped through [`RoundRow::from_row`] but the mapping result is
/// returned unchanged so the caller can classify a corrupt candidate.
fn find_existing_request(
    connection: &Connection,
    prepared: &PreparedCreateTask,
) -> Result<Option<Result<RoundRow, RoundRowError>>, CreateTaskError> {
    connection
        .query_row(
            &format!(
                "SELECT {ROUND_COLUMNS} FROM rounds WHERE project_id = ?1 AND request_id = ?2"
            ),
            params![prepared.project_id, prepared.request_id],
            |row| Ok(RoundRow::from_row(row)),
        )
        .optional()
        .map_err(CreateTaskError::Database)
}

/// Resolves an existing request into the original [`Task`] or a typed conflict.
///
/// This performs no writes. A same-`payload_hash` `implement`/`round_number=1`
/// round with an existing, same-project linked task replays; every other
/// persisted state is a [`CreateTaskError::RequestConflict`] or a fail-closed
/// [`CreateTaskError::InvalidPersistedState`].
fn replay_existing_request(
    connection: &Connection,
    prepared: &PreparedCreateTask,
    existing: Result<RoundRow, RoundRowError>,
) -> Result<Task, CreateTaskError> {
    let round = existing.map_err(|error| {
        CreateTaskError::InvalidPersistedState(ReplayStateError::RoundRow(error))
    })?;

    if round.kind != RoundKind::Implement || round.payload_hash != prepared.payload_hash {
        return Err(CreateTaskError::RequestConflict);
    }
    if round.round_number != 1 {
        return Err(CreateTaskError::InvalidPersistedState(
            ReplayStateError::InvalidRoundNumber,
        ));
    }

    let task = connection
        .query_row(
            "SELECT * FROM tasks WHERE task_id = ?1",
            params![round.task_id.to_string()],
            |row| Ok(Task::from_row(row)),
        )
        .optional()
        .map_err(CreateTaskError::Database)?
        .ok_or(CreateTaskError::InvalidPersistedState(
            ReplayStateError::MissingTask,
        ))?
        .map_err(|error| {
            CreateTaskError::InvalidPersistedState(ReplayStateError::TaskRow(error))
        })?;

    if task.project_id != round.project_id {
        return Err(CreateTaskError::InvalidPersistedState(
            ReplayStateError::ProjectMismatch,
        ));
    }

    Ok(task)
}

/// Classifies a failed `tasks` insert into the active-project or task-id
/// conflict, falling back to [`CreateTaskError::Database`].
fn classify_task_insert_error(error: rusqlite::Error) -> CreateTaskError {
    if let rusqlite::Error::SqliteFailure(inner, message) = &error {
        if let Some(message) = message {
            if message.contains("tasks.task_id") {
                return CreateTaskError::TaskIdConflict;
            }
            if message.contains("tasks.project_id") {
                return CreateTaskError::ProjectBusy;
            }
        }
        if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY {
            return CreateTaskError::TaskIdConflict;
        }
        if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE {
            return CreateTaskError::ProjectBusy;
        }
    }
    CreateTaskError::Database(error)
}

/// Classifies a failed `rounds` insert as the project-scoped request conflict,
/// falling back to [`CreateTaskError::Database`].
fn classify_round_insert_error(error: rusqlite::Error) -> CreateTaskError {
    if let rusqlite::Error::SqliteFailure(inner, message) = &error {
        if let Some(message) = message
            && (message.contains("rounds.request_id") || message.contains("rounds.project_id"))
        {
            return CreateTaskError::RequestConflict;
        }
        if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE {
            return CreateTaskError::RequestConflict;
        }
    }
    CreateTaskError::Database(error)
}

/// Returns the current UTC time as an RFC3339 string with millisecond precision
/// and a `+00:00` offset, matching Python `Storage.utcnow`
/// (`datetime.now(timezone.utc).isoformat(timespec="milliseconds")`).
fn utc_now_rfc3339_millis() -> String {
    format_rfc3339_millis(SystemTime::now())
}

/// Formats one clock sample as an RFC3339 string with millisecond precision and
/// a `+00:00` offset.
///
/// Taking the [`SystemTime`] as a parameter lets a caller derive several
/// timestamps (for example a worker start and its deadline) from a single clock
/// sample.
fn format_rfc3339_millis(time: SystemTime) -> String {
    let elapsed = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs();
    let milliseconds = elapsed.subsec_millis();
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}+00:00"
    )
}

/// Converts days since the Unix epoch to a proleptic Gregorian `(year, month,
/// day)` triple using Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * month_prime + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    })
    .unwrap_or(1);
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// The exact [`TaskStatus::is_active`] vocabulary, derived from the single
/// domain source of truth instead of hard-coded SQL literals.
fn active_statuses() -> Vec<&'static str> {
    TaskStatus::ALL
        .iter()
        .filter(|status| status.is_active())
        .map(|status| status.as_str())
        .collect()
}

/// Builds the `status IN (?, ...)` filter for [`active_statuses`] together with
/// the matching parameter values, in the same order.
fn active_status_filter() -> (String, Vec<SqlValue>) {
    let statuses = active_statuses();
    let mut filter = String::from("status IN (");
    let mut values = Vec::with_capacity(statuses.len());
    for (index, status) in statuses.into_iter().enumerate() {
        if index > 0 {
            filter.push(',');
        }
        filter.push('?');
        values.push(SqlValue::Text(status.to_owned()));
    }
    filter.push(')');
    (filter, values)
}

/// A typed, safe error raised while running a read-only task query.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that never contains row data, identifiers, project ids, SQL or
/// machine-specific paths. The underlying task-row or SQLite error, when
/// present, is reachable only through [`Error::source`].
#[derive(Debug)]
#[non_exhaustive]
pub enum QueryError {
    /// `limit` is not positive.
    InvalidLimit,
    /// `offset` is negative.
    InvalidOffset,
    /// A found `tasks` row could not be mapped.
    TaskRow(TaskRowError),
    /// An unexpected SQLite failure.
    Database(rusqlite::Error),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit => f.write_str("task query limit must be positive"),
            Self::InvalidOffset => f.write_str("task query offset must be non-negative"),
            Self::TaskRow(_) => f.write_str("task row could not be mapped"),
            Self::Database(_) => f.write_str("storage database error"),
        }
    }
}

impl Error for QueryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TaskRow(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// A typed, safe error raised while opening a runtime connection.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that never contains row data, secrets or machine-specific paths. The
/// underlying SQLite error, when present, is reachable only through
/// [`Error::source`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ConnectError {
    /// The parent directory could not be created.
    CreateDirectory,
    /// The file exists but cannot be opened read-write (for example a
    /// directory, or a file without write permission).
    NotUsable,
    /// The file is not a SQLite database.
    NotADatabase,
    /// A required `PRAGMA` could not be executed.
    Configure,
    /// `PRAGMA journal_mode` did not become `wal`.
    JournalMode { found: String },
    /// `PRAGMA foreign_keys` did not become `1`.
    ForeignKeys { found: i64 },
    /// `PRAGMA busy_timeout` did not become [`BUSY_TIMEOUT_MS`].
    BusyTimeout { found: i64 },
    /// An unexpected SQLite failure.
    Database(rusqlite::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateDirectory => f.write_str("storage directory could not be created"),
            Self::NotUsable => f.write_str("database file cannot be opened read-write"),
            Self::NotADatabase => f.write_str("file is not a SQLite database"),
            Self::Configure => f.write_str("storage connection could not be configured"),
            Self::JournalMode { found } => write!(f, "journal mode is {found}, not wal"),
            Self::ForeignKeys { found } => write!(f, "foreign keys are {found}, not enabled"),
            Self::BusyTimeout { found } => {
                write!(f, "busy timeout is {found} ms, not {}", BUSY_TIMEOUT_MS)
            }
            Self::Database(_) => f.write_str("storage database error"),
        }
    }
}

impl Error for ConnectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// Opens `path` read-write and configures it exactly like Python
/// `Storage.connect`.
///
/// The missing parent directory is created (like `mkdir(parents=True,
/// exist_ok=True)`), a missing database file is created empty, and every
/// connection applies and then verifies `PRAGMA journal_mode=WAL`,
/// `PRAGMA foreign_keys=ON` and `PRAGMA busy_timeout=30000`. If any pragma did
/// not take effect the connection is rejected (fail closed) and closed. The
/// schema, tables and `PRAGMA user_version` are never touched.
///
/// # Errors
///
/// Returns a typed category for filesystem and SQLite problems (see
/// [`ConnectError`]). No error message contains row data, secrets or
/// machine-specific paths.
pub fn connect(path: impl AsRef<Path>) -> Result<StorageConnection, ConnectError> {
    let path = path.as_ref();
    create_parent_directory(path)?;
    let connection = open_read_write(path)?;
    configure(&connection)?;
    Ok(StorageConnection { connection })
}

fn create_parent_directory(path: &Path) -> Result<(), ConnectError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|_| ConnectError::CreateDirectory)?;
    }
    Ok(())
}

fn open_read_write(path: &Path) -> Result<Connection, ConnectError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(path, flags).map_err(classify_open_error)
}

fn classify_open_error(error: rusqlite::Error) -> ConnectError {
    if let rusqlite::Error::SqliteFailure(inner, _) = &error {
        match inner.code {
            ErrorCode::CannotOpen | ErrorCode::ReadOnly => return ConnectError::NotUsable,
            ErrorCode::NotADatabase => return ConnectError::NotADatabase,
            _ => {}
        }
    }
    ConnectError::Database(error)
}

fn classify_configure_error(error: rusqlite::Error) -> ConnectError {
    if let rusqlite::Error::SqliteFailure(inner, _) = &error {
        match inner.code {
            ErrorCode::CannotOpen | ErrorCode::ReadOnly => return ConnectError::NotUsable,
            ErrorCode::NotADatabase => return ConnectError::NotADatabase,
            _ => {}
        }
    }
    ConnectError::Configure
}

// SQLite may return SQLITE_BUSY immediately for a journal-mode conversion even
// with busy_timeout installed. Retry only that contention, within the same bound.
fn configure_journal_mode(connection: &Connection) -> rusqlite::Result<String> {
    // One wall-clock bound rather than a fresh busy_timeout on each attempt.
    connection.busy_timeout(std::time::Duration::ZERO)?;
    let timeout = std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64);
    let deadline = std::time::Instant::now() + timeout;
    let result = loop {
        let result = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0));
        match &result {
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            _ => break result,
        }
    };
    connection.busy_timeout(timeout)?;
    result
}

fn configure(connection: &Connection) -> Result<(), ConnectError> {
    // The journal-mode switch can contend during first initialization too.
    connection
        .execute_batch("PRAGMA busy_timeout=30000;")
        .map_err(classify_configure_error)?;
    let journal_mode = configure_journal_mode(connection).map_err(classify_configure_error)?;
    if !journal_mode.eq_ignore_ascii_case(JOURNAL_MODE) {
        return Err(ConnectError::JournalMode {
            found: journal_mode,
        });
    }

    connection
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=30000;")
        .map_err(classify_configure_error)?;

    let foreign_keys: i64 = connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .map_err(classify_configure_error)?;
    if foreign_keys != 1 {
        return Err(ConnectError::ForeignKeys {
            found: foreign_keys,
        });
    }

    let busy_timeout: i64 = connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .map_err(classify_configure_error)?;
    if busy_timeout != BUSY_TIMEOUT_MS {
        return Err(ConnectError::BusyTimeout {
            found: busy_timeout,
        });
    }

    Ok(())
}

/// A typed, safe error raised while initializing a database.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that never contains row data, secrets or machine-specific paths.
/// Schema object names and version numbers may appear. The underlying
/// connection, inspection or SQLite error, when present, is reachable only
/// through [`Error::source`].
#[derive(Debug)]
#[non_exhaustive]
pub enum InitializeError {
    /// The runtime connection could not be opened or configured.
    Connect(ConnectError),
    /// `PRAGMA user_version` is neither `0` nor [`SCHEMA_VERSION`].
    UnsupportedUserVersion { found: i64 },
    /// A `user_version=0` database that already contains user objects
    /// (a partial or foreign schema) and is therefore never initialized.
    NonEmptyUninitialized,
    /// An existing schema v6 database that does not match the frozen contract.
    Incompatible(InspectError),
    /// An unexpected SQLite failure.
    Database(rusqlite::Error),
}

impl fmt::Display for InitializeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(_) => f.write_str("storage connection could not be opened"),
            Self::UnsupportedUserVersion { found } => {
                write!(f, "unsupported schema version {found}")
            }
            Self::NonEmptyUninitialized => f.write_str("uninitialized database is not empty"),
            Self::Incompatible(_) => {
                f.write_str("database schema is not compatible with schema v6")
            }
            Self::Database(_) => f.write_str("storage database error"),
        }
    }
}

impl Error for InitializeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Connect(error) => Some(error),
            Self::Incompatible(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// Atomically initializes `path` as a compatible, empty schema v6 database.
///
/// The database is opened with the runtime connection ([`connect`]) and the
/// whole initialization runs inside a single `BEGIN IMMEDIATE` transaction, so
/// the DDL and the version markers (`PRAGMA user_version` and
/// `meta.schema_version`) either all commit or all roll back. The resulting
/// database has exactly the tables, columns, indexes and foreign keys of the
/// frozen v6 contract and no user rows.
///
/// The state is classified *after* the write lock is held, so concurrent
/// callers serialize on the writer transaction: the first one creates schema v6
/// and every later one observes the committed v6 schema and performs an
/// idempotent no-op.
///
/// Only a missing file, or a truly empty database (`user_version=0` with no
/// user objects and no schema markers), is initialized. An already compatible
/// v6 database is validated against the full contract and left untouched. Every
/// other state fails closed without repair or upgrade: a different
/// `user_version`, a v6 database that does not match the contract, or a
/// `user_version=0` database that already contains user objects.
///
/// # Errors
///
/// Returns a typed category (see [`InitializeError`]). No error message
/// contains row data, secrets or machine-specific paths.
pub fn initialize(path: impl AsRef<Path>) -> Result<(), InitializeError> {
    let path = path.as_ref();
    let mut storage = connect(path).map_err(InitializeError::Connect)?;
    let connection = storage.connection_mut();

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(InitializeError::Database)?;

    let user_version = query_user_version(&transaction).map_err(InitializeError::Database)?;
    let non_empty = has_user_objects(&transaction).map_err(InitializeError::Database)?;

    match user_version {
        SCHEMA_VERSION => {
            validate_database(&transaction).map_err(InitializeError::Incompatible)?;
            transaction.commit().map_err(InitializeError::Database)?;
        }
        0 if !non_empty => {
            apply_schema_v6(&transaction, V6_SCHEMA_DDL)?;
            transaction.commit().map_err(InitializeError::Database)?;
        }
        0 => return Err(InitializeError::NonEmptyUninitialized),
        found => return Err(InitializeError::UnsupportedUserVersion { found }),
    }

    Ok(())
}

/// Whether `connection` contains any user object (table, index, trigger or
/// view); internal `sqlite_*` objects are ignored.
fn has_user_objects(connection: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Applies the schema v6 DDL and both version markers on an open transaction.
///
/// The caller owns the surrounding transaction; this function only issues
/// statements, so a failure leaves the transaction open for the caller to roll
/// back by dropping it.
fn apply_schema_v6(connection: &Connection, ddl: &str) -> Result<(), InitializeError> {
    connection
        .execute_batch(ddl)
        .map_err(InitializeError::Database)?;
    connection
        .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
        .map_err(InitializeError::Database)?;
    connection
        .execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )
        .map_err(InitializeError::Database)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Rust state isolation (task 3.10).
//
// Rust owns a dedicated state root and derives every runtime artifact from it.
// The layout below is the single production source of Rust runtime paths: it
// never accepts, reads or writes a Python path and cannot produce a path
// outside the explicitly passed Rust state root.
// ---------------------------------------------------------------------------

/// A Rust runtime lock file.
///
/// The names match the frozen runtime contract: `mcp.lock` and `worker.lock`
/// live in the project state directory, while `runtime.lock` is scoped to the
/// whole Rust state root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeLock {
    /// One MCP server process per project (`mcp.lock`).
    Mcp,
    /// One worker process per project (`worker.lock`).
    Worker,
    /// One start/status/stop operation per state root (`runtime.lock`).
    Runtime,
}

impl RuntimeLock {
    /// Every lock kind.
    pub const ALL: [Self; 3] = [Self::Mcp, Self::Worker, Self::Runtime];

    /// The lock file stem (`mcp`, `worker`, `runtime`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Worker => "worker",
            Self::Runtime => "runtime",
        }
    }

    /// Whether the lock is scoped to the state root instead of a project.
    #[must_use]
    pub const fn is_root_scoped(self) -> bool {
        matches!(self, Self::Runtime)
    }
}

/// A Rust PID/ownership process record kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeProcess {
    /// MCP server process record.
    Mcp,
    /// Worker process record.
    Worker,
    /// OpenCode server process record.
    OpencodeServer,
}

impl RuntimeProcess {
    /// Every process record kind.
    pub const ALL: [Self; 3] = [Self::Mcp, Self::Worker, Self::OpencodeServer];

    /// The process record file stem (`mcp`, `worker`, `opencode_server`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Worker => "worker",
            Self::OpencodeServer => "opencode_server",
        }
    }
}

/// A Rust runtime log file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeLog {
    /// MCP server log (`mcp.server.log`).
    McpServer,
    /// OpenCode server log (`opencode.server.log`).
    OpencodeServer,
    /// Worker log (`worker.log`).
    Worker,
}

impl RuntimeLog {
    /// Every log kind.
    pub const ALL: [Self; 3] = [Self::McpServer, Self::OpencodeServer, Self::Worker];

    /// The log file name, matching the frozen runtime contract.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::McpServer => "mcp.server.log",
            Self::OpencodeServer => "opencode.server.log",
            Self::Worker => "worker.log",
        }
    }
}

/// A typed, safe error raised while building or using a [`RustStateLayout`].
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that never contains project ids, artifact names or machine-specific
/// paths; the rejected value itself is never rendered.
#[derive(Debug)]
#[non_exhaustive]
pub enum StateLayoutError {
    /// The project id is not a single safe path component.
    UnsafeProjectId,
    /// The artifact name is empty, absolute or contains a path separator or a
    /// traversal component.
    InvalidArtifactName,
}

impl fmt::Display for StateLayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeProjectId => f.write_str("project id is not a safe state path component"),
            Self::InvalidArtifactName => f.write_str("artifact name is not a safe file name"),
        }
    }
}

impl Error for StateLayoutError {}

/// A typed, safe error raised while opening or initializing a Rust-owned state.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that never contains project ids, namespaces, marker contents, SQL,
/// secrets or machine-specific paths. The underlying connection, inspection or
/// SQLite error, when present, is reachable only through [`Error::source`].
#[derive(Debug)]
#[non_exhaustive]
pub enum RustStateError {
    /// Legacy writer reservations could not be repaired safely.
    Writer(WriterError),
    /// The ownership/format sidecar marker is absent.
    MissingMarker,
    /// The sidecar marker exists but is not valid JSON or lacks a required
    /// field.
    MalformedMarker,
    /// The sidecar marker is owned by a different implementation.
    ForeignImplementation,
    /// The sidecar marker's `format_version` is not supported.
    UnsupportedFormatVersion { found: u64 },
    /// The sidecar marker's project namespace differs from this layout.
    NamespaceMismatch,
    /// The Rust state database file is missing.
    MissingDatabase,
    /// An existing database carries no Rust ownership marker and is therefore
    /// never adopted or initialized.
    UnmarkedState,
    /// `meta.runtime_owner` is absent from an otherwise compatible database.
    MissingRuntimeOwner,
    /// `meta.runtime_owner` is present but is not `rust`.
    ForeignRuntimeOwner,
    /// An existing database is not compatible with its supported contract.
    IncompatibleSchema(InspectError),
    /// An existing database has an unsupported `PRAGMA user_version`.
    UnsupportedSchemaVersion { found: i64 },
    /// The schema v6 DDL could not be applied to a new state.
    Initialize(InitializeError),
    /// The runtime connection could not be opened or configured.
    Connect(ConnectError),
    /// The sidecar marker could not be read or written.
    MarkerIo,
    /// An unexpected SQLite failure.
    Database(rusqlite::Error),
}

impl fmt::Display for RustStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Writer(_) => f.write_str("rust state writer repair failed"),
            Self::MissingMarker => f.write_str("rust state ownership marker is missing"),
            Self::MalformedMarker => f.write_str("rust state ownership marker is malformed"),
            Self::ForeignImplementation => {
                f.write_str("state is owned by a different implementation")
            }
            Self::UnsupportedFormatVersion { found } => {
                write!(f, "unsupported state format version {found}")
            }
            Self::NamespaceMismatch => {
                f.write_str("state belongs to a different project namespace")
            }
            Self::MissingDatabase => f.write_str("rust state database is missing"),
            Self::UnmarkedState => f.write_str("existing state is not marked as rust-owned"),
            Self::MissingRuntimeOwner => f.write_str("database is not marked as rust-owned"),
            Self::ForeignRuntimeOwner => f.write_str("database is owned by a different runtime"),
            Self::IncompatibleSchema(_) => {
                f.write_str("database schema is not compatible with schema v6")
            }
            Self::UnsupportedSchemaVersion { found } => {
                write!(f, "unsupported schema version {found}")
            }
            Self::Initialize(_) => f.write_str("rust state could not be initialized"),
            Self::Connect(_) => f.write_str("storage connection could not be opened"),
            Self::MarkerIo => f.write_str("state ownership marker could not be accessed"),
            Self::Database(_) => f.write_str("storage database error"),
        }
    }
}

impl Error for RustStateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Writer(error) => Some(error),
            Self::IncompatibleSchema(error) => Some(error),
            Self::Initialize(error) => Some(error),
            Self::Connect(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// The typed storage-level path/namespace contract of one Rust-owned project
/// state.
///
/// A layout is created only from an explicitly passed Rust state root and a
/// [`ProjectId`], and every derived runtime artifact lives under that root.
/// Rust therefore cannot share a `state.sqlite`, lock, PID/ownership record,
/// log, token file or endpoint record with a Python state root.
///
/// The layout is purely declarative: it creates no files and never opens a
/// database. It has no API that accepts, copies or imports a Python SQLite
/// database or history. [`RustStateLayout::initialize`] creates the Rust-owned
/// empty schema v17 database at [`RustStateLayout::database`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustStateLayout {
    root: PathBuf,
    project_id: ProjectId,
}

impl RustStateLayout {
    /// The Rust-owned SQLite file name inside the project state directory.
    pub const DATABASE_FILE: &'static str = "state.sqlite";
    /// The Rust-owned ownership/format sidecar marker file name inside the
    /// project state directory.
    pub const MARKER_FILE: &'static str = ".agent-bridge-state.json";
    /// The Rust-owned token/secret namespace directory inside the state root.
    pub const SECRETS_DIR: &'static str = "secrets";
    /// The Rust-owned endpoint namespace directory inside the state root.
    pub const ENDPOINTS_DIR: &'static str = "endpoints";
    /// The lock file suffix.
    pub const LOCK_SUFFIX: &'static str = ".lock";
    /// The PID/ownership process record suffix.
    pub const PROCESS_SUFFIX: &'static str = ".process.json";

    /// Builds the layout of `project_id` under the explicit Rust `root`.
    ///
    /// `root` is the Rust state root (for example
    /// `$XDG_STATE_HOME/agent-bridge-rs`). `project_id` must be a single safe
    /// path component, so the derived project directory can never escape
    /// `root`.
    ///
    /// # Errors
    ///
    /// Returns [`StateLayoutError::UnsafeProjectId`] when `project_id` is not a
    /// single safe path component. No error message contains the id or a path.
    pub fn new(root: impl Into<PathBuf>, project_id: ProjectId) -> Result<Self, StateLayoutError> {
        if !is_safe_component(project_id.as_str()) {
            return Err(StateLayoutError::UnsafeProjectId);
        }
        Ok(Self {
            root: root.into(),
            project_id,
        })
    }

    /// The explicit Rust state root.
    #[must_use]
    pub fn state_root(&self) -> &Path {
        &self.root
    }

    /// The owning project id.
    #[must_use]
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// The project state directory `<root>/<project_id>`.
    #[must_use]
    pub fn project_dir(&self) -> PathBuf {
        self.root.join(self.project_id.as_str())
    }

    /// The Rust-owned SQLite database `<root>/<project_id>/state.sqlite`.
    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.project_dir().join(Self::DATABASE_FILE)
    }

    /// The state-root-scoped runtime manager lock `<root>/runtime.lock`.
    #[must_use]
    pub fn runtime_lock(&self) -> PathBuf {
        self.lock(RuntimeLock::Runtime)
    }

    /// The lock file of `kind`.
    ///
    /// Project-scoped locks live in the project state directory; the runtime
    /// manager lock lives directly in the state root.
    #[must_use]
    pub fn lock(&self, kind: RuntimeLock) -> PathBuf {
        let directory = if kind.is_root_scoped() {
            self.root.clone()
        } else {
            self.project_dir()
        };
        directory.join(format!("{}{}", kind.as_str(), Self::LOCK_SUFFIX))
    }

    /// The PID/ownership process record `<project_dir>/<kind>.process.json`.
    #[must_use]
    pub fn ownership_record(&self, kind: RuntimeProcess) -> PathBuf {
        self.project_dir()
            .join(format!("{}{}", kind.as_str(), Self::PROCESS_SUFFIX))
    }

    /// The log file `<project_dir>/<file_name>`.
    #[must_use]
    pub fn log(&self, kind: RuntimeLog) -> PathBuf {
        self.project_dir().join(kind.file_name())
    }

    /// The Rust-owned token file `<root>/secrets/<name>`.
    ///
    /// # Errors
    ///
    /// Returns [`StateLayoutError::InvalidArtifactName`] when `name` is not a
    /// single safe file name. No error message contains the name or a path.
    pub fn token_file(&self, name: &str) -> Result<PathBuf, StateLayoutError> {
        let name = checked_artifact_name(name)?;
        Ok(self.root.join(Self::SECRETS_DIR).join(name))
    }

    /// The Rust-owned endpoint record `<root>/endpoints/<name>`.
    ///
    /// # Errors
    ///
    /// Returns [`StateLayoutError::InvalidArtifactName`] when `name` is not a
    /// single safe file name. No error message contains the name or a path.
    pub fn endpoint_record(&self, name: &str) -> Result<PathBuf, StateLayoutError> {
        let name = checked_artifact_name(name)?;
        Ok(self.root.join(Self::ENDPOINTS_DIR).join(name))
    }

    /// The Rust-owned ownership/format sidecar marker
    /// `<project_dir>/.agent-bridge-state.json`.
    #[must_use]
    pub fn marker(&self) -> PathBuf {
        self.project_dir().join(Self::MARKER_FILE)
    }

    /// Creates Rust-owned schema v17 or atomically upgrades owned legacy v6/v11/v14/v15/v16.
    ///
    /// A new, isolated Rust state is initialized in a crash-safe order: the
    /// sidecar marker is written first (crash-durably, through a private
    /// temporary file, a no-clobber link and a directory sync) and only then
    /// the schema v17 database with the additive `meta.runtime_owner='rust'`
    /// row. An interrupted initialization is therefore always detectable: the
    /// marker without a database is completed on the next call, while a
    /// database without a marker is never adopted. A concurrent initializer
    /// that publishes the identical marker first is accepted rather than
    /// overwritten.
    ///
    /// Initialization is idempotent. An existing Rust-owned state (a valid
    /// marker *and* a schema v17 database with `meta.runtime_owner='rust'`) is
    /// validated and left unchanged. Owned legacy v6/v11/v14 state is validated before
    /// any writable connection, then rechecked and upgraded inside one
    /// transaction, preserving every existing row. A missing or truly empty
    /// database is created; foreign, unmarked and unsupported state fails closed.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RustStateError`]). An unmarked existing
    /// database, a foreign or mismatched sidecar marker, a foreign or missing
    /// `meta.runtime_owner`, an unsupported `format_version` and an
    /// incompatible schema are all rejected before any write. No error message
    /// contains row data, ids, marker contents, SQL, secrets or paths.
    pub fn initialize(&self) -> Result<(), RustStateError> {
        let marker = self.marker();
        let database = self.database();

        match read_owned_marker(&marker, &self.project_id, &self.root) {
            MarkerState::Owned => initialize_owned_state(&database),
            MarkerState::Absent => {
                if let Err(error) = require_adoptable_database(&database) {
                    // A concurrent initializer may have published the marker
                    // and populated the database after our first read. Re-read
                    // once and adopt it only when it is now our own state;
                    // otherwise fail closed with the original error.
                    if matches!(
                        read_owned_marker(&marker, &self.project_id, &self.root),
                        MarkerState::Owned
                    ) {
                        return initialize_owned_state(&database);
                    }
                    return Err(error);
                }
                // The marker is deliberately left in place if the database
                // creation then fails: a marker without a database is the
                // documented, recoverable interrupted-initialization state, and
                // never removing it here means a concurrent initializer's
                // adopted marker can never be deleted out from under it.
                write_owned_marker(&marker, &self.project_id, &self.root)?;
                initialize_rust_database(&database)
            }
            MarkerState::Invalid(error) => Err(error),
        }
    }

    /// Opens the Rust-owned state writable after the full fail-closed guard.
    ///
    /// A writable [`StorageConnection`] is returned only when *all* of the
    /// following agree:
    ///
    /// * the sidecar marker exists, parses and has
    ///   `implementation = "rust"` and a supported `format_version`;
    /// * the marker's project namespace equals this layout's project;
    /// * the marker's normalized `state_root` equals this layout's state root,
    ///   so a state copied or moved under another root is rejected;
    /// * the database exists and matches the frozen v6/v11/v14/v15/v16/v17 contract;
    /// * `meta.runtime_owner` is present and exactly `rust`.
    ///
    /// Any missing, malformed, unsupported, foreign or contradictory state
    /// returns a typed [`RustStateError`] *before* the writable connection is
    /// created, so a foreign database, an unmarked database, a Python state or
    /// a partially initialized state is never opened or modified.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RustStateError`]). No error message
    /// contains row data, ids, marker contents, SQL, secrets or paths.
    /// Opens owned current state read-only; never initializes or migrates it.
    pub fn open_readonly(&self) -> Result<StorageConnection, RustStateError> {
        match read_owned_marker(&self.marker(), &self.project_id, &self.root) {
            MarkerState::Owned => {}
            MarkerState::Absent => return Err(RustStateError::MissingMarker),
            MarkerState::Invalid(error) => return Err(error),
        }
        validate_rust_database(&self.database())?;
        let connection = open_read_only_current(&self.database())?;
        Ok(StorageConnection { connection })
    }

    pub fn open(&self) -> Result<StorageConnection, RustStateError> {
        match read_owned_marker(&self.marker(), &self.project_id, &self.root) {
            MarkerState::Owned => {}
            MarkerState::Absent => return Err(RustStateError::MissingMarker),
            MarkerState::Invalid(error) => return Err(error),
        }

        validate_rust_database(&self.database())?;

        let connection =
            open_read_write_existing(&self.database()).map_err(RustStateError::Connect)?;
        configure(&connection).map_err(RustStateError::Connect)?;
        Ok(StorageConnection { connection })
    }
}

/// Whether `name` is a single, safe path component that cannot escape a root
/// directory when joined to it.
fn is_safe_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// Validates an artifact file name, returning it unchanged on success.
fn checked_artifact_name(name: &str) -> Result<&str, StateLayoutError> {
    if is_safe_component(name) {
        Ok(name)
    } else {
        Err(StateLayoutError::InvalidArtifactName)
    }
}

// ---------------------------------------------------------------------------
// Ownership/format marker and fail-closed guard (task 3.11).
//
// Rust-owned state carries two independent ownership markers: a versioned
// sidecar file next to the database and the additive `meta.runtime_owner` row.
// A writable connection is handed out only when both markers, the project
// namespace and the frozen schema v6 contract agree.
// ---------------------------------------------------------------------------

/// The `meta.runtime_owner` value of every Rust-owned state.
pub const RUNTIME_OWNER: &str = "rust";

/// The sidecar marker `implementation` value of Rust-owned state.
const MARKER_IMPLEMENTATION: &str = "rust";

/// The only supported sidecar marker `format_version`.
const MARKER_FORMAT_VERSION: u64 = 1;

/// The outcome of reading a sidecar marker file.
enum MarkerState {
    /// No marker file exists.
    Absent,
    /// A valid Rust marker for this exact project namespace.
    Owned,
    /// The marker exists but must fail closed with this error.
    Invalid(RustStateError),
}

/// The parsed fields of a sidecar marker that the guard depends on.
struct OwnedMarker {
    implementation: String,
    format_version: u64,
    project_id: String,
    state_root: String,
}

/// Reads and validates the sidecar marker at `path` for `project_id`/`root`.
///
/// The marker is only [`MarkerState::Owned`] when it parses, is owned by the
/// Rust implementation, has a supported `format_version` and names exactly this
/// project namespace *and* this state root. Any other state is
/// [`MarkerState::Invalid`] with a safe typed error, so the caller never
/// proceeds on a foreign marker.
fn read_owned_marker(path: &Path, project_id: &ProjectId, root: &Path) -> MarkerState {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return MarkerState::Absent,
        Err(_) => return MarkerState::Invalid(RustStateError::MarkerIo),
    };
    let marker = match parse_owned_marker(&text) {
        Ok(marker) => marker,
        Err(error) => return MarkerState::Invalid(error),
    };
    if marker.implementation != MARKER_IMPLEMENTATION {
        return MarkerState::Invalid(RustStateError::ForeignImplementation);
    }
    if marker.format_version != MARKER_FORMAT_VERSION {
        return MarkerState::Invalid(RustStateError::UnsupportedFormatVersion {
            found: marker.format_version,
        });
    }
    if marker.project_id != project_id.as_str() {
        return MarkerState::Invalid(RustStateError::NamespaceMismatch);
    }
    if marker.state_root != encode_state_root(root) {
        return MarkerState::Invalid(RustStateError::NamespaceMismatch);
    }
    MarkerState::Owned
}

/// Parses a sidecar marker, failing closed on malformed JSON or a missing field.
///
/// Every field, including `state_root`, is required: a marker that omits or
/// mistypes one is [`RustStateError::MalformedMarker`].
fn parse_owned_marker(text: &str) -> Result<OwnedMarker, RustStateError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| RustStateError::MalformedMarker)?;
    let object = value.as_object().ok_or(RustStateError::MalformedMarker)?;
    let implementation = object
        .get("implementation")
        .and_then(serde_json::Value::as_str)
        .ok_or(RustStateError::MalformedMarker)?
        .to_owned();
    let format_version = object
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or(RustStateError::MalformedMarker)?;
    let project_id = object
        .get("project_id")
        .and_then(serde_json::Value::as_str)
        .ok_or(RustStateError::MalformedMarker)?
        .to_owned();
    let state_root = object
        .get("state_root")
        .and_then(serde_json::Value::as_str)
        .ok_or(RustStateError::MalformedMarker)?
        .to_owned();
    Ok(OwnedMarker {
        implementation,
        format_version,
        project_id,
        state_root,
    })
}

/// Lexically normalizes a state root into a stable namespace key.
///
/// `.` components are dropped and redundant separators disappear. A `..`
/// component cancels only a *preceding normal* component; an unmatched leading
/// `..` (one with no normal component left to cancel) is preserved instead of
/// being silently dropped, so `../a` and `../../a` keep distinct keys and never
/// collide with `a`. Nothing touches the filesystem and no symlink is followed.
/// The normalized path is then hex-encoded from its stable raw OS bytes, so the
/// key is a valid UTF-8 JSON string, lossless even for non-UTF-8 paths, and two
/// distinct roots never collide. A copied or moved project directory therefore
/// keeps the key of its original root and fails the guard under another root.
fn encode_state_root(root: &Path) -> String {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in root.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let cancels_normal = matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                );
                if cancels_normal {
                    normalized.pop();
                } else {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    encode_state_root_key(&normalized)
}

/// Hex-encodes a normalized state root into its stable on-disk namespace key.
///
/// The bytes are taken from the documented, stable Unix byte representation
/// ([`std::os::unix::ffi::OsStrExt::as_bytes`]) rather than the unspecified
/// [`std::ffi::OsStr::as_encoded_bytes`] encoding, whose output may change
/// between Rust versions and is only meant for same-version round trips. This
/// makes the persisted `state_root` marker field a stable interchange value for
/// the supported platform (Unix/Linux). Hex encoding keeps the key a valid
/// UTF-8 JSON string and lossless even for non-UTF-8 paths.
#[cfg(unix)]
fn encode_state_root_key(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(hex_digit(byte >> 4));
        encoded.push(hex_digit(byte & 0x0f));
    }
    encoded
}

#[cfg(not(unix))]
compile_error!(
    "bridge-storage persists stable state-root namespace keys via the Unix byte encoding; only Unix/Linux targets are supported"
);

/// A private, uniquely named temporary marker path in `path`'s directory.
///
/// The process id and a monotonic counter make the name unique per attempt, so
/// concurrent initializers never share or clobber a temporary file.
fn marker_temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| OsString::from("marker"));
    name.push(format!(".tmp.{}.{}", std::process::id(), sequence));
    path.with_file_name(name)
}

/// Best-effort `fsync` of a directory so a link/rename is durable.
///
/// Directories cannot be opened for sync on every platform; unsupported
/// platforms are ignored because the marker content itself was already synced.
fn sync_directory(path: &Path) {
    if let Ok(directory) = std::fs::File::open(path) {
        let _ = directory.sync_all();
    }
}

/// Writes the Rust sidecar marker for `project_id` under `root`.
///
/// A concurrent initializer that has already published an identical,
/// compatible marker is accepted rather than treated as a failure.
///
/// The marker is written and synced to a private, uniquely named temporary file
/// (`create_new`, so another attempt's temp file is never clobbered), then
/// published with a no-clobber hard link so an existing marker is never
/// overwritten. The temporary file is always cleaned up and the parent
/// directory is synced where supported, so the published marker is durable and
/// a crash never leaves a truncated marker at the final path.
fn write_owned_marker(
    path: &Path,
    project_id: &ProjectId,
    root: &Path,
) -> Result<(), RustStateError> {
    let parent = path.parent().ok_or(RustStateError::MarkerIo)?;
    std::fs::create_dir_all(parent).map_err(|_| RustStateError::MarkerIo)?;

    let value = serde_json::json!({
        "implementation": MARKER_IMPLEMENTATION,
        "format_version": MARKER_FORMAT_VERSION,
        "project_id": project_id.as_str(),
        "state_root": encode_state_root(root),
    });
    let text = serde_json::to_string(&value).map_err(|_| RustStateError::MarkerIo)?;

    let temporary = marker_temp_path(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| RustStateError::MarkerIo)?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return Err(RustStateError::MarkerIo);
    }

    let published = match std::fs::hard_link(&temporary, path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(RustStateError::MarkerIo);
        }
    };
    let _ = std::fs::remove_file(&temporary);

    if !published {
        return match read_owned_marker(path, project_id, root) {
            MarkerState::Owned => {
                sync_directory(parent);
                Ok(())
            }
            MarkerState::Absent => Err(RustStateError::MarkerIo),
            MarkerState::Invalid(error) => Err(error),
        };
    }

    sync_directory(parent);
    Ok(())
}

/// Rejects an unmarked existing database that must never be adopted.
///
/// A missing database or a truly empty one (`user_version=0` with no user
/// objects) may be initialized; anything else is a foreign or unmarked state.
fn require_adoptable_database(path: &Path) -> Result<(), RustStateError> {
    if !path.exists() {
        return Ok(());
    }
    if is_truly_empty_database(path)? {
        Ok(())
    } else {
        Err(RustStateError::UnmarkedState)
    }
}

/// Initializes or validates a state whose sidecar marker is already owned.
///
/// A populated database is validated read-only before a writable connection.
/// v17 is left unchanged; owned v6/v11/v14/v15/v16 is upgraded transactionally. Missing or
/// truly empty state is created through [`initialize_rust_database`].
fn initialize_owned_state(path: &Path) -> Result<(), RustStateError> {
    if path.exists() && !is_truly_empty_database(path)? {
        validate_rust_database(path)?;
        let connection = open_read_only_current(path)?;
        let version = query_user_version(&connection).map_err(RustStateError::Database)?;
        drop(connection);
        if version != RUST_SCHEMA_VERSION {
            initialize_rust_database(path)
        } else {
            Ok(())
        }
    } else {
        initialize_rust_database(path)
    }
}

/// Whether `path` is a readable SQLite database with no user objects and
/// `PRAGMA user_version=0`.
///
/// A file that cannot be opened as a database is never considered empty.
fn is_truly_empty_database(path: &Path) -> Result<bool, RustStateError> {
    let connection = match open_read_only_current(path) {
        Ok(connection) => connection,
        Err(_) => return Ok(false),
    };
    let user_version = query_user_version(&connection).map_err(RustStateError::Database)?;
    if user_version != 0 {
        return Ok(false);
    }
    let non_empty = has_user_objects(&connection).map_err(RustStateError::Database)?;
    Ok(!non_empty)
}

/// Validates existing state as a Rust-owned supported v6/v11/v14/v15 database.
///
/// The database must exist, match its frozen schema contract and carry
/// `meta.runtime_owner='rust'`. The check is read-only and never creates or
/// modifies the database.
fn validate_rust_database(path: &Path) -> Result<(), RustStateError> {
    if !path.exists() {
        return Err(RustStateError::MissingDatabase);
    }
    let connection = open_read_only_current(path)?;
    // All guard queries must observe one committed schema. A concurrent
    // initializer can replace historical indexes between separate SELECTs.
    let transaction = connection
        .unchecked_transaction()
        .map_err(RustStateError::Database)?;
    validate_database(&transaction).map_err(RustStateError::IncompatibleSchema)?;
    require_runtime_owner(&transaction)?;
    transaction.commit().map_err(RustStateError::Database)
}

/// Requires the exact `meta.runtime_owner='rust'` row on `connection`.
fn require_runtime_owner(connection: &Connection) -> Result<(), RustStateError> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'runtime_owner'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(RustStateError::Database)?;
    match value.as_deref() {
        Some(RUNTIME_OWNER) => Ok(()),
        Some(_) => Err(RustStateError::ForeignRuntimeOwner),
        None => Err(RustStateError::MissingRuntimeOwner),
    }
}

/// Creates Rust-owned v17 or upgrades validated Rust-owned v6/v11/v14/v15/v16.
///
/// An already compatible owned v17 database is accepted unchanged; foreign or
/// incompatible state fails closed. Creation and additive upgrade both run
/// inside one `BEGIN IMMEDIATE` transaction, so a partial schema or a database
/// without `meta.runtime_owner` is never committed.
fn initialize_rust_database(path: &Path) -> Result<(), RustStateError> {
    let mut storage = connect(path).map_err(RustStateError::Connect)?;
    let connection = storage.connection_mut();

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(RustStateError::Database)?;

    let user_version = query_user_version(&transaction).map_err(RustStateError::Database)?;
    let non_empty = has_user_objects(&transaction).map_err(RustStateError::Database)?;

    match user_version {
        RUST_SCHEMA_VERSION => {
            validate_database(&transaction).map_err(RustStateError::IncompatibleSchema)?;
            require_runtime_owner(&transaction)?;
        }
        SCHEMA_VERSION => {
            validate_database(&transaction).map_err(RustStateError::IncompatibleSchema)?;
            require_runtime_owner(&transaction)?;
            schema_v15::upgrade(&transaction).map_err(RustStateError::Database)?;
            writers::reconcile_legacy(&transaction).map_err(RustStateError::Writer)?;
            validate_database(&transaction).map_err(RustStateError::IncompatibleSchema)?;
        }
        15 | 16 => {
            validate_database(&transaction).map_err(RustStateError::IncompatibleSchema)?;
            require_runtime_owner(&transaction)?;
        }
        11 | 14 => {
            validate_database(&transaction).map_err(RustStateError::IncompatibleSchema)?;
            require_runtime_owner(&transaction)?;
            schema_v15::upgrade_intermediate(&transaction, user_version)
                .map_err(RustStateError::Database)?;
            writers::reconcile_legacy(&transaction).map_err(RustStateError::Writer)?;
            validate_database(&transaction).map_err(RustStateError::IncompatibleSchema)?;
        }
        0 if !non_empty => {
            apply_schema_v6(&transaction, V6_SCHEMA_DDL).map_err(RustStateError::Initialize)?;
            schema_v15::upgrade(&transaction).map_err(RustStateError::Database)?;
            transaction
                .execute(
                    "INSERT INTO meta (key, value) VALUES ('runtime_owner', ?1)",
                    params![RUNTIME_OWNER],
                )
                .map_err(RustStateError::Database)?;
            validate_database(&transaction).map_err(RustStateError::IncompatibleSchema)?;
        }
        0 => return Err(RustStateError::UnmarkedState),
        found => return Err(RustStateError::UnsupportedSchemaVersion { found }),
    }

    if query_user_version(&transaction).map_err(RustStateError::Database)? == 15 {
        transaction.execute_batch("ALTER TABLE tasks ADD COLUMN delivery_mode TEXT NOT NULL DEFAULT 'manual'; PRAGMA user_version=16; UPDATE meta SET value='16' WHERE key='schema_version';")
            .map_err(RustStateError::Database)?;
        validate_database(&transaction).map_err(RustStateError::IncompatibleSchema)?;
    }
    if query_user_version(&transaction).map_err(RustStateError::Database)? == 16 {
        transaction.execute_batch("CREATE TABLE automation_runs (run_id TEXT PRIMARY KEY, status TEXT NOT NULL, control TEXT NOT NULL DEFAULT 'run', document TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL); CREATE UNIQUE INDEX ux_automation_unfinished ON automation_runs((1)) WHERE status NOT IN ('completed','ready','stopped'); PRAGMA user_version=17; UPDATE meta SET value='17' WHERE key='schema_version';")
            .map_err(RustStateError::Database)?;
        validate_database(&transaction).map_err(RustStateError::IncompatibleSchema)?;
    }
    transaction.commit().map_err(RustStateError::Database)?;
    Ok(())
}

/// Opens an existing database read-write without creating it.
///
/// Unlike [`open_read_write`], the missing-file case is a typed error rather
/// than a silent creation, which keeps [`RustStateLayout::open`] fail closed.
fn open_read_write_existing(path: &Path) -> Result<Connection, ConnectError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(path, flags).map_err(classify_open_error)
}

/// Opens an existing database read-only using the current `mode=ro` URI.
///
/// The shared [`open_read_only`] uses `immutable=1`; the ownership guard needs
/// the *current* committed content (including a live WAL) so a marker written
/// by another connection is observed. No file is created and no sidecar is
/// written by a read-only open.
fn open_read_only_current(path: &Path) -> Result<Connection, RustStateError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(read_only_current_uri(path), flags)
        .map_err(classify_error)
        .map_err(RustStateError::IncompatibleSchema)
}

/// A typed task view of the historical columns and optional modern budget.
///
/// All fifteen columns are represented. Domain-typed columns use [`TaskId`],
/// [`ProjectId`] and [`TaskStatus`]; `allowed_paths` and `test_commands` are
/// decoded JSON arrays of strings; `snapshot` is an optional decoded JSON
/// object (a stored JSON `null` maps to `None`). Opaque identifiers and
/// timestamps stay as the exact stored strings.
///
/// # Mapping contract
///
/// [`Task::from_row`] is the only constructor. It fails closed on an unknown
/// status, an invalid task or project id, malformed or wrong-shaped JSON, a
/// SQLite type mismatch and a negative `revision_count`. The synthetic
/// `task-<n>` identifiers used by the committed SQLite fixtures are not valid
/// UUIDs and are therefore rejected by design; the fixtures themselves are
/// never changed or weakened.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    /// Primary key (`task_id`), parsed as a UUID.
    pub task_id: TaskId,
    /// Owning project (`project_id`).
    pub project_id: ProjectId,
    /// Workspace path (`workspace`).
    pub workspace: String,
    /// Lifecycle status (`status`).
    pub status: TaskStatus,
    /// Bound OpenCode session, when resolved (`session_id`).
    pub session_id: Option<String>,
    /// Task text (`task`).
    pub text: String,
    /// Allowed workspace-relative paths (`allowed_paths`, JSON array of
    /// strings).
    pub allowed_paths: Vec<String>,
    /// Verification commands (`test_commands`, JSON array of strings).
    pub test_commands: Vec<String>,
    /// Creation timestamp (`created_at`).
    pub created_at: String,
    /// Last update timestamp (`updated_at`).
    pub updated_at: String,
    /// Base Git head, when captured (`base_head`).
    pub base_head: Option<String>,
    /// Decoded workspace snapshot (`snapshot`), when present: a JSON object or
    /// `None` for SQL `NULL` or a stored JSON `null`.
    pub snapshot: Option<serde_json::Value>,
    /// Number of revision rounds (`revision_count`), never negative.
    pub revision_count: i64,
    /// Time a cooperative close was requested (`close_requested_at`).
    pub close_requested_at: Option<String>,
    /// Reason for a cooperative close (`close_reason`).
    pub close_reason: Option<String>,
    /// Effective saved delivery policy; missing/corrupt values default to manual.
    pub delivery_mode: bridge_domain::DeliveryMode,
    /// Normalized optional budget. Corrupt non-NULL data fails row mapping.
    pub budget: Option<TaskBudget>,
}

impl Task {
    /// Maps one `tasks` row into a [`Task`].
    ///
    /// The row must expose the fifteen schema v6 columns by name; a missing
    /// required column is reported as [`TaskRowError::MissingColumn`]. When
    /// `budget_json` is present it must be SQL NULL or a valid budget object.
    /// Historical v6 rows have no budget column and map to no budget. Production
    /// queries select all task columns so modern budget validation cannot be skipped.
    ///
    /// # Errors
    ///
    /// Returns a typed category for every kind of corrupted persisted data
    /// (see [`TaskRowError`]). No error message contains row data, task text,
    /// workspace, paths, identifiers or JSON payloads.
    pub fn from_row(row: &Row<'_>) -> Result<Self, TaskRowError> {
        Self::from_row_inner(row, false)
    }
    /// Read-only diagnostic mapping. Invalid budget is surfaced by the separate
    /// budget decision; all other identity and row validation remains strict.
    pub fn from_row_for_status(row: &Row<'_>) -> Result<Self, TaskRowError> {
        Self::from_row_inner(row, true)
    }
    fn from_row_inner(row: &Row<'_>, diagnostic: bool) -> Result<Self, TaskRowError> {
        let task_id = TaskId::from_str(&read_typed::<String>(row, "task_id")?)
            .map_err(|_| TaskRowError::InvalidTaskId)?;
        let project_id = ProjectId::from_str(&read_typed::<String>(row, "project_id")?)
            .map_err(|_| TaskRowError::InvalidProjectId)?;
        let workspace = read_typed::<String>(row, "workspace")?;
        let status = TaskStatus::from_str(&read_typed::<String>(row, "status")?)
            .map_err(|_| TaskRowError::UnknownStatus)?;
        let session_id = read_typed::<Option<String>>(row, "session_id")?;
        let text = read_typed::<String>(row, "task")?;
        let allowed_paths = read_string_array(row, "allowed_paths")?;
        let test_commands = read_string_array(row, "test_commands")?;
        let created_at = read_typed::<String>(row, "created_at")?;
        let updated_at = read_typed::<String>(row, "updated_at")?;
        let base_head = read_typed::<Option<String>>(row, "base_head")?;
        let snapshot = read_snapshot(row)?;
        let revision_count = read_typed::<i64>(row, "revision_count")?;
        if revision_count < 0 {
            return Err(TaskRowError::NegativeRevisionCount);
        }
        let close_requested_at = read_typed::<Option<String>>(row, "close_requested_at")?;
        let close_reason = read_typed::<Option<String>>(row, "close_reason")?;
        // v6 genuinely has no budget column. Modern production reads SELECT *.
        let budget = match row.as_ref().column_index("budget_json") {
            Ok(_) => {
                let parsed = read_typed::<Option<String>>(row, "budget_json").and_then(|raw| {
                    normalize_persisted_budget(raw.as_deref())
                        .map_err(|_| TaskRowError::InvalidBudget)
                });
                match parsed {
                    Ok(budget) => budget,
                    Err(_) if diagnostic => None,
                    Err(error) => return Err(error),
                }
            }
            Err(rusqlite::Error::InvalidColumnName(_)) => None,
            Err(error) => return Err(TaskRowError::Database(error)),
        };

        let delivery_mode = match row.get_ref("delivery_mode") {
            Ok(rusqlite::types::ValueRef::Text(b"on_accept")) => {
                bridge_domain::DeliveryMode::OnAccept
            }
            _ => bridge_domain::DeliveryMode::Manual,
        };

        Ok(Self {
            task_id,
            project_id,
            workspace,
            status,
            session_id,
            text,
            allowed_paths,
            test_commands,
            created_at,
            updated_at,
            base_head,
            snapshot,
            revision_count,
            close_requested_at,
            close_reason,
            delivery_mode,
            budget,
        })
    }
}

/// A typed, safe error raised while mapping a `tasks` row.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that names only the schema column at fault. It never contains row
/// data: task text, workspace, paths, identifiers, JSON payloads or timestamps
/// are never rendered. The underlying SQLite error, when present, is reachable
/// only through [`Error::source`].
#[derive(Debug)]
#[non_exhaustive]
pub enum TaskRowError {
    /// A required column is absent from the mapped row.
    MissingColumn { column: &'static str },
    /// A column holds a SQLite type that does not match the schema v6 contract.
    ColumnType { column: &'static str },
    /// `task_id` is not a valid UUID.
    InvalidTaskId,
    /// `project_id` is not a valid project id.
    InvalidProjectId,
    /// `status` is not a known task status.
    UnknownStatus,
    /// A JSON column is not valid JSON.
    MalformedJson { column: &'static str },
    /// A JSON column does not have the contract shape.
    WrongJsonShape { column: &'static str },
    /// `revision_count` is negative.
    NegativeRevisionCount,
    /// Non-NULL budget JSON is malformed or violates the public contract.
    InvalidBudget,
    /// An unexpected SQLite failure.
    Database(rusqlite::Error),
}

impl fmt::Display for TaskRowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingColumn { column } => {
                write!(f, "task row is missing column {column}")
            }
            Self::ColumnType { column } => {
                write!(f, "task row column {column} has an unexpected SQLite type")
            }
            Self::InvalidTaskId => f.write_str("task row has an invalid task id"),
            Self::InvalidProjectId => f.write_str("task row has an invalid project id"),
            Self::UnknownStatus => f.write_str("task row has an unknown status"),
            Self::MalformedJson { column } => {
                write!(f, "task row column {column} is not valid JSON")
            }
            Self::WrongJsonShape { column } => {
                write!(f, "task row column {column} has an unexpected JSON shape")
            }
            Self::NegativeRevisionCount => f.write_str("task revision count is negative"),
            Self::InvalidBudget => f.write_str("task row budget is invalid"),
            Self::Database(_) => f.write_str("storage database error"),
        }
    }
}

impl Error for TaskRowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

fn read_typed<T: rusqlite::types::FromSql>(
    row: &Row<'_>,
    column: &'static str,
) -> Result<T, TaskRowError> {
    row.get::<_, T>(column)
        .map_err(|error| classify_row_error(error, column))
}

fn classify_row_error(error: rusqlite::Error, column: &'static str) -> TaskRowError {
    match error {
        rusqlite::Error::InvalidColumnName(_) | rusqlite::Error::InvalidColumnIndex(_) => {
            TaskRowError::MissingColumn { column }
        }
        rusqlite::Error::InvalidColumnType(..) | rusqlite::Error::FromSqlConversionFailure(..) => {
            TaskRowError::ColumnType { column }
        }
        other => TaskRowError::Database(other),
    }
}

fn read_string_array(row: &Row<'_>, column: &'static str) -> Result<Vec<String>, TaskRowError> {
    let raw = read_typed::<String>(row, column)?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| TaskRowError::MalformedJson { column })?;
    let items = value
        .as_array()
        .ok_or(TaskRowError::WrongJsonShape { column })?;
    let mut decoded = Vec::with_capacity(items.len());
    for item in items {
        let item = item
            .as_str()
            .ok_or(TaskRowError::WrongJsonShape { column })?;
        decoded.push(item.to_owned());
    }
    Ok(decoded)
}

fn read_snapshot(row: &Row<'_>) -> Result<Option<serde_json::Value>, TaskRowError> {
    let Some(raw) = read_typed::<Option<String>>(row, "snapshot")? else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|_| TaskRowError::MalformedJson { column: "snapshot" })?;
    if value.is_null() {
        return Ok(None);
    }
    if !value.is_object() {
        return Err(TaskRowError::WrongJsonShape { column: "snapshot" });
    }
    Ok(Some(value))
}

/// A complete, typed view of one schema v6 `rounds` row.
///
/// All twenty-one columns are represented. Domain-typed columns use [`TaskId`],
/// [`ProjectId`], [`RoundKind`], [`RoundStatus`] and [`VerifierState`];
/// `result_json` is an optional decoded JSON object (a stored JSON `null` maps
/// to `None`) and `verifier_json` is an optional decoded domain
/// [`Verification`]. Opaque identifiers, message ids, hashes and timestamps stay
/// as the exact stored strings.
///
/// This is a storage-owned, complete row model; it deliberately neither extends
/// nor duplicates the purpose of the minimal [`bridge_domain::Round`] domain
/// view.
///
/// # Mapping contract
///
/// [`RoundRow::from_row`] is the only constructor. It fails closed on an
/// unknown kind, status or verifier state, an invalid task or project id, a
/// `round_number` outside `1..=u32::MAX`, an `attempted` value other than
/// SQLite `0`/`1`, malformed or wrong-shaped JSON, a SQLite type mismatch and an
/// inconsistent `verifier_state`/`verifier_json` pair. The synthetic
/// `task-<n>` identifiers used by the committed SQLite fixtures are not valid
/// UUIDs and are therefore rejected by design; the fixtures themselves are
/// never changed or weakened.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundRow {
    /// Owning task (`task_id`), parsed as a UUID.
    pub task_id: TaskId,
    /// Owning project (`project_id`).
    pub project_id: ProjectId,
    /// One-based round number within the task (`round_number`), `1..=u32::MAX`.
    pub round_number: u32,
    /// Idempotency key that created the round (`request_id`).
    pub request_id: String,
    /// Hash of the request payload (`payload_hash`).
    pub payload_hash: String,
    /// Whether this is the initial or a revision round (`kind`).
    pub kind: RoundKind,
    /// Current lifecycle status (`status`).
    pub status: RoundStatus,
    /// Outbound OpenCode message id, when prepared (`outbound_message_id`).
    pub outbound_message_id: Option<String>,
    /// Whether the prompt was attempted (`attempted`), stored as `0` or `1`.
    pub attempted: bool,
    /// Response OpenCode message id, when resolved (`response_message_id`).
    pub response_message_id: Option<String>,
    /// Assistant response text (`response`).
    pub response: Option<String>,
    /// Machine-readable failure code (`error_code`).
    pub error_code: Option<String>,
    /// Decoded change-collection result (`result_json`): a JSON object, or
    /// `None` for SQL `NULL` or a stored JSON `null`.
    pub result_json: Option<serde_json::Value>,
    /// Revision findings text (`findings`), preserved verbatim.
    pub findings: Option<String>,
    /// Bound OpenCode session, when resolved (`session_id`).
    pub session_id: Option<String>,
    /// Worker start timestamp (`worker_started_at`).
    pub worker_started_at: Option<String>,
    /// Worker deadline timestamp (`worker_deadline_at`).
    pub worker_deadline_at: Option<String>,
    /// Persisted verifier lifecycle marker (`verifier_state`), when a verifier
    /// run exists.
    pub verifier_state: Option<VerifierState>,
    /// Decoded verifier outcome (`verifier_json`), when persisted.
    pub verifier_json: Option<Verification>,
    /// Creation timestamp (`created_at`).
    pub created_at: String,
    /// Last update timestamp (`updated_at`).
    pub updated_at: String,
}

impl RoundRow {
    /// Maps one `rounds` row into a [`RoundRow`].
    ///
    /// The row must expose the twenty-one schema v6 `rounds` columns by name; a
    /// missing column is reported as [`RoundRowError::MissingColumn`].
    ///
    /// # Errors
    ///
    /// Returns a typed category for every kind of corrupted persisted data
    /// (see [`RoundRowError`]). No error message contains row data, response,
    /// findings, identifiers, hashes, paths or JSON payloads.
    pub fn from_row(row: &Row<'_>) -> Result<Self, RoundRowError> {
        let task_id = TaskId::from_str(&read_round_typed::<String>(row, "task_id")?)
            .map_err(|_| RoundRowError::InvalidTaskId)?;
        let project_id = ProjectId::from_str(&read_round_typed::<String>(row, "project_id")?)
            .map_err(|_| RoundRowError::InvalidProjectId)?;
        let round_number = u32::try_from(read_round_typed::<i64>(row, "round_number")?)
            .ok()
            .filter(|value| *value >= 1)
            .ok_or(RoundRowError::InvalidRoundNumber)?;
        let request_id = read_round_typed::<String>(row, "request_id")?;
        let payload_hash = read_round_typed::<String>(row, "payload_hash")?;
        let kind = RoundKind::from_str(&read_round_typed::<String>(row, "kind")?)
            .map_err(|_| RoundRowError::UnknownKind)?;
        let status = RoundStatus::from_str(&read_round_typed::<String>(row, "status")?)
            .map_err(|_| RoundRowError::UnknownStatus)?;
        let outbound_message_id = read_round_typed::<Option<String>>(row, "outbound_message_id")?;
        let attempted = match read_round_typed::<i64>(row, "attempted")? {
            0 => false,
            1 => true,
            _ => return Err(RoundRowError::InvalidAttempted),
        };
        let response_message_id = read_round_typed::<Option<String>>(row, "response_message_id")?;
        let response = read_round_typed::<Option<String>>(row, "response")?;
        let error_code = read_round_typed::<Option<String>>(row, "error_code")?;
        let result_json = read_result_json(row)?;
        let findings = read_round_typed::<Option<String>>(row, "findings")?;
        let session_id = read_round_typed::<Option<String>>(row, "session_id")?;
        let worker_started_at = read_round_typed::<Option<String>>(row, "worker_started_at")?;
        let worker_deadline_at = read_round_typed::<Option<String>>(row, "worker_deadline_at")?;
        let verifier_state = match read_round_typed::<Option<String>>(row, "verifier_state")? {
            Some(raw) => Some(
                VerifierState::from_str(&raw).map_err(|_| RoundRowError::UnknownVerifierState)?,
            ),
            None => None,
        };
        let verifier_json = read_verifier_json(row)?;
        check_verifier_consistency(verifier_state, verifier_json.as_ref())?;
        let created_at = read_round_typed::<String>(row, "created_at")?;
        let updated_at = read_round_typed::<String>(row, "updated_at")?;

        Ok(Self {
            task_id,
            project_id,
            round_number,
            request_id,
            payload_hash,
            kind,
            status,
            outbound_message_id,
            attempted,
            response_message_id,
            response,
            error_code,
            result_json,
            findings,
            session_id,
            worker_started_at,
            worker_deadline_at,
            verifier_state,
            verifier_json,
            created_at,
            updated_at,
        })
    }
}

/// A typed, safe error raised while mapping a `rounds` row.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that names only the schema column at fault. It never contains row
/// data: response, findings, identifiers, hashes, paths, JSON payloads or
/// timestamps are never rendered. The underlying SQLite error, when present, is
/// reachable only through [`Error::source`].
#[derive(Debug)]
#[non_exhaustive]
pub enum RoundRowError {
    /// A required column is absent from the mapped row.
    MissingColumn { column: &'static str },
    /// A column holds a SQLite type that does not match the schema v6 contract.
    ColumnType { column: &'static str },
    /// `task_id` is not a valid UUID.
    InvalidTaskId,
    /// `project_id` is not a valid project id.
    InvalidProjectId,
    /// `kind` is not a known round kind.
    UnknownKind,
    /// `status` is not a known round status.
    UnknownStatus,
    /// `verifier_state` is not a known verifier state.
    UnknownVerifierState,
    /// `round_number` is outside `1..=u32::MAX`.
    InvalidRoundNumber,
    /// `attempted` is a SQLite integer other than `0` or `1`.
    InvalidAttempted,
    /// A JSON column is not valid JSON.
    MalformedJson { column: &'static str },
    /// A JSON column does not have the contract shape.
    WrongJsonShape { column: &'static str },
    /// `verifier_state` and `verifier_json` contradict each other.
    InconsistentVerifier,
    /// An unexpected SQLite failure.
    Database(rusqlite::Error),
}

impl fmt::Display for RoundRowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingColumn { column } => {
                write!(f, "round row is missing column {column}")
            }
            Self::ColumnType { column } => {
                write!(f, "round row column {column} has an unexpected SQLite type")
            }
            Self::InvalidTaskId => f.write_str("round row has an invalid task id"),
            Self::InvalidProjectId => f.write_str("round row has an invalid project id"),
            Self::UnknownKind => f.write_str("round row has an unknown kind"),
            Self::UnknownStatus => f.write_str("round row has an unknown status"),
            Self::UnknownVerifierState => f.write_str("round row has an unknown verifier state"),
            Self::InvalidRoundNumber => f.write_str("round row has an invalid round number"),
            Self::InvalidAttempted => f.write_str("round row has an invalid attempted flag"),
            Self::MalformedJson { column } => {
                write!(f, "round row column {column} is not valid JSON")
            }
            Self::WrongJsonShape { column } => {
                write!(f, "round row column {column} has an unexpected JSON shape")
            }
            Self::InconsistentVerifier => {
                f.write_str("round row has an inconsistent verifier pair")
            }
            Self::Database(_) => f.write_str("storage database error"),
        }
    }
}

impl Error for RoundRowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

fn read_round_typed<T: rusqlite::types::FromSql>(
    row: &Row<'_>,
    column: &'static str,
) -> Result<T, RoundRowError> {
    row.get::<_, T>(column)
        .map_err(|error| classify_round_row_error(error, column))
}

fn classify_round_row_error(error: rusqlite::Error, column: &'static str) -> RoundRowError {
    match error {
        rusqlite::Error::InvalidColumnName(_) | rusqlite::Error::InvalidColumnIndex(_) => {
            RoundRowError::MissingColumn { column }
        }
        rusqlite::Error::InvalidColumnType(..) | rusqlite::Error::FromSqlConversionFailure(..) => {
            RoundRowError::ColumnType { column }
        }
        other => RoundRowError::Database(other),
    }
}

fn read_result_json(row: &Row<'_>) -> Result<Option<serde_json::Value>, RoundRowError> {
    const COLUMN: &str = "result_json";
    let Some(raw) = read_round_typed::<Option<String>>(row, COLUMN)? else {
        return Ok(None);
    };
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| RoundRowError::MalformedJson { column: COLUMN })?;
    if value.is_null() {
        return Ok(None);
    }
    if !value.is_object() {
        return Err(RoundRowError::WrongJsonShape { column: COLUMN });
    }
    Ok(Some(value))
}

fn read_verifier_json(row: &Row<'_>) -> Result<Option<Verification>, RoundRowError> {
    const COLUMN: &str = "verifier_json";
    let Some(raw) = read_round_typed::<Option<String>>(row, COLUMN)? else {
        return Ok(None);
    };
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|_| RoundRowError::MalformedJson { column: COLUMN })?;
    if !value.is_object() {
        return Err(RoundRowError::WrongJsonShape { column: COLUMN });
    }
    if row
        .get::<_, Option<String>>("verifier_state")
        .ok()
        .flatten()
        .as_deref()
        == Some("running")
        && value.get("status").is_none()
    {
        return Ok(None);
    }
    let verification: Verification = serde_json::from_value(value)
        .map_err(|_| RoundRowError::WrongJsonShape { column: COLUMN })?;
    Ok(Some(verification))
}

fn check_verifier_consistency(
    state: Option<VerifierState>,
    verification: Option<&Verification>,
) -> Result<(), RoundRowError> {
    match (state, verification) {
        (Some(VerifierState::Done), Some(_))
        | (Some(VerifierState::Running), None)
        | (None, None) => Ok(()),
        _ => Err(RoundRowError::InconsistentVerifier),
    }
}

#[cfg(test)]
mod tests {
    mod automation;
    mod budgets;
    mod delivery_policy;
    mod dependencies;
    mod mcp_lifecycle;

    mod prune;
    mod recovery;
    mod schema16;
    mod schema17;
    mod verifier_progress;
    mod worktrees;
    mod writer_indexes;
    mod writers;
    use super::{
        BUSY_TIMEOUT_MS, CLOSE_REASON_FALLBACK, CLOSE_REASON_MAX_CHARS, Column,
        CompleteRequestedCloseOutcome, CompleteVerifierInput, ConnectError, Contract,
        CreateRevisionRoundInput, CreateTaskError, CreateTaskInput, CreateTaskOutcome,
        FinishRoundInput, ForeignKey, Index, InitializeError, InspectError, QueryError,
        RECOVERABLE_FAILED_ERROR_CODE, ROUND_TRANSITIONS, RUNTIME_OWNER, ReopenFailedRoundOutcome,
        ReplayStateError, RequestTaskCloseOutcome, RoundRef, RoundRow, RoundRowError,
        RoundUpdateError, RuntimeLock, RuntimeLog, RuntimeProcess, RustStateError, RustStateLayout,
        SCHEMA_VERSION, SchemaMismatch, StateLayoutError, StorageConnection, Table, Task,
        TaskRowError, V6_SCHEMA_DDL, VerifierUpdateOutcome, apply_schema_v6, connect,
        encode_state_root, initialize, inspect, open_read_only, query_user_version,
        round_transition_allowed, v6_contract,
    };
    use bridge_domain::{
        ProjectId, RoundKind, RoundStatus, TaskId, TaskStatus, Verification, VerificationCommand,
        VerificationStatus, VerifierState,
    };
    use rusqlite::types::Value as SqlValue;
    use rusqlite::{Connection, TransactionBehavior};
    use serde_json::Value;
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU64, Ordering};

    const FIXTURES: [&str; 4] = [
        "active-v6.sqlite",
        "awaiting-review-v6.sqlite",
        "empty-v6.sqlite",
        "terminal-v6.sqlite",
    ];

    const FK_CLAUSE: &str = "FOREIGN KEY (task_id) REFERENCES tasks(task_id)";

    /// The schema v6 DDL shared with production initialization: the tests build
    /// their synthetic databases from the very constant [`initialize`] uses.
    const V6_SCHEMA: &str = V6_SCHEMA_DDL;

    fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/fixtures/sqlite")
    }

    fn load_expected() -> Value {
        let text = std::fs::read_to_string(fixture_dir().join("expected.json"))
            .expect("expected.json must be readable");
        serde_json::from_str(&text).expect("expected.json must be valid JSON")
    }

    /// Parses the contract out of `expected.json.schema` and normalizes it.
    fn expected_contract(expected: &Value) -> Contract {
        let schema = &expected["schema"];

        let tables = schema["tables"]
            .as_object()
            .expect("schema.tables must be an object")
            .iter()
            .map(|(name, columns)| {
                let mut parsed: Vec<Column> = columns
                    .as_array()
                    .expect("table columns must be an array")
                    .iter()
                    .map(|column| Column {
                        name: column["name"].as_str().expect("column name").to_owned(),
                        declared_type: column["type"].as_str().expect("column type").to_owned(),
                        not_null: column["notnull"].as_bool().expect("column notnull"),
                        primary_key: column["pk"].as_i64().expect("column pk"),
                    })
                    .collect();
                parsed.sort_by(|left, right| left.name.cmp(&right.name));
                Table {
                    name: name.clone(),
                    columns: parsed,
                }
            })
            .collect();

        let indexes = schema["indexes"]
            .as_object()
            .expect("schema.indexes must be an object")
            .iter()
            .map(|(name, index)| Index {
                name: name.clone(),
                table: index["table"].as_str().expect("index table").to_owned(),
                columns: index["columns"]
                    .as_array()
                    .expect("index columns must be an array")
                    .iter()
                    .map(|column| column.as_str().expect("index column").to_owned())
                    .collect(),
                unique: index["unique"].as_bool().expect("index unique"),
                partial: index["partial"].as_bool().expect("index partial"),
            })
            .collect();

        let foreign_keys = schema["foreign_keys"]
            .as_array()
            .expect("schema.foreign_keys must be an array")
            .iter()
            .map(|key| ForeignKey {
                table: key["table"].as_str().expect("foreign key table").to_owned(),
                from: key["from"].as_str().expect("foreign key from").to_owned(),
                to_table: key["to_table"]
                    .as_str()
                    .expect("foreign key to_table")
                    .to_owned(),
                to_column: key["to_column"]
                    .as_str()
                    .expect("foreign key to_column")
                    .to_owned(),
                on_update: "NO ACTION".to_owned(),
                on_delete: "NO ACTION".to_owned(),
            })
            .collect();

        normalize(Contract {
            tables,
            indexes,
            foreign_keys,
        })
    }

    fn normalize(mut contract: Contract) -> Contract {
        contract
            .tables
            .sort_by(|left, right| left.name.cmp(&right.name));
        for table in &mut contract.tables {
            table
                .columns
                .sort_by(|left, right| left.name.cmp(&right.name));
        }
        contract
            .indexes
            .sort_by(|left, right| left.name.cmp(&right.name));
        contract.foreign_keys.sort_by(|left, right| {
            (left.table.as_str(), left.from.as_str())
                .cmp(&(right.table.as_str(), right.from.as_str()))
        });
        contract
    }

    #[test]
    fn embedded_contract_matches_expected_json() {
        let expected = load_expected();
        let expected = expected_contract(&expected);
        let actual = normalize(v6_contract());
        assert_eq!(actual, expected);
    }

    #[test]
    fn all_schema_v6_fixtures_are_compatible() {
        let expected = load_expected();
        let expected = expected_contract(&expected);
        for name in FIXTURES {
            let inspection = inspect(fixture_dir().join(name))
                .unwrap_or_else(|error| panic!("{name}: {error:?}"));
            assert_eq!(inspection.user_version(), SCHEMA_VERSION, "{name}");
            assert_eq!(inspection.meta_schema_version(), "6", "{name}");
            let observed = normalize(Contract {
                tables: inspection.tables().to_vec(),
                indexes: inspection.indexes().to_vec(),
                foreign_keys: inspection.foreign_keys().to_vec(),
            });
            assert_eq!(
                observed, expected,
                "{name} schema differs from expected.json"
            );
        }
    }

    #[test]
    fn inspection_does_not_modify_fixtures_or_create_sidecars() {
        for name in FIXTURES {
            let path = fixture_dir().join(name);
            let bytes_before = std::fs::read(&path).expect("fixture must be readable");
            let metadata_before = std::fs::metadata(&path).expect("fixture metadata");
            let modified_before = metadata_before.modified().expect("fixture mtime");

            let result = inspect(&path);
            assert!(result.is_ok(), "{name}: {result:?}");

            let bytes_after = std::fs::read(&path).expect("fixture must be readable");
            let metadata_after = std::fs::metadata(&path).expect("fixture metadata");
            assert_eq!(bytes_before, bytes_after, "{name} bytes changed");
            assert_eq!(
                metadata_before.len(),
                metadata_after.len(),
                "{name} length changed"
            );
            assert_eq!(
                modified_before,
                metadata_after.modified().expect("fixture mtime"),
                "{name} mtime changed"
            );
            assert!(
                !sidecar(&path, "-wal").exists(),
                "{name} left a -wal sidecar"
            );
            assert!(
                !sidecar(&path, "-shm").exists(),
                "{name} left a -shm sidecar"
            );
        }
    }

    #[test]
    fn connect_creates_database_and_enables_wal() {
        let dir = TempDir::new("connect-create");
        let path = dir.join("state.sqlite");
        assert!(!path.exists(), "database must not exist before connect");

        let storage = connect(&path).expect("connect must succeed");
        assert!(path.exists(), "connect must create the database file");
        let journal_mode: String = storage
            .connection()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read journal_mode");
        assert_eq!(journal_mode, "wal");
    }

    #[test]
    fn connect_applies_foreign_keys_and_busy_timeout() {
        let dir = TempDir::new("connect-pragmas");
        let path = dir.join("state.sqlite");
        let storage = connect(&path).expect("connect must succeed");

        let foreign_keys: i64 = storage
            .connection()
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("read foreign_keys");
        assert_eq!(foreign_keys, 1);

        let busy_timeout: i64 = storage
            .connection()
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("read busy_timeout");
        assert_eq!(busy_timeout, BUSY_TIMEOUT_MS);
        assert_eq!(busy_timeout, 30_000);
    }

    #[test]
    fn connect_enforces_foreign_keys_behaviourally() {
        let dir = TempDir::new("connect-fk");
        let path = dir.join("state.sqlite");
        {
            let raw = Connection::open(&path).expect("open database for schema");
            raw.execute_batch(
                "CREATE TABLE parent (id INTEGER PRIMARY KEY); \
                 CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL \
                     REFERENCES parent(id));",
            )
            .expect("create schema");
        }

        let storage = connect(&path).expect("connect must succeed");
        let connection = storage.connection();
        connection
            .execute("INSERT INTO parent (id) VALUES (1)", [])
            .expect("valid parent insert");
        connection
            .execute("INSERT INTO child (id, parent_id) VALUES (1, 1)", [])
            .expect("valid child insert");
        let violation = connection.execute("INSERT INTO child (id, parent_id) VALUES (2, 999)", []);
        assert!(violation.is_err(), "foreign key violation must be rejected");
    }

    #[test]
    fn reconnect_preserves_configuration() {
        let dir = TempDir::new("connect-reconnect");
        let path = dir.join("state.sqlite");
        drop(connect(&path).expect("first connect must succeed"));

        let storage = connect(&path).expect("second connect must succeed");
        let journal_mode: String = storage
            .connection()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read journal_mode");
        assert_eq!(journal_mode, "wal");
        let foreign_keys: i64 = storage
            .connection()
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("read foreign_keys");
        assert_eq!(foreign_keys, 1);
        let busy_timeout: i64 = storage
            .connection()
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("read busy_timeout");
        assert_eq!(busy_timeout, BUSY_TIMEOUT_MS);
    }

    #[test]
    fn connect_creates_missing_parent_directory() {
        let dir = TempDir::new("connect-parent");
        let path = dir.join("nested/deep/state.sqlite");
        let parent = path.parent().expect("path has a parent").to_path_buf();
        assert!(!parent.exists(), "parent must not exist before connect");

        drop(connect(&path).expect("connect must create the parent directory"));
        assert!(parent.is_dir(), "parent directory must be created");
        assert!(path.exists(), "database must be created");
    }

    #[test]
    fn connect_does_not_change_user_version() {
        let dir = TempDir::new("connect-user-version");

        let fresh = dir.join("fresh.sqlite");
        {
            let storage = connect(&fresh).expect("connect fresh database");
            let user_version: i64 = storage
                .connection()
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .expect("read user_version");
            assert_eq!(user_version, 0);
        }

        let existing = dir.join("existing.sqlite");
        create_v6(&existing);
        {
            let storage = connect(&existing).expect("connect existing database");
            let user_version: i64 = storage
                .connection()
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .expect("read user_version");
            assert_eq!(user_version, SCHEMA_VERSION);
        }
    }

    #[test]
    fn unusable_parent_is_reported_without_leaking_the_path() {
        let dir = TempDir::new("connect-unusable-parent");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("write blocker file");
        let path = dir.join("blocker/state.sqlite");

        let error = connect(&path).expect_err("unusable parent must be rejected");
        assert!(matches!(error, ConnectError::CreateDirectory), "{error:?}");
        let message = error.to_string();
        assert!(!message.contains("blocker"), "path leaked: {message}");
        assert!(
            !message.contains(&path.to_string_lossy().into_owned()),
            "path leaked: {message}"
        );
    }

    #[test]
    fn directory_path_is_reported_without_leaking_the_path() {
        let dir = TempDir::new("connect-directory");
        let path = dir.join("state.sqlite");
        std::fs::create_dir(&path).expect("create directory at database path");

        let error = connect(&path).expect_err("directory path must be rejected");
        assert!(matches!(error, ConnectError::NotUsable), "{error:?}");
        let message = error.to_string();
        assert!(
            !message.contains(&path.to_string_lossy().into_owned()),
            "path leaked: {message}"
        );
    }

    #[test]
    fn non_database_file_is_reported_without_leaking_the_path() {
        let dir = TempDir::new("connect-non-database");
        let path = dir.join("state.sqlite");
        std::fs::write(&path, b"this is not a sqlite database").expect("write junk");

        let error = connect(&path).expect_err("non-database file must be rejected");
        assert!(matches!(error, ConnectError::NotADatabase), "{error:?}");
        let message = error.to_string();
        assert!(
            !message.contains(&path.to_string_lossy().into_owned()),
            "path leaked: {message}"
        );
    }

    #[test]
    fn connect_on_fixture_copy_leaves_committed_fixture_untouched() {
        let source = fixture_dir().join("active-v6.sqlite");
        let before = std::fs::read(&source).expect("read fixture");
        let dir = TempDir::new("connect-fixture-copy");
        let copy = dir.join("state.sqlite");
        std::fs::copy(&source, &copy).expect("copy fixture");

        {
            let storage = connect(&copy).expect("connect fixture copy");
            let journal_mode: String = storage
                .connection()
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .expect("read journal_mode");
            assert_eq!(journal_mode, "wal");
        }

        assert_eq!(std::fs::read(&source).expect("re-read fixture"), before);
        assert!(
            !sidecar(&source, "-wal").exists(),
            "committed fixture got a -wal sidecar"
        );
        assert!(
            !sidecar(&source, "-shm").exists(),
            "committed fixture got a -shm sidecar"
        );
    }

    /// Snapshots the logical state that a fail-closed initialization must never
    /// change: the `PRAGMA user_version` and every user object.
    fn logical_state(path: &Path) -> (i64, Vec<(String, String)>) {
        let connection = Connection::open(path).expect("open database for state snapshot");
        let user_version = query_user_version(&connection).expect("read user_version for snapshot");
        let mut statement = connection
            .prepare(
                "SELECT type, name FROM sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .expect("prepare user object query");
        let objects = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query user objects")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect user objects");
        (user_version, objects)
    }

    /// Asserts that `path` matches the frozen schema v6 contract exactly.
    fn assert_compatible_v6(path: &Path) {
        let inspection = inspect(path).expect("initialized database must be inspectable");
        assert_eq!(inspection.user_version(), SCHEMA_VERSION);
        assert_eq!(inspection.meta_schema_version(), "6");
        let observed = normalize(Contract {
            tables: inspection.tables().to_vec(),
            indexes: inspection.indexes().to_vec(),
            foreign_keys: inspection.foreign_keys().to_vec(),
        });
        let expected = expected_contract(&load_expected());
        assert_eq!(
            observed, expected,
            "initialized schema differs from expected.json"
        );
    }

    /// Asserts that `path` is a compatible v6 database with no user rows.
    fn assert_compatible_empty_v6(path: &Path) {
        assert_compatible_v6(path);
        let connection = Connection::open(path).expect("open initialized database");
        for table in ["tasks", "rounds", "events"] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count rows");
            assert_eq!(count, 0, "new database must be empty of {table} rows");
        }
    }

    #[test]
    fn initialize_missing_file_creates_compatible_empty_schema() {
        let dir = TempDir::new("initialize-missing");
        let path = dir.join("state.sqlite");
        assert!(!path.exists(), "database must not exist before initialize");

        initialize(&path).expect("missing file must be initialized");
        assert!(path.exists(), "initialize must create the database file");
        assert_compatible_empty_v6(&path);
    }

    #[test]
    fn initialize_truly_empty_file_creates_compatible_empty_schema() {
        let dir = TempDir::new("initialize-empty-file");
        let path = dir.join("state.sqlite");
        std::fs::write(&path, b"").expect("write empty file");

        initialize(&path).expect("empty file must be initialized");
        assert_compatible_empty_v6(&path);
    }

    #[test]
    fn initialize_is_idempotent_and_preserves_rows() {
        let dir = TempDir::new("initialize-idempotent");
        let path = dir.join("state.sqlite");
        initialize(&path).expect("first initialize must succeed");
        {
            let connection = Connection::open(&path).expect("open initialized database");
            connection
                .execute(
                    "INSERT INTO tasks (task_id, project_id, workspace, status, task, \
                     allowed_paths, test_commands, created_at, updated_at, revision_count) \
                     VALUES (?1, 'proj', '/fixture/workspace', 'implementing', 't', '[]', '[]', \
                     '2026-01-01T00:00:00.000+00:00', '2026-01-01T00:00:00.000+00:00', 0)",
                    rusqlite::params![VALID_TASK_ID],
                )
                .expect("insert task row");
        }

        initialize(&path).expect("second initialize must be an idempotent no-op");

        let connection = Connection::open(&path).expect("open after second initialize");
        let (task_id, project_id): (String, String) = connection
            .query_row("SELECT task_id, project_id FROM tasks", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("task row must be preserved");
        assert_eq!(task_id, VALID_TASK_ID);
        assert_eq!(project_id, "proj");
        assert_eq!(
            query_user_version(&connection).expect("read user_version"),
            SCHEMA_VERSION
        );
        drop(connection);
        assert_compatible_v6(&path);
    }

    #[test]
    fn failed_schema_creation_rolls_back_every_change() {
        let dir = TempDir::new("initialize-rollback");
        let path = dir.join("state.sqlite");
        let mut storage = connect(&path).expect("connect must succeed");
        {
            let connection = storage.connection_mut();
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("begin immediate");
            let bad_ddl = "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL); \
                           CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);";
            let error =
                apply_schema_v6(&transaction, bad_ddl).expect_err("duplicate table must fail");
            assert!(matches!(error, InitializeError::Database(_)), "{error:?}");
        }

        let connection = storage.connection();
        assert_eq!(
            query_user_version(connection).expect("read user_version"),
            0,
            "rolled back initialization must keep user_version=0"
        );
        let objects: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .expect("count user objects");
        assert_eq!(objects, 0, "partial schema survived the rollback");
    }

    #[test]
    fn concurrent_initialize_serializes_to_one_schema() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("initialize-concurrent");
        let path = dir.join("state.sqlite");
        // Pre-create the database in WAL mode so the threads contend only on the
        // writer transaction, not on the journal-mode switch.
        drop(connect(&path).expect("pre-create database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                initialize(path.as_path())
            }));
        }
        for handle in handles {
            handle
                .join()
                .expect("initialize thread must not panic")
                .expect("concurrent initialize must succeed");
        }

        assert_compatible_empty_v6(&path);
    }

    #[test]
    fn initialize_on_fixture_copy_is_noop_and_leaves_fixture_untouched() {
        let source = fixture_dir().join("active-v6.sqlite");
        let before = std::fs::read(&source).expect("read fixture");
        let dir = TempDir::new("initialize-fixture-copy");
        let copy = dir.join("state.sqlite");
        std::fs::copy(&source, &copy).expect("copy fixture");

        initialize(&copy).expect("initializing a v6 copy must be a no-op");
        let inspection = inspect(&copy).expect("copy must stay compatible");
        assert_eq!(inspection.user_version(), SCHEMA_VERSION);
        assert_eq!(inspection.meta_schema_version(), "6");

        assert_eq!(std::fs::read(&source).expect("re-read fixture"), before);
        assert!(
            !sidecar(&source, "-wal").exists(),
            "committed fixture got a -wal sidecar"
        );
        assert!(
            !sidecar(&source, "-shm").exists(),
            "committed fixture got a -shm sidecar"
        );
    }

    #[test]
    fn initialize_errors_do_not_leak_the_path() {
        let dir = TempDir::new("initialize-path-leak");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("write blocker file");
        let path = dir.join("blocker/state.sqlite");

        let error = initialize(&path).expect_err("unusable parent must be rejected");
        assert!(
            matches!(
                error,
                InitializeError::Connect(ConnectError::CreateDirectory)
            ),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(!message.contains("blocker"), "path leaked: {message}");
        assert!(
            !message.contains(&path.to_string_lossy().into_owned()),
            "path leaked: {message}"
        );
    }

    #[test]
    fn initialize_rejects_unknown_user_version_without_changes() {
        let dir = TempDir::new("initialize-version5");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        execute(&path, "PRAGMA user_version = 5");
        let before = logical_state(&path);

        let error = initialize(&path).expect_err("version 5 must be rejected");
        assert!(
            matches!(error, InitializeError::UnsupportedUserVersion { found: 5 }),
            "{error:?}"
        );
        assert_eq!(
            logical_state(&path),
            before,
            "failed initialize changed the database"
        );
    }

    #[test]
    fn initialize_rejects_incompatible_v6_without_changes() {
        let dir = TempDir::new("initialize-incompatible-v6");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        execute(&path, "DROP INDEX ux_tasks_active");
        let before = logical_state(&path);

        let error = initialize(&path).expect_err("incompatible v6 must be rejected");
        assert!(
            matches!(error, InitializeError::Incompatible(_)),
            "{error:?}"
        );
        assert_eq!(logical_state(&path), before);
    }

    #[test]
    fn initialize_rejects_non_empty_uninitialized_without_changes() {
        let dir = TempDir::new("initialize-non-empty-v0");
        let path = dir.join("state.sqlite");
        execute(
            &path,
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        );
        let before = logical_state(&path);
        assert_eq!(before.0, 0, "fixture must start at user_version 0");

        let error = initialize(&path).expect_err("non-empty v0 must be rejected");
        assert!(
            matches!(error, InitializeError::NonEmptyUninitialized),
            "{error:?}"
        );
        assert_eq!(logical_state(&path), before);
    }

    #[test]
    fn initialize_rejects_partial_v0_schema_without_changes() {
        let dir = TempDir::new("initialize-partial-v0");
        let path = dir.join("state.sqlite");
        execute(&path, "CREATE TABLE tasks (task_id TEXT PRIMARY KEY)");
        let before = logical_state(&path);

        let error = initialize(&path).expect_err("partial v0 must be rejected");
        assert!(
            matches!(error, InitializeError::NonEmptyUninitialized),
            "{error:?}"
        );
        assert_eq!(logical_state(&path), before);
    }

    #[test]
    fn missing_database_is_reported_and_not_created() {
        let dir = TempDir::new("missing");
        let path = dir.join("state.sqlite");
        let error = inspect(&path).expect_err("missing database must be rejected");
        assert!(matches!(error, InspectError::NotFound), "{error:?}");
        assert!(
            !path.exists(),
            "inspection created the missing database file"
        );
    }

    #[test]
    fn non_database_file_is_rejected() {
        let dir = TempDir::new("non-database");
        let path = dir.join("state.sqlite");
        std::fs::write(&path, b"this is not a sqlite database").expect("write junk");
        let error = inspect(&path).expect_err("non-database file must be rejected");
        assert!(matches!(error, InspectError::NotADatabase), "{error:?}");
    }

    #[test]
    fn empty_uninitialized_database_is_rejected() {
        let dir = TempDir::new("uninitialized");
        let path = dir.join("state.sqlite");
        std::fs::write(&path, b"").expect("write empty file");
        let error = inspect(&path).expect_err("uninitialized database must be rejected");
        assert!(
            matches!(error, InspectError::UnsupportedUserVersion { found: 0 }),
            "{error:?}"
        );
    }

    #[test]
    fn unsupported_user_version_is_rejected() {
        let dir = TempDir::new("unsupported-version");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        execute(&path, "PRAGMA user_version = 5");
        let error = inspect(&path).expect_err("unsupported version must be rejected");
        assert!(
            matches!(error, InspectError::UnsupportedUserVersion { found: 5 }),
            "{error:?}"
        );
    }

    #[test]
    fn missing_meta_schema_version_is_rejected() {
        let dir = TempDir::new("missing-meta");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        execute(&path, "DELETE FROM meta");
        let error = inspect(&path).expect_err("missing meta.schema_version must be rejected");
        assert!(
            matches!(error, InspectError::MissingSchemaVersion),
            "{error:?}"
        );
    }

    #[test]
    fn mismatched_meta_schema_version_is_rejected() {
        let dir = TempDir::new("mismatched-meta");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        execute(
            &path,
            "UPDATE meta SET value = '7' WHERE key = 'schema_version'",
        );
        let error = inspect(&path).expect_err("mismatched version markers must be rejected");
        assert!(
            matches!(
                error,
                InspectError::MismatchedSchemaVersion {
                    user_version: 6,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn malformed_meta_schema_version_is_rejected() {
        let dir = TempDir::new("malformed-meta");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        execute(
            &path,
            "UPDATE meta SET value = 'six' WHERE key = 'schema_version'",
        );
        let error = inspect(&path).expect_err("malformed meta.schema_version must be rejected");
        assert!(
            matches!(error, InspectError::MalformedSchemaVersion),
            "{error:?}"
        );
    }

    #[test]
    fn incompatible_schemas_are_rejected_by_category() {
        let cases: Vec<(&str, Mutation, SchemaMismatch)> = vec![
            (
                "missing_table",
                Mutation::DropTable("events"),
                SchemaMismatch::MissingTable {
                    table: "events".to_owned(),
                },
            ),
            (
                "unexpected_table",
                Mutation::AddTable,
                SchemaMismatch::UnexpectedTable {
                    table: "extra_table".to_owned(),
                },
            ),
            (
                "missing_column",
                Mutation::DropColumn("tasks", "session_id"),
                SchemaMismatch::MissingColumn {
                    table: "tasks".to_owned(),
                    column: "session_id".to_owned(),
                },
            ),
            (
                "unexpected_column",
                Mutation::AddColumn("tasks", "extra_column"),
                SchemaMismatch::UnexpectedColumn {
                    table: "tasks".to_owned(),
                    column: "extra_column".to_owned(),
                },
            ),
            (
                "missing_index",
                Mutation::DropIndex("ux_tasks_active"),
                SchemaMismatch::MissingIndex {
                    index: "ux_tasks_active".to_owned(),
                },
            ),
            (
                "unexpected_index",
                Mutation::AddIndex,
                SchemaMismatch::UnexpectedIndex {
                    index: "extra_index".to_owned(),
                },
            ),
            (
                "index_definition",
                Mutation::RedefineIndex,
                SchemaMismatch::IndexDefinition {
                    index: "ux_rounds_request".to_owned(),
                },
            ),
            (
                "missing_foreign_key",
                Mutation::NoForeignKey,
                SchemaMismatch::MissingForeignKey {
                    table: "rounds".to_owned(),
                    from: "task_id".to_owned(),
                },
            ),
            (
                "foreign_key_definition",
                Mutation::CascadeForeignKey,
                SchemaMismatch::ForeignKeyDefinition {
                    table: "rounds".to_owned(),
                    from: "task_id".to_owned(),
                },
            ),
            (
                "unexpected_foreign_key",
                Mutation::ExtraForeignKey,
                SchemaMismatch::UnexpectedForeignKey {
                    table: "rounds".to_owned(),
                    from: "task_id".to_owned(),
                },
            ),
        ];

        for (name, mutation, expected) in cases {
            let dir = TempDir::new(name);
            let path = dir.join("state.sqlite");
            apply_mutation(&path, mutation);
            let error = inspect(&path).expect_err("mutated schema must be rejected");
            match error {
                InspectError::IncompatibleSchema(actual) => {
                    assert_eq!(actual, expected, "case {name}");
                }
                other => panic!("case {name}: expected schema mismatch, got {other:?}"),
            }
        }
    }

    #[derive(Clone, Copy)]
    enum Mutation {
        DropTable(&'static str),
        AddTable,
        DropColumn(&'static str, &'static str),
        AddColumn(&'static str, &'static str),
        DropIndex(&'static str),
        AddIndex,
        RedefineIndex,
        NoForeignKey,
        CascadeForeignKey,
        ExtraForeignKey,
    }

    fn apply_mutation(path: &Path, mutation: Mutation) {
        match mutation {
            Mutation::DropTable(table) => {
                create_v6(path);
                execute(path, &format!("DROP TABLE {table}"));
            }
            Mutation::AddTable => {
                create_v6(path);
                execute(path, "CREATE TABLE extra_table (x INTEGER)");
            }
            Mutation::DropColumn(table, column) => {
                create_v6(path);
                execute(path, &format!("ALTER TABLE {table} DROP COLUMN {column}"));
            }
            Mutation::AddColumn(table, column) => {
                create_v6(path);
                execute(
                    path,
                    &format!("ALTER TABLE {table} ADD COLUMN {column} TEXT"),
                );
            }
            Mutation::DropIndex(index) => {
                create_v6(path);
                execute(path, &format!("DROP INDEX {index}"));
            }
            Mutation::AddIndex => {
                create_v6(path);
                execute(path, "CREATE INDEX extra_index ON tasks(project_id)");
            }
            Mutation::RedefineIndex => {
                create_v6(path);
                execute(path, "DROP INDEX ux_rounds_request");
                execute(
                    path,
                    "CREATE UNIQUE INDEX ux_rounds_request ON rounds(request_id)",
                );
            }
            Mutation::NoForeignKey => {
                let schema = V6_SCHEMA.replace(&format!(",\n    {FK_CLAUSE}"), "");
                create_database(path, &schema);
            }
            Mutation::CascadeForeignKey => {
                let schema =
                    V6_SCHEMA.replace(FK_CLAUSE, &format!("{FK_CLAUSE} ON DELETE CASCADE"));
                create_database(path, &schema);
            }
            Mutation::ExtraForeignKey => {
                let schema = V6_SCHEMA.replace(
                    FK_CLAUSE,
                    &format!("{FK_CLAUSE},\n    {FK_CLAUSE} ON UPDATE CASCADE"),
                );
                create_database(path, &schema);
            }
        }
    }

    fn create_v6(path: &Path) {
        create_database(path, V6_SCHEMA);
    }

    fn create_database(path: &Path, schema: &str) {
        let connection = Connection::open(path).expect("open database for creation");
        connection.execute_batch(schema).expect("create schema");
        connection
            .execute_batch(
                "PRAGMA user_version = 6; \
                 INSERT INTO meta (key, value) VALUES ('schema_version', '6');",
            )
            .expect("set version markers");
    }

    fn execute(path: &Path, sql: &str) {
        let connection = Connection::open(path).expect("open database for mutation");
        connection.execute_batch(sql).expect("apply mutation");
    }

    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    }

    const VALID_TASK_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    const TASK_ROW_SELECT: &str = "SELECT ?1 AS task_id, ?2 AS project_id, ?3 AS workspace, \
         ?4 AS status, ?5 AS session_id, ?6 AS task, ?7 AS allowed_paths, \
         ?8 AS test_commands, ?9 AS created_at, ?10 AS updated_at, ?11 AS base_head, \
         ?12 AS snapshot, ?13 AS revision_count, ?14 AS close_requested_at, \
         ?15 AS close_reason";

    /// A valid row with every column present, using a UUID `task_id` (the
    /// committed fixtures use synthetic `task-<n>` ids and are not mapped here).
    fn valid_task_values() -> Vec<SqlValue> {
        vec![
            SqlValue::Text(VALID_TASK_ID.to_owned()),
            SqlValue::Text("proj".to_owned()),
            SqlValue::Text("/fixture/workspace".to_owned()),
            SqlValue::Text("implementing".to_owned()),
            SqlValue::Text("ses-1".to_owned()),
            SqlValue::Text("Implement the fixture change".to_owned()),
            SqlValue::Text("[\"module.py\"]".to_owned()),
            SqlValue::Text("[\"pytest -q\"]".to_owned()),
            SqlValue::Text("2026-01-01T00:00:00.000+00:00".to_owned()),
            SqlValue::Text("2026-01-01T00:00:01.000+00:00".to_owned()),
            SqlValue::Text("1111111111111111111111111111111111111111".to_owned()),
            SqlValue::Text("{\"head\":\"abc\"}".to_owned()),
            SqlValue::Integer(2),
            SqlValue::Text("2026-01-01T00:00:02.000+00:00".to_owned()),
            SqlValue::Text("user asked to stop".to_owned()),
        ]
    }

    /// Maps a synthetic `SELECT` of the fifteen `tasks` columns through
    /// [`Task::from_row`], exercising a real [`rusqlite::Row`].
    fn map_task_values(values: &[SqlValue]) -> Result<Task, TaskRowError> {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        connection
            .query_row(
                TASK_ROW_SELECT,
                rusqlite::params_from_iter(values.iter().cloned()),
                |row| Ok(Task::from_row(row)),
            )
            .expect("row query must execute")
    }

    fn assert_error_is_safe(error: &TaskRowError, secret: &str) {
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(
            !display.contains(secret),
            "Display leaked row data: {display}"
        );
        assert!(!debug.contains(secret), "Debug leaked row data: {debug}");
    }

    #[test]
    fn task_row_maps_all_fifteen_columns() {
        let task = map_task_values(&valid_task_values()).expect("valid row must map");

        assert_eq!(task.task_id.to_string(), VALID_TASK_ID);
        assert_eq!(task.project_id.as_str(), "proj");
        assert_eq!(task.workspace, "/fixture/workspace");
        assert_eq!(task.status, TaskStatus::Implementing);
        assert_eq!(task.session_id.as_deref(), Some("ses-1"));
        assert_eq!(task.text, "Implement the fixture change");
        assert_eq!(task.allowed_paths, ["module.py"]);
        assert_eq!(task.test_commands, ["pytest -q"]);
        assert_eq!(task.created_at, "2026-01-01T00:00:00.000+00:00");
        assert_eq!(task.updated_at, "2026-01-01T00:00:01.000+00:00");
        assert_eq!(
            task.base_head.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(task.snapshot, Some(serde_json::json!({"head": "abc"})));
        assert_eq!(task.revision_count, 2);
        assert_eq!(
            task.close_requested_at.as_deref(),
            Some("2026-01-01T00:00:02.000+00:00")
        );
        assert_eq!(task.close_reason.as_deref(), Some("user asked to stop"));
    }

    #[test]
    fn task_row_maps_row_from_schema_v6_table() {
        let dir = TempDir::new("task-row-schema");
        let path = dir.join("state.sqlite");
        create_v6(&path);

        let connection = Connection::open(&path).expect("open database");
        connection
            .execute(
                "INSERT INTO tasks (task_id, project_id, workspace, status, session_id, task, \
                 allowed_paths, test_commands, created_at, updated_at, base_head, snapshot, \
                 revision_count, close_requested_at, close_reason) \
                 VALUES (?1, 'proj', '/fixture/workspace', 'implementing', 'ses-1', \
                 'Implement the fixture change', '[\"module.py\"]', '[\"pytest -q\"]', \
                 '2026-01-01T00:00:00.000+00:00', '2026-01-01T00:00:01.000+00:00', \
                 '1111111111111111111111111111111111111111', '{\"head\":\"abc\"}', 0, NULL, NULL)",
                rusqlite::params![VALID_TASK_ID],
            )
            .expect("insert task row");

        let task = connection
            .query_row("SELECT * FROM tasks", [], |row| Ok(Task::from_row(row)))
            .expect("row query must execute")
            .expect("valid persisted row must map");

        assert_eq!(task.task_id.to_string(), VALID_TASK_ID);
        assert_eq!(task.status, TaskStatus::Implementing);
        assert_eq!(task.allowed_paths, ["module.py"]);
        assert_eq!(task.revision_count, 0);
        assert_eq!(task.snapshot, Some(serde_json::json!({"head": "abc"})));
    }

    #[test]
    fn task_row_preserves_nullable_columns() {
        let mut values = valid_task_values();
        values[4] = SqlValue::Null;
        values[10] = SqlValue::Null;
        values[11] = SqlValue::Null;
        values[13] = SqlValue::Null;
        values[14] = SqlValue::Null;

        let task = map_task_values(&values).expect("nullable row must map");
        assert_eq!(task.session_id, None);
        assert_eq!(task.base_head, None);
        assert_eq!(task.snapshot, None);
        assert_eq!(task.close_requested_at, None);
        assert_eq!(task.close_reason, None);
    }

    #[test]
    fn task_row_maps_every_status() {
        for status in TaskStatus::ALL {
            let mut values = valid_task_values();
            values[3] = SqlValue::Text(status.as_str().to_owned());
            let task = map_task_values(&values).expect("known status must map");
            assert_eq!(task.status, status);
        }
    }

    #[test]
    fn task_row_classifies_active_and_terminal_statuses() {
        let active = [
            TaskStatus::Implementing,
            TaskStatus::AwaitingReview,
            TaskStatus::Revising,
            TaskStatus::NeedsUser,
            TaskStatus::Failed,
            TaskStatus::DeliveryUnknown,
        ];
        for status in active {
            let mut values = valid_task_values();
            values[3] = SqlValue::Text(status.as_str().to_owned());
            let task = map_task_values(&values).expect("active status must map");
            assert!(task.status.is_active(), "{status} must be active");
            assert!(!task.status.is_terminal(), "{status} must not be terminal");
        }

        for status in [TaskStatus::Accepted, TaskStatus::Closed] {
            let mut values = valid_task_values();
            values[3] = SqlValue::Text(status.as_str().to_owned());
            let task = map_task_values(&values).expect("terminal status must map");
            assert!(task.status.is_terminal(), "{status} must be terminal");
            assert!(!task.status.is_active(), "{status} must not be active");
        }
    }

    #[test]
    fn task_row_rejects_unknown_status() {
        let mut values = valid_task_values();
        values[3] = SqlValue::Text("bogus".to_owned());
        let error = map_task_values(&values).expect_err("unknown status must fail");
        assert!(matches!(error, TaskRowError::UnknownStatus), "{error:?}");
    }

    #[test]
    fn task_row_rejects_invalid_task_uuid() {
        let mut values = valid_task_values();
        values[0] = SqlValue::Text("task-1".to_owned());
        let error = map_task_values(&values).expect_err("synthetic task id must fail");
        assert!(matches!(error, TaskRowError::InvalidTaskId), "{error:?}");
    }

    #[test]
    fn task_row_rejects_empty_project_id() {
        let mut values = valid_task_values();
        values[1] = SqlValue::Text(String::new());
        let error = map_task_values(&values).expect_err("empty project id must fail");
        assert!(matches!(error, TaskRowError::InvalidProjectId), "{error:?}");
    }

    #[test]
    fn task_row_rejects_malformed_json_columns() {
        let cases: [(usize, &str); 3] =
            [(6, "allowed_paths"), (7, "test_commands"), (11, "snapshot")];
        for (index, column) in cases {
            let mut values = valid_task_values();
            values[index] = SqlValue::Text("{not json".to_owned());
            let error = map_task_values(&values).expect_err("malformed JSON must fail");
            match error {
                TaskRowError::MalformedJson { column: actual } => assert_eq!(actual, column),
                other => panic!("{column}: expected malformed JSON, got {other:?}"),
            }
        }
    }

    #[test]
    fn task_row_rejects_wrong_shaped_json_arrays() {
        let cases: [(usize, &str, &str); 4] = [
            (6, "allowed_paths", "{\"module.py\": true}"),
            (6, "allowed_paths", "[\"module.py\", 3]"),
            (7, "test_commands", "\"pytest -q\""),
            (7, "test_commands", "[[\"pytest\"]]"),
        ];
        for (index, column, raw) in cases {
            let mut values = valid_task_values();
            values[index] = SqlValue::Text(raw.to_owned());
            let error = map_task_values(&values).expect_err("wrong shape must fail");
            match error {
                TaskRowError::WrongJsonShape { column: actual } => assert_eq!(actual, column),
                other => panic!("{column}: expected wrong shape, got {other:?}"),
            }
        }
    }

    #[test]
    fn task_row_rejects_wrong_shaped_snapshot_json() {
        let cases: [&str; 4] = ["[]", "\"text\"", "1", "true"];
        for raw in cases {
            let mut values = valid_task_values();
            values[11] = SqlValue::Text(raw.to_owned());
            let error = map_task_values(&values).expect_err("wrong snapshot shape must fail");
            match error {
                TaskRowError::WrongJsonShape { column } => assert_eq!(column, "snapshot"),
                other => panic!("snapshot {raw}: expected wrong shape, got {other:?}"),
            }
        }
    }

    #[test]
    fn task_row_maps_json_null_snapshot_to_none() {
        let mut values = valid_task_values();
        values[11] = SqlValue::Text("null".to_owned());
        let task = map_task_values(&values).expect("JSON null snapshot must map");
        assert_eq!(task.snapshot, None);
    }

    #[test]
    fn task_row_rejects_sqlite_type_mismatches() {
        let cases: [(usize, &str, SqlValue); 4] = [
            (2, "workspace", SqlValue::Integer(7)),
            (5, "task", SqlValue::Null),
            (6, "allowed_paths", SqlValue::Null),
            (12, "revision_count", SqlValue::Text("7".to_owned())),
        ];
        for (index, column, replacement) in cases {
            let mut values = valid_task_values();
            values[index] = replacement;
            let error = map_task_values(&values).expect_err("type mismatch must fail");
            match error {
                TaskRowError::ColumnType { column: actual } => assert_eq!(actual, column),
                other => panic!("{column}: expected column type error, got {other:?}"),
            }
        }
    }

    #[test]
    fn task_row_rejects_negative_revision_count() {
        let mut values = valid_task_values();
        values[12] = SqlValue::Integer(-1);
        let error = map_task_values(&values).expect_err("negative revision count must fail");
        assert!(
            matches!(error, TaskRowError::NegativeRevisionCount),
            "{error:?}"
        );
    }

    #[test]
    fn task_row_rejects_missing_column() {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        let result = connection
            .query_row(
                "SELECT ?1 AS task_id",
                rusqlite::params![VALID_TASK_ID],
                |row| Ok(Task::from_row(row)),
            )
            .expect("row query must execute");
        let error = result.expect_err("missing column must fail");
        match error {
            TaskRowError::MissingColumn { column } => assert_eq!(column, "project_id"),
            other => panic!("expected missing column, got {other:?}"),
        }
    }

    #[test]
    fn task_row_errors_do_not_leak_row_data() {
        const SECRET: &str = "super-secret-token";

        let mut values = valid_task_values();
        values[0] = SqlValue::Text(SECRET.to_owned());
        let error = map_task_values(&values).expect_err("invalid uuid must fail");
        assert_error_is_safe(&error, SECRET);

        let mut values = valid_task_values();
        values[3] = SqlValue::Text(SECRET.to_owned());
        let error = map_task_values(&values).expect_err("unknown status must fail");
        assert_error_is_safe(&error, SECRET);

        let mut values = valid_task_values();
        values[6] = SqlValue::Text(SECRET.to_owned());
        let error = map_task_values(&values).expect_err("malformed JSON must fail");
        assert_error_is_safe(&error, SECRET);

        let mut values = valid_task_values();
        values[7] = SqlValue::Text(format!("{{\"secret\": \"{SECRET}\"}}"));
        let error = map_task_values(&values).expect_err("wrong shape must fail");
        assert_error_is_safe(&error, SECRET);
    }

    const ROUND_ROW_SELECT: &str = "SELECT ?1 AS task_id, ?2 AS project_id, ?3 AS round_number, \
         ?4 AS request_id, ?5 AS payload_hash, ?6 AS kind, ?7 AS status, \
         ?8 AS outbound_message_id, ?9 AS attempted, ?10 AS response_message_id, \
         ?11 AS response, ?12 AS error_code, ?13 AS result_json, ?14 AS findings, \
         ?15 AS session_id, ?16 AS worker_started_at, ?17 AS worker_deadline_at, \
         ?18 AS verifier_state, ?19 AS verifier_json, ?20 AS created_at, ?21 AS updated_at";

    const ROUND_ROW_COLUMNS: &str = "task_id, project_id, round_number, request_id, payload_hash, \
         kind, status, outbound_message_id, attempted, response_message_id, response, error_code, \
         result_json, findings, session_id, worker_started_at, worker_deadline_at, verifier_state, \
         verifier_json, created_at, updated_at";

    fn valid_verifier_json() -> String {
        serde_json::json!({
            "status": "passed",
            "commands": [{"command": "pytest -q", "duration": 1.234, "exit_code": 0}],
            "log": "verification/task-1/round_1"
        })
        .to_string()
    }

    /// A valid row with every one of the twenty-one `rounds` columns present,
    /// using a UUID `task_id` (the committed fixtures use synthetic `task-<n>`
    /// ids and are not mapped here).
    fn valid_round_values() -> Vec<SqlValue> {
        vec![
            SqlValue::Text(VALID_TASK_ID.to_owned()),
            SqlValue::Text("proj".to_owned()),
            SqlValue::Integer(1),
            SqlValue::Text("req-1".to_owned()),
            SqlValue::Text("hash-req-1".to_owned()),
            SqlValue::Text("implement".to_owned()),
            SqlValue::Text("complete".to_owned()),
            SqlValue::Text("msg-1".to_owned()),
            SqlValue::Integer(1),
            SqlValue::Text("msg-2".to_owned()),
            SqlValue::Text("Implemented the change.".to_owned()),
            SqlValue::Null,
            SqlValue::Text("{\"changed_paths\":[]}".to_owned()),
            SqlValue::Null,
            SqlValue::Text("ses-1".to_owned()),
            SqlValue::Text("2026-01-01T00:00:01.000+00:00".to_owned()),
            SqlValue::Text("2026-01-01T00:15:01.000+00:00".to_owned()),
            SqlValue::Text("done".to_owned()),
            SqlValue::Text(valid_verifier_json()),
            SqlValue::Text("2026-01-01T00:00:00.000+00:00".to_owned()),
            SqlValue::Text("2026-01-01T00:00:03.000+00:00".to_owned()),
        ]
    }

    /// Maps a synthetic `SELECT` of the twenty-one `rounds` columns through
    /// [`RoundRow::from_row`], exercising a real [`rusqlite::Row`].
    fn map_round_values(values: &[SqlValue]) -> Result<RoundRow, RoundRowError> {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        connection
            .query_row(
                ROUND_ROW_SELECT,
                rusqlite::params_from_iter(values.iter().cloned()),
                |row| Ok(RoundRow::from_row(row)),
            )
            .expect("row query must execute")
    }

    fn assert_round_error_is_safe(error: &RoundRowError, secret: &str) {
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(
            !display.contains(secret),
            "Display leaked row data: {display}"
        );
        assert!(!debug.contains(secret), "Debug leaked row data: {debug}");
    }

    #[test]
    fn round_row_maps_all_twenty_one_columns() {
        let row = map_round_values(&valid_round_values()).expect("valid row must map");

        assert_eq!(row.task_id.to_string(), VALID_TASK_ID);
        assert_eq!(row.project_id.as_str(), "proj");
        assert_eq!(row.round_number, 1);
        assert_eq!(row.request_id, "req-1");
        assert_eq!(row.payload_hash, "hash-req-1");
        assert_eq!(row.kind, RoundKind::Implement);
        assert_eq!(row.status, RoundStatus::Complete);
        assert_eq!(row.outbound_message_id.as_deref(), Some("msg-1"));
        assert!(row.attempted);
        assert_eq!(row.response_message_id.as_deref(), Some("msg-2"));
        assert_eq!(row.response.as_deref(), Some("Implemented the change."));
        assert_eq!(row.error_code, None);
        assert_eq!(
            row.result_json,
            Some(serde_json::json!({"changed_paths": []}))
        );
        assert_eq!(row.findings, None);
        assert_eq!(row.session_id.as_deref(), Some("ses-1"));
        assert_eq!(
            row.worker_started_at.as_deref(),
            Some("2026-01-01T00:00:01.000+00:00")
        );
        assert_eq!(
            row.worker_deadline_at.as_deref(),
            Some("2026-01-01T00:15:01.000+00:00")
        );
        assert_eq!(row.verifier_state, Some(VerifierState::Done));
        assert_eq!(
            row.verifier_json
                .as_ref()
                .map(|verification| verification.status),
            Some(bridge_domain::VerificationStatus::Passed)
        );
        assert_eq!(row.created_at, "2026-01-01T00:00:00.000+00:00");
        assert_eq!(row.updated_at, "2026-01-01T00:00:03.000+00:00");
    }

    #[test]
    fn round_row_maps_row_from_schema_v6_table() {
        let dir = TempDir::new("round-row-schema");
        let path = dir.join("state.sqlite");
        create_v6(&path);

        let connection = Connection::open(&path).expect("open database");
        connection
            .execute(
                "INSERT INTO tasks (task_id, project_id, workspace, status, task, allowed_paths, \
                 test_commands, created_at, updated_at, revision_count) \
                 VALUES (?1, 'proj', '/fixture/workspace', 'implementing', 't', '[]', '[]', \
                 '2026-01-01T00:00:00.000+00:00', '2026-01-01T00:00:00.000+00:00', 0)",
                rusqlite::params![VALID_TASK_ID],
            )
            .expect("insert task row");
        connection
            .execute(
                "INSERT INTO rounds (task_id, project_id, round_number, request_id, payload_hash, \
                 kind, status, outbound_message_id, attempted, response_message_id, response, \
                 error_code, result_json, findings, session_id, worker_started_at, \
                 worker_deadline_at, verifier_state, verifier_json, created_at, updated_at) \
                 VALUES (?1, 'proj', 1, 'req-1', 'hash-req-1', 'implement', 'observing', 'msg-1', \
                 1, NULL, NULL, NULL, NULL, NULL, 'ses-1', '2026-01-01T00:00:01.000+00:00', \
                 '2026-01-01T00:15:01.000+00:00', NULL, NULL, '2026-01-01T00:00:00.000+00:00', \
                 '2026-01-01T00:00:01.000+00:00')",
                rusqlite::params![VALID_TASK_ID],
            )
            .expect("insert round row");

        let row = connection
            .query_row(
                &format!("SELECT {ROUND_ROW_COLUMNS} FROM rounds"),
                [],
                |row| Ok(RoundRow::from_row(row)),
            )
            .expect("row query must execute")
            .expect("valid persisted row must map");

        assert_eq!(row.task_id.to_string(), VALID_TASK_ID);
        assert_eq!(row.kind, RoundKind::Implement);
        assert_eq!(row.status, RoundStatus::Observing);
        assert!(row.attempted);
        assert_eq!(row.verifier_state, None);
        assert_eq!(row.result_json, None);
    }

    #[test]
    fn round_row_preserves_nullable_columns() {
        let mut values = valid_round_values();
        values[7] = SqlValue::Null;
        values[9] = SqlValue::Null;
        values[10] = SqlValue::Null;
        values[11] = SqlValue::Null;
        values[12] = SqlValue::Null;
        values[13] = SqlValue::Null;
        values[14] = SqlValue::Null;
        values[15] = SqlValue::Null;
        values[16] = SqlValue::Null;
        values[17] = SqlValue::Null;
        values[18] = SqlValue::Null;

        let row = map_round_values(&values).expect("nullable row must map");
        assert_eq!(row.outbound_message_id, None);
        assert_eq!(row.response_message_id, None);
        assert_eq!(row.response, None);
        assert_eq!(row.error_code, None);
        assert_eq!(row.result_json, None);
        assert_eq!(row.findings, None);
        assert_eq!(row.session_id, None);
        assert_eq!(row.worker_started_at, None);
        assert_eq!(row.worker_deadline_at, None);
        assert_eq!(row.verifier_state, None);
        assert_eq!(row.verifier_json, None);
    }

    #[test]
    fn round_row_maps_every_kind() {
        for kind in RoundKind::ALL {
            let mut values = valid_round_values();
            values[5] = SqlValue::Text(kind.as_str().to_owned());
            let row = map_round_values(&values).expect("known kind must map");
            assert_eq!(row.kind, kind);
        }
    }

    #[test]
    fn round_row_maps_every_status() {
        for status in RoundStatus::ALL {
            let mut values = valid_round_values();
            values[6] = SqlValue::Text(status.as_str().to_owned());
            let row = map_round_values(&values).expect("known status must map");
            assert_eq!(row.status, status);
        }
    }

    #[test]
    fn round_row_maps_every_verifier_state() {
        for state in VerifierState::ALL {
            let mut values = valid_round_values();
            values[17] = SqlValue::Text(state.as_str().to_owned());
            if state == VerifierState::Done {
                values[18] = SqlValue::Text(valid_verifier_json());
            } else {
                values[18] = SqlValue::Null;
            }
            let row = map_round_values(&values).expect("known verifier state must map");
            assert_eq!(row.verifier_state, Some(state));
            assert_eq!(row.verifier_json.is_some(), state == VerifierState::Done);
        }
    }

    #[test]
    fn round_row_accepts_round_number_boundaries() {
        for number in [1_i64, i64::from(u32::MAX)] {
            let mut values = valid_round_values();
            values[2] = SqlValue::Integer(number);
            let row = map_round_values(&values).expect("boundary round number must map");
            assert_eq!(i64::from(row.round_number), number);
        }
    }

    #[test]
    fn round_row_rejects_round_number_out_of_range() {
        for number in [0_i64, -1, i64::from(u32::MAX) + 1] {
            let mut values = valid_round_values();
            values[2] = SqlValue::Integer(number);
            let error = map_round_values(&values).expect_err("out-of-range round number must fail");
            assert!(
                matches!(error, RoundRowError::InvalidRoundNumber),
                "{number}: {error:?}"
            );
        }
    }

    #[test]
    fn round_row_maps_attempted_boolean() {
        let mut values = valid_round_values();
        values[8] = SqlValue::Integer(0);
        let row = map_round_values(&values).expect("attempted=0 must map");
        assert!(!row.attempted);

        let mut values = valid_round_values();
        values[8] = SqlValue::Integer(1);
        let row = map_round_values(&values).expect("attempted=1 must map");
        assert!(row.attempted);
    }

    #[test]
    fn round_row_rejects_invalid_attempted() {
        for attempted in [2_i64, -1] {
            let mut values = valid_round_values();
            values[8] = SqlValue::Integer(attempted);
            let error = map_round_values(&values).expect_err("invalid attempted must fail");
            assert!(
                matches!(error, RoundRowError::InvalidAttempted),
                "{attempted}: {error:?}"
            );
        }
    }

    #[test]
    fn round_row_rejects_unknown_vocabulary() {
        let mut values = valid_round_values();
        values[5] = SqlValue::Text("bogus".to_owned());
        assert!(matches!(
            map_round_values(&values).expect_err("unknown kind must fail"),
            RoundRowError::UnknownKind
        ));

        let mut values = valid_round_values();
        values[6] = SqlValue::Text("bogus".to_owned());
        assert!(matches!(
            map_round_values(&values).expect_err("unknown status must fail"),
            RoundRowError::UnknownStatus
        ));

        let mut values = valid_round_values();
        values[17] = SqlValue::Text("bogus".to_owned());
        assert!(matches!(
            map_round_values(&values).expect_err("unknown verifier state must fail"),
            RoundRowError::UnknownVerifierState
        ));
    }

    #[test]
    fn round_row_rejects_invalid_identifiers() {
        let mut values = valid_round_values();
        values[0] = SqlValue::Text("task-1".to_owned());
        assert!(matches!(
            map_round_values(&values).expect_err("synthetic task id must fail"),
            RoundRowError::InvalidTaskId
        ));

        let mut values = valid_round_values();
        values[1] = SqlValue::Text(String::new());
        assert!(matches!(
            map_round_values(&values).expect_err("empty project id must fail"),
            RoundRowError::InvalidProjectId
        ));
    }

    #[test]
    fn round_row_rejects_malformed_json_columns() {
        for (index, column) in [(12_usize, "result_json"), (18, "verifier_json")] {
            let mut values = valid_round_values();
            values[index] = SqlValue::Text("{not json".to_owned());
            let error = map_round_values(&values).expect_err("malformed JSON must fail");
            match error {
                RoundRowError::MalformedJson { column: actual } => assert_eq!(actual, column),
                other => panic!("{column}: expected malformed JSON, got {other:?}"),
            }
        }
    }

    #[test]
    fn round_row_rejects_wrong_shaped_result_json() {
        for raw in ["[]", "\"text\"", "1", "true"] {
            let mut values = valid_round_values();
            values[12] = SqlValue::Text(raw.to_owned());
            let error = map_round_values(&values).expect_err("wrong result shape must fail");
            match error {
                RoundRowError::WrongJsonShape { column } => assert_eq!(column, "result_json"),
                other => panic!("result_json {raw}: expected wrong shape, got {other:?}"),
            }
        }
    }

    #[test]
    fn round_row_rejects_wrong_shaped_verifier_json() {
        for raw in [
            "[]",
            "\"text\"",
            "1",
            "true",
            "{\"status\":\"bogus\",\"log\":\"x\"}",
            "{\"status\":\"passed\"}",
        ] {
            let mut values = valid_round_values();
            values[18] = SqlValue::Text(raw.to_owned());
            let error = map_round_values(&values).expect_err("wrong verifier shape must fail");
            match error {
                RoundRowError::WrongJsonShape { column } => assert_eq!(column, "verifier_json"),
                other => panic!("verifier_json {raw}: expected wrong shape, got {other:?}"),
            }
        }
    }

    #[test]
    fn round_row_maps_json_null_result_json_to_none() {
        let mut values = valid_round_values();
        values[12] = SqlValue::Text("null".to_owned());
        let row = map_round_values(&values).expect("JSON null result must map");
        assert_eq!(row.result_json, None);
    }

    #[test]
    fn round_row_rejects_inconsistent_verifier_pairs() {
        let mut done_without_payload = valid_round_values();
        done_without_payload[18] = SqlValue::Null;
        assert!(matches!(
            map_round_values(&done_without_payload).expect_err("done without payload must fail"),
            RoundRowError::InconsistentVerifier
        ));

        let mut running_with_payload = valid_round_values();
        running_with_payload[17] = SqlValue::Text("running".to_owned());
        assert!(matches!(
            map_round_values(&running_with_payload).expect_err("running with payload must fail"),
            RoundRowError::InconsistentVerifier
        ));

        let mut absent_state_with_payload = valid_round_values();
        absent_state_with_payload[17] = SqlValue::Null;
        assert!(matches!(
            map_round_values(&absent_state_with_payload)
                .expect_err("absent state with payload must fail"),
            RoundRowError::InconsistentVerifier
        ));
    }

    #[test]
    fn round_row_rejects_sqlite_type_mismatches() {
        let cases: [(usize, &str, SqlValue); 5] = [
            (0, "task_id", SqlValue::Null),
            (2, "round_number", SqlValue::Text("1".to_owned())),
            (8, "attempted", SqlValue::Text("1".to_owned())),
            (12, "result_json", SqlValue::Integer(1)),
            (17, "verifier_state", SqlValue::Integer(1)),
        ];
        for (index, column, replacement) in cases {
            let mut values = valid_round_values();
            values[index] = replacement;
            let error = map_round_values(&values).expect_err("type mismatch must fail");
            match error {
                RoundRowError::ColumnType { column: actual } => assert_eq!(actual, column),
                other => panic!("{column}: expected column type error, got {other:?}"),
            }
        }
    }

    #[test]
    fn round_row_rejects_missing_column() {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        let result = connection
            .query_row(
                "SELECT ?1 AS task_id",
                rusqlite::params![VALID_TASK_ID],
                |row| Ok(RoundRow::from_row(row)),
            )
            .expect("row query must execute");
        let error = result.expect_err("missing column must fail");
        match error {
            RoundRowError::MissingColumn { column } => assert_eq!(column, "project_id"),
            other => panic!("expected missing column, got {other:?}"),
        }
    }

    #[test]
    fn round_row_rejects_fixture_synthetic_task_id_without_touching_fixture() {
        let path = fixture_dir().join("active-v6.sqlite");
        let before = std::fs::read(&path).expect("read fixture");

        let connection = open_read_only(&path).expect("open fixture read-only");
        let result = connection
            .query_row(
                &format!("SELECT {ROUND_ROW_COLUMNS} FROM rounds"),
                [],
                |row| Ok(RoundRow::from_row(row)),
            )
            .expect("row query must execute");
        let error = result.expect_err("synthetic task id must be rejected");
        assert!(matches!(error, RoundRowError::InvalidTaskId), "{error:?}");

        assert_eq!(std::fs::read(&path).expect("re-read fixture"), before);
        assert!(
            !sidecar(&path, "-wal").exists(),
            "fixture got a -wal sidecar"
        );
        assert!(
            !sidecar(&path, "-shm").exists(),
            "fixture got a -shm sidecar"
        );
    }

    #[test]
    fn round_row_errors_do_not_leak_row_data() {
        const SECRET: &str = "super-secret-token";

        let mut values = valid_round_values();
        values[0] = SqlValue::Text(SECRET.to_owned());
        let error = map_round_values(&values).expect_err("invalid uuid must fail");
        assert_round_error_is_safe(&error, SECRET);

        let mut values = valid_round_values();
        values[5] = SqlValue::Text(SECRET.to_owned());
        let error = map_round_values(&values).expect_err("unknown kind must fail");
        assert_round_error_is_safe(&error, SECRET);

        let mut values = valid_round_values();
        values[6] = SqlValue::Text(SECRET.to_owned());
        let error = map_round_values(&values).expect_err("unknown status must fail");
        assert_round_error_is_safe(&error, SECRET);

        let mut values = valid_round_values();
        values[17] = SqlValue::Text(SECRET.to_owned());
        let error = map_round_values(&values).expect_err("unknown verifier state must fail");
        assert_round_error_is_safe(&error, SECRET);

        let mut values = valid_round_values();
        values[12] = SqlValue::Text(SECRET.to_owned());
        let error = map_round_values(&values).expect_err("malformed result JSON must fail");
        assert_round_error_is_safe(&error, SECRET);

        let mut values = valid_round_values();
        values[12] = SqlValue::Text(format!("[\"{SECRET}\"]"));
        let error = map_round_values(&values).expect_err("wrong result shape must fail");
        assert_round_error_is_safe(&error, SECRET);

        let mut values = valid_round_values();
        values[18] = SqlValue::Text(format!("{{\"secret\":\"{SECRET}\"}}"));
        let error = map_round_values(&values).expect_err("wrong verifier shape must fail");
        assert_round_error_is_safe(&error, SECRET);
    }

    #[test]
    fn round_row_is_not_the_domain_round() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RoundRow>();
        assert_ne!(
            std::any::TypeId::of::<RoundRow>(),
            std::any::TypeId::of::<bridge_domain::Round>()
        );
    }

    const QUERY_PROJECT_A: &str = "query-project-a";
    const QUERY_PROJECT_B: &str = "query-project-b";

    const TS_EARLY: &str = "2026-01-01T00:00:00.000+00:00";
    const TS_MID: &str = "2026-01-02T00:00:00.000+00:00";
    const TS_LATE: &str = "2026-01-03T00:00:00.000+00:00";

    /// A distinct valid UUID per small `n`, never the synthetic `task-<n>` ids
    /// used by the committed fixtures.
    fn query_task_id(n: u32) -> TaskId {
        TaskId::from_str(&format!("550e8400-e29b-41d4-a716-44665544{n:04}"))
            .expect("generated task id must be a valid UUID")
    }

    fn query_project(name: &str) -> ProjectId {
        ProjectId::from_str(name).expect("project id must be valid")
    }

    fn open_query_storage(tag: &str) -> (TempDir, StorageConnection) {
        let dir = TempDir::new(tag);
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let storage = connect(&path).expect("connect must succeed");
        (dir, storage)
    }

    fn insert_query_task(
        storage: &StorageConnection,
        task_id: TaskId,
        project_id: &ProjectId,
        status: TaskStatus,
        created_at: &str,
        updated_at: &str,
    ) {
        storage
            .connection()
            .execute(
                "INSERT INTO tasks (task_id, project_id, workspace, status, task, allowed_paths, \
                 test_commands, created_at, updated_at, revision_count) \
                 VALUES (?1, ?2, '/fixture/workspace', ?3, 'task text', '[]', '[]', ?4, ?5, 0)",
                rusqlite::params![
                    task_id.to_string(),
                    project_id.as_str(),
                    status.as_str(),
                    created_at,
                    updated_at
                ],
            )
            .expect("insert task row");
    }

    #[test]
    fn query_get_task_finds_exact_id_and_returns_none_for_missing() {
        let (_dir, storage) = open_query_storage("query-get");
        let project = query_project(QUERY_PROJECT_A);
        insert_query_task(
            &storage,
            query_task_id(1),
            &project,
            TaskStatus::Implementing,
            TS_EARLY,
            TS_EARLY,
        );

        let found = storage
            .get_task(query_task_id(1))
            .expect("query must succeed")
            .expect("task must be found");
        assert_eq!(found.task_id, query_task_id(1));
        assert_eq!(found.project_id, project);
        assert_eq!(found.status, TaskStatus::Implementing);

        assert!(
            storage
                .get_task(query_task_id(2))
                .expect("query must succeed")
                .is_none()
        );
    }

    #[test]
    fn query_on_freshly_initialized_database_returns_empty_results() {
        let dir = TempDir::new("query-initialized-empty");
        let path = dir.join("state.sqlite");
        assert!(!path.exists(), "database must not exist before initialize");

        initialize(&path).expect("missing file must be initialized");
        let storage = connect(&path).expect("connect must succeed");
        let project = query_project(QUERY_PROJECT_A);

        assert!(
            storage
                .get_task(query_task_id(1))
                .expect("query must succeed")
                .is_none()
        );
        assert!(
            storage
                .get_active_task(&project)
                .expect("query must succeed")
                .is_none()
        );
        assert!(
            storage
                .list_tasks(&project, false, 10, 0)
                .expect("query must succeed")
                .is_empty()
        );
        assert!(
            storage
                .list_tasks(&project, true, 10, 0)
                .expect("query must succeed")
                .is_empty()
        );
        assert_eq!(storage.count_tasks(&project, false).expect("count"), 0);
        assert_eq!(storage.count_tasks(&project, true).expect("count"), 0);
    }

    #[test]
    fn query_isolates_projects() {
        let (_dir, storage) = open_query_storage("query-isolation");
        let project_a = query_project(QUERY_PROJECT_A);
        let project_b = query_project(QUERY_PROJECT_B);
        let other = query_project("query-project-c");
        insert_query_task(
            &storage,
            query_task_id(1),
            &project_a,
            TaskStatus::Implementing,
            TS_EARLY,
            TS_EARLY,
        );
        insert_query_task(
            &storage,
            query_task_id(2),
            &project_b,
            TaskStatus::AwaitingReview,
            TS_MID,
            TS_MID,
        );

        let active_a = storage
            .get_active_task(&project_a)
            .expect("query must succeed")
            .expect("project a must have an active task");
        assert_eq!(active_a.task_id, query_task_id(1));
        let active_b = storage
            .get_active_task(&project_b)
            .expect("query must succeed")
            .expect("project b must have an active task");
        assert_eq!(active_b.task_id, query_task_id(2));
        assert!(
            storage
                .get_active_task(&other)
                .expect("query must succeed")
                .is_none()
        );

        let listed_a: Vec<TaskId> = storage
            .list_tasks(&project_a, false, 10, 0)
            .expect("query must succeed")
            .iter()
            .map(|task| task.task_id)
            .collect();
        assert_eq!(listed_a, vec![query_task_id(1)]);
        let listed_b: Vec<TaskId> = storage
            .list_tasks(&project_b, false, 10, 0)
            .expect("query must succeed")
            .iter()
            .map(|task| task.task_id)
            .collect();
        assert_eq!(listed_b, vec![query_task_id(2)]);
        assert!(
            storage
                .list_tasks(&other, false, 10, 0)
                .expect("query must succeed")
                .is_empty()
        );

        assert_eq!(storage.count_tasks(&project_a, false).expect("count"), 1);
        assert_eq!(storage.count_tasks(&project_b, false).expect("count"), 1);
        assert_eq!(storage.count_tasks(&other, false).expect("count"), 0);
    }

    #[test]
    fn query_active_vocabulary_matches_is_active() {
        let (_dir, storage) = open_query_storage("query-active-vocabulary");
        let active: Vec<TaskStatus> = TaskStatus::ALL
            .iter()
            .copied()
            .filter(|status| status.is_active())
            .collect();
        assert_eq!(active.len(), 7);

        for (index, status) in active.into_iter().enumerate() {
            let project = query_project(&format!("query-active-{}", status.as_str()));
            let number = u32::try_from(index + 1).expect("small index");
            insert_query_task(
                &storage,
                query_task_id(number),
                &project,
                status,
                TS_EARLY,
                TS_EARLY,
            );

            let found = storage
                .get_active_task(&project)
                .expect("query must succeed")
                .expect("active status must be found");
            assert_eq!(found.status, status, "status {status}");
            assert_eq!(
                storage
                    .list_tasks(&project, true, 10, 0)
                    .expect("query must succeed")
                    .len(),
                1,
                "status {status}"
            );
            assert_eq!(
                storage.count_tasks(&project, true).expect("count"),
                1,
                "status {status}"
            );
        }
    }

    #[test]
    fn query_active_filter_excludes_terminal_rows() {
        let (_dir, storage) = open_query_storage("query-terminal");
        let project = query_project(QUERY_PROJECT_A);
        insert_query_task(
            &storage,
            query_task_id(1),
            &project,
            TaskStatus::Revising,
            TS_MID,
            TS_MID,
        );
        insert_query_task(
            &storage,
            query_task_id(2),
            &project,
            TaskStatus::Accepted,
            TS_EARLY,
            TS_EARLY,
        );
        insert_query_task(
            &storage,
            query_task_id(3),
            &project,
            TaskStatus::Closed,
            TS_EARLY,
            TS_EARLY,
        );

        let active = storage
            .get_active_task(&project)
            .expect("query must succeed")
            .expect("active task must be found");
        assert_eq!(active.task_id, query_task_id(1));

        let active_only = storage
            .list_tasks(&project, true, 10, 0)
            .expect("query must succeed");
        assert_eq!(active_only.len(), 1);
        assert_eq!(active_only[0].task_id, query_task_id(1));

        assert_eq!(
            storage
                .list_tasks(&project, false, 10, 0)
                .expect("query must succeed")
                .len(),
            3
        );
        assert_eq!(storage.count_tasks(&project, true).expect("count"), 1);
        assert_eq!(storage.count_tasks(&project, false).expect("count"), 3);
    }

    #[test]
    fn query_rejects_second_active_task_in_project() {
        let (_dir, storage) = open_query_storage("query-one-active");
        let project = query_project(QUERY_PROJECT_A);
        insert_query_task(
            &storage,
            query_task_id(1),
            &project,
            TaskStatus::Implementing,
            TS_EARLY,
            TS_EARLY,
        );

        let second = storage.connection().execute(
            "INSERT INTO tasks (task_id, project_id, workspace, status, task, allowed_paths, \
             test_commands, created_at, updated_at, revision_count) \
             VALUES (?1, ?2, '/fixture/workspace', 'revising', 't', '[]', '[]', ?3, ?3, 0)",
            rusqlite::params![query_task_id(2).to_string(), project.as_str(), TS_MID],
        );
        assert!(
            second.is_err(),
            "second active task must violate ux_tasks_active"
        );
        assert_eq!(storage.count_tasks(&project, true).expect("count"), 1);
    }

    #[test]
    fn query_list_order_is_deterministic() {
        let (_dir, storage) = open_query_storage("query-order");
        let project = query_project(QUERY_PROJECT_A);
        // 1 and 2 share updated_at and created_at: task_id DESC breaks the tie.
        insert_query_task(
            &storage,
            query_task_id(1),
            &project,
            TaskStatus::Accepted,
            TS_EARLY,
            TS_MID,
        );
        insert_query_task(
            &storage,
            query_task_id(2),
            &project,
            TaskStatus::Accepted,
            TS_EARLY,
            TS_MID,
        );
        // 3 shares updated_at but has a newer created_at: created_at DESC wins.
        insert_query_task(
            &storage,
            query_task_id(3),
            &project,
            TaskStatus::Accepted,
            TS_MID,
            TS_MID,
        );
        // 4 has the newest created_at but an older updated_at: updated_at DESC
        // dominates.
        insert_query_task(
            &storage,
            query_task_id(4),
            &project,
            TaskStatus::Accepted,
            TS_LATE,
            TS_EARLY,
        );

        let order: Vec<TaskId> = storage
            .list_tasks(&project, false, 10, 0)
            .expect("query must succeed")
            .iter()
            .map(|task| task.task_id)
            .collect();
        assert_eq!(
            order,
            vec![
                query_task_id(3),
                query_task_id(2),
                query_task_id(1),
                query_task_id(4)
            ]
        );
    }

    #[test]
    fn query_list_pagination_is_stable() {
        let (_dir, storage) = open_query_storage("query-pagination");
        let project = query_project(QUERY_PROJECT_A);
        for n in 1..=6_u32 {
            let updated = format!("2026-01-0{n}T00:00:00.000+00:00");
            insert_query_task(
                &storage,
                query_task_id(n),
                &project,
                TaskStatus::Accepted,
                TS_EARLY,
                &updated,
            );
        }

        let all: Vec<TaskId> = storage
            .list_tasks(&project, false, 10, 0)
            .expect("query must succeed")
            .iter()
            .map(|task| task.task_id)
            .collect();
        assert_eq!(all.len(), 6);

        let mut paged = Vec::new();
        for offset in [0_i64, 2, 4] {
            let page = storage
                .list_tasks(&project, false, 2, offset)
                .expect("query must succeed");
            assert_eq!(page.len(), 2, "offset {offset}");
            paged.extend(page.iter().map(|task| task.task_id));
        }
        assert_eq!(paged, all, "pages must reproduce the full order");

        let mut unique: Vec<String> = paged.iter().map(ToString::to_string).collect();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 6, "a task appeared on two pages");

        for offset in [6_i64, 7, 100] {
            assert!(
                storage
                    .list_tasks(&project, false, 2, offset)
                    .expect("query must succeed")
                    .is_empty(),
                "offset {offset} past the end must be empty"
            );
        }
    }

    #[test]
    fn query_rejects_invalid_limit_and_offset() {
        let (_dir, storage) = open_query_storage("query-invalid-window");
        let project = query_project(QUERY_PROJECT_A);

        assert!(matches!(
            storage.list_tasks(&project, false, 0, 0),
            Err(QueryError::InvalidLimit)
        ));
        assert!(matches!(
            storage.list_tasks(&project, false, -1, 0),
            Err(QueryError::InvalidLimit)
        ));
        assert!(matches!(
            storage.list_tasks(&project, false, 1, -1),
            Err(QueryError::InvalidOffset)
        ));
        assert!(
            storage
                .list_tasks(&project, false, 1, 0)
                .expect("valid window must succeed")
                .is_empty()
        );

        let error = storage
            .list_tasks(&project, false, 0, 0)
            .expect_err("invalid limit must fail");
        assert!(
            !error.to_string().contains(QUERY_PROJECT_A),
            "project leaked: {error}"
        );
        assert!(error.source().is_none());
    }

    #[test]
    fn query_count_matches_list_length() {
        let (_dir, storage) = open_query_storage("query-count-parity");
        let project_a = query_project(QUERY_PROJECT_A);
        let project_b = query_project(QUERY_PROJECT_B);
        insert_query_task(
            &storage,
            query_task_id(1),
            &project_a,
            TaskStatus::Implementing,
            TS_EARLY,
            TS_EARLY,
        );
        insert_query_task(
            &storage,
            query_task_id(2),
            &project_a,
            TaskStatus::Accepted,
            TS_MID,
            TS_MID,
        );
        insert_query_task(
            &storage,
            query_task_id(3),
            &project_b,
            TaskStatus::Revising,
            TS_LATE,
            TS_LATE,
        );
        insert_query_task(
            &storage,
            query_task_id(4),
            &project_b,
            TaskStatus::Closed,
            TS_EARLY,
            TS_EARLY,
        );

        for project in [&project_a, &project_b] {
            for active_only in [false, true] {
                let count = storage.count_tasks(project, active_only).expect("count");
                let listed = storage
                    .list_tasks(project, active_only, 100, 0)
                    .expect("list")
                    .len();
                assert_eq!(
                    usize::try_from(count).expect("count is non-negative"),
                    listed
                );
            }
        }
    }

    #[test]
    fn query_surfaces_corrupted_row_safely() {
        let (_dir, storage) = open_query_storage("query-corrupted");
        let project = query_project(QUERY_PROJECT_A);
        storage
            .connection()
            .execute(
                "INSERT INTO tasks (task_id, project_id, workspace, status, task, allowed_paths, \
                 test_commands, created_at, updated_at, revision_count) \
                 VALUES (?1, ?2, '/fixture/workspace', 'super-secret-status', 't', '[]', '[]', \
                 ?3, ?3, 0)",
                rusqlite::params![query_task_id(1).to_string(), project.as_str(), TS_EARLY],
            )
            .expect("insert corrupted row");

        let error = storage
            .get_task(query_task_id(1))
            .expect_err("corrupted row must fail");
        assert!(
            matches!(error, QueryError::TaskRow(TaskRowError::UnknownStatus)),
            "{error:?}"
        );
        let display = error.to_string();
        assert!(
            !display.contains("super-secret-status"),
            "status leaked: {display}"
        );
        assert!(
            !display.contains(QUERY_PROJECT_A),
            "project leaked: {display}"
        );
        assert!(error.source().is_some());

        assert!(matches!(
            storage.list_tasks(&project, false, 10, 0),
            Err(QueryError::TaskRow(TaskRowError::UnknownStatus))
        ));
        // Counting does not map rows, so it still succeeds.
        assert_eq!(storage.count_tasks(&project, false).expect("count"), 1);
    }

    #[test]
    fn query_does_not_write_or_change_schema() {
        let dir = TempDir::new("query-readonly");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let storage = connect(&path).expect("connect must succeed");
        let project = query_project(QUERY_PROJECT_A);
        insert_query_task(
            &storage,
            query_task_id(1),
            &project,
            TaskStatus::Implementing,
            TS_EARLY,
            TS_EARLY,
        );
        insert_query_task(
            &storage,
            query_task_id(2),
            &project,
            TaskStatus::Accepted,
            TS_MID,
            TS_MID,
        );

        let state_before = logical_state(&path);
        let rows_before: i64 = storage
            .connection()
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .expect("count rows");

        storage
            .get_task(query_task_id(1))
            .expect("get must succeed");
        storage
            .get_active_task(&project)
            .expect("active must succeed");
        storage
            .list_tasks(&project, true, 1, 0)
            .expect("active list must succeed");
        storage
            .list_tasks(&project, false, 10, 0)
            .expect("full list must succeed");
        storage
            .count_tasks(&project, false)
            .expect("count must succeed");

        assert_eq!(logical_state(&path), state_before, "schema changed");
        let rows_after: i64 = storage
            .connection()
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .expect("count rows");
        assert_eq!(rows_after, rows_before, "row count changed");
        assert_eq!(
            query_user_version(storage.connection()).expect("read user_version"),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn query_on_fixture_copy_rejects_synthetic_id_without_touching_fixture() {
        let source = fixture_dir().join("active-v6.sqlite");
        let before = std::fs::read(&source).expect("read fixture");
        let dir = TempDir::new("query-fixture-copy");
        let copy = dir.join("state.sqlite");
        std::fs::copy(&source, &copy).expect("copy fixture");

        let storage = connect(&copy).expect("connect copy");
        let project = query_project("proj");
        let error = storage
            .list_tasks(&project, false, 10, 0)
            .expect_err("synthetic task id must be rejected");
        assert!(
            matches!(error, QueryError::TaskRow(TaskRowError::InvalidTaskId)),
            "{error:?}"
        );

        assert_eq!(std::fs::read(&source).expect("re-read fixture"), before);
        assert!(
            !sidecar(&source, "-wal").exists(),
            "committed fixture got a -wal sidecar"
        );
        assert!(
            !sidecar(&source, "-shm").exists(),
            "committed fixture got a -shm sidecar"
        );
    }

    fn create_task_input(
        task_id: TaskId,
        project: &ProjectId,
        request_id: &str,
    ) -> CreateTaskInput {
        CreateTaskInput {
            task_id,
            project_id: project.clone(),
            workspace: "/fixture/workspace".to_owned(),
            task: "Implement the fixture change".to_owned(),
            request_id: request_id.to_owned(),
            payload_hash: format!("hash-{request_id}"),
            base_head: Some("1111111111111111111111111111111111111111".to_owned()),
            allowed_paths: vec!["module.py".to_owned()],
            test_commands: vec!["pytest -q".to_owned()],
            snapshot: Some(serde_json::json!({"head": "1111111111111111111111111111111111111111"})),
        }
    }

    fn count_rows(storage: &StorageConnection, table: &str) -> i64 {
        storage
            .connection()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count rows")
    }

    fn is_rfc3339_millis_utc(value: &str) -> bool {
        let bytes = value.as_bytes();
        bytes.len() == 29
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b'T'
            && bytes[13] == b':'
            && bytes[16] == b':'
            && bytes[19] == b'.'
            && bytes[23] == b'+'
            && bytes[26] == b':'
            && value.ends_with("+00:00")
            && value
                .get(20..23)
                .is_some_and(|millis| millis.chars().all(|c| c.is_ascii_digit()))
    }

    #[test]
    fn civil_from_days_matches_known_utc_dates() {
        assert_eq!(super::civil_from_days(0), (1970, 1, 1));
        assert_eq!(super::civil_from_days(365), (1971, 1, 1));
        assert_eq!(super::civil_from_days(10_957), (2000, 1, 1));
        assert_eq!(super::civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(super::civil_from_days(-1), (1969, 12, 31));
    }

    fn assert_create_error_is_safe(error: &CreateTaskError, secret: &str) {
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(!display.contains(secret), "Display leaked input: {display}");
        assert!(!debug.contains(secret), "Debug leaked input: {debug}");
    }

    #[test]
    fn create_task_writes_exactly_three_rows_with_python_defaults() {
        let (_dir, mut storage) = open_query_storage("create-happy");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        let input = create_task_input(task_id, &project, "req-1");

        let outcome = storage
            .create_task(input)
            .expect("create_task must succeed");
        assert!(outcome.is_created(), "fresh request must be Created");
        let task = outcome.into_task();

        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);

        let round = storage
            .connection()
            .query_row(
                &format!("SELECT {ROUND_ROW_COLUMNS} FROM rounds"),
                [],
                |row| Ok(RoundRow::from_row(row)),
            )
            .expect("round query must execute")
            .expect("round row must map");
        assert_eq!(round.round_number, 1);
        assert_eq!(round.kind, RoundKind::Implement);
        assert_eq!(round.status, RoundStatus::Pending);
        assert!(!round.attempted);
        assert_eq!(round.request_id, "req-1");
        assert_eq!(round.payload_hash, "hash-req-1");
        assert_eq!(round.outbound_message_id, None);
        assert_eq!(round.response_message_id, None);
        assert_eq!(round.response, None);
        assert_eq!(round.error_code, None);
        assert_eq!(round.result_json, None);
        assert_eq!(round.findings, None);
        assert_eq!(round.session_id, None);
        assert_eq!(round.worker_started_at, None);
        assert_eq!(round.worker_deadline_at, None);
        assert_eq!(round.verifier_state, None);
        assert_eq!(round.verifier_json, None);

        assert_eq!(task.status, TaskStatus::Implementing);
        assert_eq!(task.revision_count, 0);
        assert_eq!(task.session_id, None);
        assert_eq!(task.close_requested_at, None);
        assert_eq!(task.close_reason, None);
        assert_eq!(task.allowed_paths, ["module.py"]);
        assert_eq!(task.test_commands, ["pytest -q"]);
        assert_eq!(
            task.snapshot,
            Some(serde_json::json!({"head": "1111111111111111111111111111111111111111"}))
        );

        let (kind, message, round_number, event_created): (String, String, Option<i64>, String) =
            storage
                .connection()
                .query_row(
                    "SELECT kind, message, round_number, created_at FROM events",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("event row");
        assert_eq!(kind, "created");
        assert_eq!(message, "task created (implement)");
        assert_eq!(round_number, Some(1));
        assert_eq!(event_created, task.created_at);

        assert_eq!(task.created_at, task.updated_at);
        assert_eq!(round.created_at, round.updated_at);
        assert_eq!(round.created_at, task.created_at);
        assert!(
            is_rfc3339_millis_utc(&task.created_at),
            "unexpected timestamp {}",
            task.created_at
        );
    }

    #[test]
    fn create_task_handles_snapshot_none_object_and_rejects_non_object() {
        let (_dir, mut storage) = open_query_storage("create-snapshot");
        let project_a = query_project(QUERY_PROJECT_A);
        let project_b = query_project(QUERY_PROJECT_B);
        let project_c = query_project("query-project-c");

        let mut input = create_task_input(query_task_id(1), &project_a, "req-none");
        input.snapshot = None;
        storage
            .create_task(input)
            .expect("snapshot None must succeed");
        let raw: Option<String> = storage
            .connection()
            .query_row(
                "SELECT snapshot FROM tasks WHERE task_id = ?1",
                rusqlite::params![query_task_id(1).to_string()],
                |row| row.get(0),
            )
            .expect("snapshot");
        assert_eq!(raw, None);

        let mut input = create_task_input(query_task_id(2), &project_b, "req-object");
        input.snapshot = Some(serde_json::json!({"head": "abc", "nested": {"x": 1}}));
        storage
            .create_task(input)
            .expect("snapshot object must succeed");
        let raw: Option<String> = storage
            .connection()
            .query_row(
                "SELECT snapshot FROM tasks WHERE task_id = ?1",
                rusqlite::params![query_task_id(2).to_string()],
                |row| row.get(0),
            )
            .expect("snapshot");
        let parsed: Value = serde_json::from_str(raw.as_deref().expect("object stored"))
            .expect("stored snapshot must be valid JSON");
        assert_eq!(
            parsed,
            serde_json::json!({"head": "abc", "nested": {"x": 1}})
        );

        for bad in [
            serde_json::json!([]),
            serde_json::json!("text"),
            serde_json::json!(1),
            serde_json::json!(true),
            serde_json::json!(null),
        ] {
            let mut input = create_task_input(query_task_id(3), &project_c, "req-bad");
            input.snapshot = Some(bad.clone());
            let error = storage
                .create_task(input)
                .expect_err("non-object snapshot must fail");
            assert!(
                matches!(error, CreateTaskError::InvalidInput),
                "snapshot {bad}: {error:?}"
            );
        }

        assert_eq!(count_rows(&storage, "tasks"), 2);
        assert_eq!(count_rows(&storage, "rounds"), 2);
        assert_eq!(count_rows(&storage, "events"), 2);
    }

    #[test]
    fn create_task_rejects_invalid_required_text_before_writing() {
        let (_dir, mut storage) = open_query_storage("create-invalid-text");
        let project = query_project(QUERY_PROJECT_A);

        let mut empty_workspace = create_task_input(query_task_id(1), &project, "req-1");
        empty_workspace.workspace = String::new();
        let mut blank_task = create_task_input(query_task_id(1), &project, "req-1");
        blank_task.task = "   ".to_owned();
        let mut empty_request = create_task_input(query_task_id(1), &project, "req-1");
        empty_request.request_id = String::new();
        let mut empty_hash = create_task_input(query_task_id(1), &project, "req-1");
        empty_hash.payload_hash = String::new();

        for input in [empty_workspace, blank_task, empty_request, empty_hash] {
            let error = storage
                .create_task(input)
                .expect_err("invalid text must fail");
            assert!(matches!(error, CreateTaskError::InvalidInput), "{error:?}");
        }

        assert_eq!(count_rows(&storage, "tasks"), 0);
        assert_eq!(count_rows(&storage, "rounds"), 0);
        assert_eq!(count_rows(&storage, "events"), 0);
    }

    #[test]
    fn create_task_duplicate_task_id_is_task_id_conflict() {
        let (_dir, mut storage) = open_query_storage("create-task-id-conflict");
        let project = query_project(QUERY_PROJECT_A);
        storage
            .create_task(create_task_input(query_task_id(1), &project, "req-1"))
            .expect("first create must succeed");

        let other = query_project(QUERY_PROJECT_B);
        let error = storage
            .create_task(create_task_input(query_task_id(1), &other, "req-2"))
            .expect_err("duplicate task id must fail");
        assert!(
            matches!(error, CreateTaskError::TaskIdConflict),
            "{error:?}"
        );

        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);
    }

    #[test]
    fn create_task_active_project_is_busy() {
        let (_dir, mut storage) = open_query_storage("create-project-busy");
        let project = query_project(QUERY_PROJECT_A);
        storage
            .create_task(create_task_input(query_task_id(1), &project, "req-1"))
            .expect("first create must succeed");

        let error = storage
            .create_task(create_task_input(query_task_id(2), &project, "req-2"))
            .expect_err("second active task must fail");
        assert!(matches!(error, CreateTaskError::ProjectBusy), "{error:?}");

        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);
    }

    #[test]
    fn create_task_same_project_request_conflict_rolls_back_new_task() {
        let (_dir, mut storage) = open_query_storage("create-request-conflict");
        let project = query_project(QUERY_PROJECT_A);
        let connection = storage.connection();
        connection
            .execute(
                "INSERT INTO tasks (task_id, project_id, workspace, status, task, allowed_paths, \
                 test_commands, created_at, updated_at, revision_count) \
                 VALUES (?1, ?2, '/fixture/workspace', 'accepted', 't', '[]', '[]', ?3, ?3, 0)",
                rusqlite::params![query_task_id(9).to_string(), project.as_str(), TS_EARLY],
            )
            .expect("seed terminal task");
        connection
            .execute(
                "INSERT INTO rounds (task_id, project_id, round_number, request_id, payload_hash, \
                 kind, status, attempted, created_at, updated_at) \
                 VALUES (?1, ?2, 1, 'req-dup', 'hash', 'implement', 'complete', 0, ?3, ?3)",
                rusqlite::params![query_task_id(9).to_string(), project.as_str(), TS_EARLY],
            )
            .expect("seed round");

        let error = storage
            .create_task(create_task_input(query_task_id(1), &project, "req-dup"))
            .expect_err("request conflict must fail");
        assert!(
            matches!(error, CreateTaskError::RequestConflict),
            "{error:?}"
        );

        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 0);
        let new_task: i64 = storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE task_id = ?1",
                rusqlite::params![query_task_id(1).to_string()],
                |row| row.get(0),
            )
            .expect("count new task");
        assert_eq!(new_task, 0, "the newly inserted task survived the rollback");
    }

    #[test]
    fn create_task_same_request_across_projects_succeeds() {
        let (_dir, mut storage) = open_query_storage("create-request-cross-project");
        let project_a = query_project(QUERY_PROJECT_A);
        let project_b = query_project(QUERY_PROJECT_B);
        storage
            .create_task(create_task_input(
                query_task_id(1),
                &project_a,
                "req-shared",
            ))
            .expect("project a create must succeed");
        storage
            .create_task(create_task_input(
                query_task_id(2),
                &project_b,
                "req-shared",
            ))
            .expect("project b create must succeed");

        let shared: i64 = storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM rounds WHERE request_id = 'req-shared'",
                [],
                |row| row.get(0),
            )
            .expect("count shared request");
        assert_eq!(shared, 2);
    }

    #[test]
    fn create_task_forced_post_task_failure_rolls_back_every_row() {
        let dir = TempDir::new("create-forced-rollback");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        execute(&path, "DROP TABLE events");
        let mut storage = connect(&path).expect("connect must succeed");

        let project = query_project(QUERY_PROJECT_A);
        let error = storage
            .create_task(create_task_input(query_task_id(1), &project, "req-1"))
            .expect_err("event insert must fail");
        assert!(matches!(error, CreateTaskError::Database(_)), "{error:?}");

        assert_eq!(count_rows(&storage, "tasks"), 0);
        assert_eq!(count_rows(&storage, "rounds"), 0);
    }

    #[test]
    fn create_task_concurrent_different_ids_same_project_yields_one_success_one_busy() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("create-concurrent");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        // Pre-create the WAL database so the threads contend only on the writer
        // transaction, not on the journal-mode switch.
        drop(connect(&path).expect("pre-create WAL database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(2));
        let project = query_project(QUERY_PROJECT_A);
        let mut handles = Vec::new();
        for n in 1..=2_u32 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let project = project.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut storage = connect(path.as_path()).expect("connect must succeed");
                storage.create_task(create_task_input(
                    query_task_id(n),
                    &project,
                    &format!("req-{n}"),
                ))
            }));
        }

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread must not panic"))
            .collect();
        let successes = results.iter().filter(|result| result.is_ok()).count();
        let busy = results
            .iter()
            .filter(|result| matches!(result, Err(CreateTaskError::ProjectBusy)))
            .count();
        assert_eq!(successes, 1, "{results:?}");
        assert_eq!(busy, 1, "{results:?}");

        let storage = connect(path.as_path()).expect("connect verify");
        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);
    }

    #[test]
    fn create_task_returns_task_and_persisted_round_row_maps() {
        let (_dir, mut storage) = open_query_storage("create-mapping");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        let mut input = create_task_input(task_id, &project, "req-map");
        input.base_head = Some("2222222222222222222222222222222222222222".to_owned());
        input.snapshot = Some(serde_json::json!({"head": "abc"}));

        let outcome = storage
            .create_task(input)
            .expect("create_task must succeed");
        assert!(outcome.is_created(), "fresh request must be Created");
        let task = outcome.into_task();

        assert_eq!(task.task_id, task_id);
        assert_eq!(task.project_id, project);
        assert_eq!(task.workspace, "/fixture/workspace");
        assert_eq!(task.status, TaskStatus::Implementing);
        assert_eq!(task.session_id, None);
        assert_eq!(task.text, "Implement the fixture change");
        assert_eq!(task.allowed_paths, ["module.py"]);
        assert_eq!(task.test_commands, ["pytest -q"]);
        assert_eq!(
            task.base_head.as_deref(),
            Some("2222222222222222222222222222222222222222")
        );
        assert_eq!(task.snapshot, Some(serde_json::json!({"head": "abc"})));
        assert_eq!(task.revision_count, 0);
        assert_eq!(task.close_requested_at, None);
        assert_eq!(task.close_reason, None);

        let fetched = storage
            .get_task(task_id)
            .expect("get must succeed")
            .expect("created task must be present");
        assert_eq!(fetched, task);

        let round = storage
            .connection()
            .query_row(
                &format!("SELECT {ROUND_ROW_COLUMNS} FROM rounds"),
                [],
                |row| Ok(RoundRow::from_row(row)),
            )
            .expect("round query must execute")
            .expect("round row must map");
        assert_eq!(round.task_id, task_id);
        assert_eq!(round.project_id, project);
        assert_eq!(round.round_number, 1);
        assert_eq!(round.kind, RoundKind::Implement);
        assert_eq!(round.status, RoundStatus::Pending);
        assert_eq!(round.created_at, task.created_at);
        assert_eq!(round.updated_at, task.updated_at);
    }

    #[test]
    fn create_task_errors_do_not_leak_input() {
        const SECRET: &str = "super-secret-token";
        let (_dir, mut storage) = open_query_storage("create-error-safety");
        let project = query_project(QUERY_PROJECT_A);

        let mut invalid = create_task_input(query_task_id(1), &project, "req-1");
        invalid.snapshot = Some(serde_json::json!([SECRET]));
        let error = storage
            .create_task(invalid)
            .expect_err("non-object snapshot must fail");
        assert!(matches!(error, CreateTaskError::InvalidInput), "{error:?}");
        assert_create_error_is_safe(&error, SECRET);

        let mut first = create_task_input(query_task_id(2), &project, "req-2");
        first.workspace = format!("/{SECRET}");
        first.task = SECRET.to_owned();
        storage
            .create_task(first)
            .expect("first create must succeed");
        let mut second = create_task_input(query_task_id(3), &project, "req-3");
        second.request_id = SECRET.to_owned();
        let error = storage
            .create_task(second)
            .expect_err("project busy must fail");
        assert!(matches!(error, CreateTaskError::ProjectBusy), "{error:?}");
        assert_create_error_is_safe(&error, SECRET);

        let other = query_project(QUERY_PROJECT_B);
        let mut duplicate = create_task_input(query_task_id(2), &other, "req-4");
        duplicate.payload_hash = SECRET.to_owned();
        let error = storage
            .create_task(duplicate)
            .expect_err("duplicate task id must fail");
        assert!(
            matches!(error, CreateTaskError::TaskIdConflict),
            "{error:?}"
        );
        assert_create_error_is_safe(&error, SECRET);
    }

    /// Seeds one `tasks` row with a raw connection (foreign keys off) so tests
    /// can build states the runtime connection would refuse to write.
    fn seed_task_raw(path: &Path, task_id: &str, project_id: &str, status: &str) {
        let connection = Connection::open(path).expect("open seed database");
        connection
            .execute(
                "INSERT INTO tasks (task_id, project_id, workspace, status, task, allowed_paths, \
                 test_commands, created_at, updated_at, revision_count) \
                 VALUES (?1, ?2, '/fixture/workspace', ?3, 't', '[]', '[]', ?4, ?4, 0)",
                rusqlite::params![task_id, project_id, status, TS_EARLY],
            )
            .expect("seed task");
    }

    /// Seeds one `rounds` row with a raw connection (foreign keys off) so tests
    /// can build a missing-task or otherwise corrupt replay candidate.
    #[allow(clippy::too_many_arguments)]
    fn seed_round_raw(
        path: &Path,
        task_id: &str,
        project_id: &str,
        round_number: i64,
        request_id: &str,
        payload_hash: &str,
        kind: &str,
    ) {
        let connection = Connection::open(path).expect("open seed database");
        connection
            .execute_batch("PRAGMA foreign_keys=OFF;")
            .expect("disable foreign keys for seeding");
        connection
            .execute(
                "INSERT INTO rounds (task_id, project_id, round_number, request_id, payload_hash, \
                 kind, status, attempted, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7, ?7)",
                rusqlite::params![
                    task_id,
                    project_id,
                    round_number,
                    request_id,
                    payload_hash,
                    kind,
                    TS_EARLY
                ],
            )
            .expect("seed round");
    }

    /// Dumps every column of every row of `table` in physical order so a test
    /// can prove that a replay mutated nothing.
    fn dump_rows(storage: &StorageConnection, table: &str) -> Vec<Vec<SqlValue>> {
        let mut statement = storage
            .connection()
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .expect("prepare dump");
        let column_count = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                let mut values = Vec::with_capacity(column_count);
                for index in 0..column_count {
                    values.push(row.get::<_, SqlValue>(index)?);
                }
                Ok(values)
            })
            .expect("query dump");
        rows.map(|row| row.expect("dump row")).collect()
    }

    #[test]
    fn create_task_replay_returns_original_task_without_mutation() {
        let (_dir, mut storage) = open_query_storage("replay-equal");
        let project = query_project(QUERY_PROJECT_A);
        let original_id = query_task_id(1);

        let created = storage
            .create_task(create_task_input(original_id, &project, "req-replay"))
            .expect("first create must succeed");
        assert!(created.is_created(), "{created:?}");
        let original = created.into_task();

        let tasks_before = dump_rows(&storage, "tasks");
        let rounds_before = dump_rows(&storage, "rounds");
        let events_before = dump_rows(&storage, "events");

        let mut replay = create_task_input(query_task_id(2), &project, "req-replay");
        replay.workspace = "/other/workspace".to_owned();
        replay.task = "different task text".to_owned();
        replay.allowed_paths = vec!["other.py".to_owned()];
        replay.test_commands = vec!["cargo test".to_owned()];
        replay.base_head = Some("2222222222222222222222222222222222222222".to_owned());
        replay.snapshot = Some(serde_json::json!({"head": "different"}));

        let replayed = storage.create_task(replay).expect("replay must succeed");
        assert!(replayed.is_replayed(), "{replayed:?}");
        let replayed_task = replayed.into_task();

        assert_eq!(replayed_task, original);
        assert_eq!(replayed_task.task_id, original_id);
        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);
        assert_eq!(dump_rows(&storage, "tasks"), tasks_before);
        assert_eq!(dump_rows(&storage, "rounds"), rounds_before);
        assert_eq!(dump_rows(&storage, "events"), events_before);
    }

    #[test]
    fn create_task_replay_different_payload_hash_is_request_conflict() {
        let (_dir, mut storage) = open_query_storage("replay-hash-conflict");
        let project = query_project(QUERY_PROJECT_A);
        storage
            .create_task(create_task_input(query_task_id(1), &project, "req-x"))
            .expect("first create must succeed");

        let mut second = create_task_input(query_task_id(2), &project, "req-x");
        second.payload_hash = "hash-other".to_owned();
        let error = storage
            .create_task(second)
            .expect_err("different hash must conflict");
        assert!(
            matches!(error, CreateTaskError::RequestConflict),
            "{error:?}"
        );

        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);
    }

    #[test]
    fn create_task_replay_same_request_wins_over_project_busy() {
        let (_dir, mut storage) = open_query_storage("replay-before-busy");
        let project = query_project(QUERY_PROJECT_A);
        storage
            .create_task(create_task_input(query_task_id(1), &project, "req-1"))
            .expect("first create must succeed");

        let outcome = storage
            .create_task(create_task_input(query_task_id(2), &project, "req-1"))
            .expect("same request must replay before ProjectBusy");
        assert!(outcome.is_replayed(), "{outcome:?}");
        assert_eq!(outcome.task().task_id, query_task_id(1));

        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);
    }

    #[test]
    fn create_task_replay_revision_kind_is_request_conflict() {
        let dir = TempDir::new("replay-revise");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        seed_task_raw(
            &path,
            &query_task_id(9).to_string(),
            project.as_str(),
            "accepted",
        );
        seed_round_raw(
            &path,
            &query_task_id(9).to_string(),
            project.as_str(),
            1,
            "req-rev",
            "hash-req-rev",
            "revise",
        );
        let mut storage = connect(&path).expect("connect must succeed");

        let error = storage
            .create_task(create_task_input(query_task_id(1), &project, "req-rev"))
            .expect_err("revision-kind request must conflict");
        assert!(
            matches!(error, CreateTaskError::RequestConflict),
            "{error:?}"
        );
        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 0);
    }

    #[test]
    fn create_task_replay_unmappable_round_is_invalid_persisted_state() {
        let dir = TempDir::new("replay-bad-round");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        seed_round_raw(
            &path,
            &query_task_id(5).to_string(),
            project.as_str(),
            1,
            "req-badround",
            "hash-req-badround",
            "bogus",
        );
        let mut storage = connect(&path).expect("connect must succeed");

        let error = storage
            .create_task(create_task_input(
                query_task_id(1),
                &project,
                "req-badround",
            ))
            .expect_err("unmappable round must fail closed");
        assert!(
            matches!(
                error,
                CreateTaskError::InvalidPersistedState(ReplayStateError::RoundRow(_))
            ),
            "{error:?}"
        );
        assert_eq!(count_rows(&storage, "tasks"), 0);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 0);
    }

    #[test]
    fn create_task_replay_non_initial_implement_round_is_invalid_persisted_state() {
        let dir = TempDir::new("replay-non-initial");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        seed_task_raw(
            &path,
            &query_task_id(6).to_string(),
            project.as_str(),
            "accepted",
        );
        seed_round_raw(
            &path,
            &query_task_id(6).to_string(),
            project.as_str(),
            2,
            "req-non1",
            "hash-req-non1",
            "implement",
        );
        let mut storage = connect(&path).expect("connect must succeed");

        let error = storage
            .create_task(create_task_input(query_task_id(1), &project, "req-non1"))
            .expect_err("non-initial implement round must fail closed");
        assert!(
            matches!(
                error,
                CreateTaskError::InvalidPersistedState(ReplayStateError::InvalidRoundNumber)
            ),
            "{error:?}"
        );
        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 0);
    }

    #[test]
    fn create_task_replay_missing_linked_task_is_invalid_persisted_state() {
        let dir = TempDir::new("replay-missing-task");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        seed_round_raw(
            &path,
            &query_task_id(7).to_string(),
            project.as_str(),
            1,
            "req-missing",
            "hash-req-missing",
            "implement",
        );
        let mut storage = connect(&path).expect("connect must succeed");

        let error = storage
            .create_task(create_task_input(query_task_id(1), &project, "req-missing"))
            .expect_err("missing linked task must fail closed");
        assert!(
            matches!(
                error,
                CreateTaskError::InvalidPersistedState(ReplayStateError::MissingTask)
            ),
            "{error:?}"
        );
        assert_eq!(count_rows(&storage, "tasks"), 0);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 0);
    }

    #[test]
    fn create_task_replay_unmappable_task_is_invalid_persisted_state() {
        let dir = TempDir::new("replay-bad-task");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        seed_task_raw(
            &path,
            &query_task_id(8).to_string(),
            project.as_str(),
            "bogus-status",
        );
        seed_round_raw(
            &path,
            &query_task_id(8).to_string(),
            project.as_str(),
            1,
            "req-maperr",
            "hash-req-maperr",
            "implement",
        );
        let mut storage = connect(&path).expect("connect must succeed");

        let error = storage
            .create_task(create_task_input(query_task_id(1), &project, "req-maperr"))
            .expect_err("unmappable linked task must fail closed");
        assert!(
            matches!(
                error,
                CreateTaskError::InvalidPersistedState(ReplayStateError::TaskRow(_))
            ),
            "{error:?}"
        );
        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 0);
    }

    #[test]
    fn create_task_replay_task_project_mismatch_is_invalid_persisted_state() {
        let dir = TempDir::new("replay-project-mismatch");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project_a = query_project(QUERY_PROJECT_A);
        let project_b = query_project(QUERY_PROJECT_B);
        seed_task_raw(
            &path,
            &query_task_id(9).to_string(),
            project_b.as_str(),
            "accepted",
        );
        seed_round_raw(
            &path,
            &query_task_id(9).to_string(),
            project_a.as_str(),
            1,
            "req-mismatch",
            "hash-req-mismatch",
            "implement",
        );
        let mut storage = connect(&path).expect("connect must succeed");

        let error = storage
            .create_task(create_task_input(
                query_task_id(1),
                &project_a,
                "req-mismatch",
            ))
            .expect_err("project mismatch must fail closed");
        assert!(
            matches!(
                error,
                CreateTaskError::InvalidPersistedState(ReplayStateError::ProjectMismatch)
            ),
            "{error:?}"
        );
        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 0);
    }

    #[test]
    fn create_task_replay_corrupt_errors_do_not_leak_input() {
        const SECRET: &str = "super-secret-token";
        let dir = TempDir::new("replay-error-safety");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        seed_round_raw(
            &path,
            &query_task_id(5).to_string(),
            project.as_str(),
            1,
            SECRET,
            SECRET,
            "bogus",
        );
        let mut storage = connect(&path).expect("connect must succeed");

        let mut input = create_task_input(query_task_id(1), &project, SECRET);
        input.payload_hash = SECRET.to_owned();
        let error = storage
            .create_task(input)
            .expect_err("corrupt replay must fail closed");
        assert!(
            matches!(error, CreateTaskError::InvalidPersistedState(_)),
            "{error:?}"
        );
        assert_create_error_is_safe(&error, SECRET);
    }

    #[test]
    fn create_task_same_request_across_projects_replays_independently() {
        let (_dir, mut storage) = open_query_storage("replay-cross-project");
        let project_a = query_project(QUERY_PROJECT_A);
        let project_b = query_project(QUERY_PROJECT_B);

        let first_a = storage
            .create_task(create_task_input(
                query_task_id(1),
                &project_a,
                "req-shared",
            ))
            .expect("project a create must succeed");
        assert!(first_a.is_created());
        let first_b = storage
            .create_task(create_task_input(
                query_task_id(2),
                &project_b,
                "req-shared",
            ))
            .expect("project b create must succeed");
        assert!(first_b.is_created());

        let replay_a = storage
            .create_task(create_task_input(
                query_task_id(3),
                &project_a,
                "req-shared",
            ))
            .expect("project a replay must succeed");
        assert!(replay_a.is_replayed());
        assert_eq!(replay_a.task().task_id, query_task_id(1));
        let replay_b = storage
            .create_task(create_task_input(
                query_task_id(4),
                &project_b,
                "req-shared",
            ))
            .expect("project b replay must succeed");
        assert!(replay_b.is_replayed());
        assert_eq!(replay_b.task().task_id, query_task_id(2));

        assert_eq!(count_rows(&storage, "tasks"), 2);
        assert_eq!(count_rows(&storage, "rounds"), 2);
        assert_eq!(count_rows(&storage, "events"), 2);
    }

    #[test]
    fn create_task_concurrent_same_request_same_hash_yields_one_created_one_replayed() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("replay-concurrent-same");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        drop(connect(&path).expect("pre-create WAL database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(2));
        let project = query_project(QUERY_PROJECT_A);
        let mut handles = Vec::new();
        for n in 1..=2_u32 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let project = project.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut storage = connect(path.as_path()).expect("connect must succeed");
                storage.create_task(create_task_input(
                    query_task_id(n),
                    &project,
                    "req-concurrent",
                ))
            }));
        }

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread must not panic"))
            .collect();
        let created = results
            .iter()
            .filter(|result| matches!(result, Ok(CreateTaskOutcome::Created(_))))
            .count();
        let replayed = results
            .iter()
            .filter(|result| matches!(result, Ok(CreateTaskOutcome::Replayed(_))))
            .count();
        assert_eq!(created, 1, "{results:?}");
        assert_eq!(replayed, 1, "{results:?}");
        let ids: Vec<TaskId> = results
            .iter()
            .map(|result| {
                result
                    .as_ref()
                    .expect("both calls must succeed")
                    .task()
                    .task_id
            })
            .collect();
        assert_eq!(ids[0], ids[1], "{results:?}");

        let storage = connect(path.as_path()).expect("connect verify");
        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);
    }

    #[test]
    fn create_task_concurrent_same_request_different_hash_yields_one_created_one_conflict() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("replay-concurrent-hash");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        drop(connect(&path).expect("pre-create WAL database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(2));
        let project = query_project(QUERY_PROJECT_A);
        let mut handles = Vec::new();
        for n in 1..=2_u32 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let project = project.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut storage = connect(path.as_path()).expect("connect must succeed");
                let mut input = create_task_input(query_task_id(n), &project, "req-race");
                input.payload_hash = format!("hash-{n}");
                storage.create_task(input)
            }));
        }

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread must not panic"))
            .collect();
        let created = results
            .iter()
            .filter(|result| matches!(result, Ok(CreateTaskOutcome::Created(_))))
            .count();
        let conflicts = results
            .iter()
            .filter(|result| matches!(result, Err(CreateTaskError::RequestConflict)))
            .count();
        assert_eq!(created, 1, "{results:?}");
        assert_eq!(conflicts, 1, "{results:?}");

        let storage = connect(path.as_path()).expect("connect verify");
        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bridge-storage-test-{}-{tag}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn round_ref(task_id: TaskId, project: &ProjectId, round_number: u32) -> RoundRef {
        RoundRef {
            task_id,
            project_id: project.clone(),
            round_number,
        }
    }

    fn fetch_round(storage: &StorageConnection, task_id: TaskId, round_number: u32) -> RoundRow {
        storage
            .connection()
            .query_row(
                &format!(
                    "SELECT {ROUND_ROW_COLUMNS} FROM rounds WHERE task_id = ?1 AND round_number = ?2"
                ),
                rusqlite::params![task_id.to_string(), i64::from(round_number)],
                |row| Ok(RoundRow::from_row(row)),
            )
            .expect("round query must execute")
            .expect("round row must map")
    }

    fn fetch_task(storage: &StorageConnection, task_id: TaskId) -> Task {
        storage
            .get_task(task_id)
            .expect("get_task must succeed")
            .expect("task must be present")
    }

    fn event_rows(
        storage: &StorageConnection,
        task_id: TaskId,
    ) -> Vec<(Option<i64>, String, String)> {
        let mut statement = storage
            .connection()
            .prepare(
                "SELECT round_number, kind, message FROM events WHERE task_id = ?1 ORDER BY id",
            )
            .expect("prepare events");
        let rows = statement
            .query_map(rusqlite::params![task_id.to_string()], |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("query events");
        let mut events = Vec::new();
        for row in rows {
            events.push(row.expect("event row"));
        }
        events
    }

    /// Creates a task and drives its initial round to `awaiting_review` so
    /// revision-round tests start from the only status that may create one.
    fn setup_awaiting_review(
        storage: &mut StorageConnection,
        task_id: TaskId,
        project: &ProjectId,
    ) {
        storage
            .create_task(create_task_input(
                task_id,
                project,
                &format!("setup-{task_id}"),
            ))
            .expect("setup create task");
        storage
            .mark_round_observing(round_ref(task_id, project, 1))
            .expect("setup observing");
        storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_id, project, 1),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect("setup finish");
    }

    /// Drives the current round of an existing task to `failed` with `error_code`
    /// so reopen tests start from a known eligible or non-eligible state.
    fn fail_current_round(
        storage: &mut StorageConnection,
        task_id: TaskId,
        project: &ProjectId,
        round_number: u32,
        error_code: &str,
    ) {
        storage
            .mark_round_observing(round_ref(task_id, project, round_number))
            .expect("setup observing");
        storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_id, project, round_number),
                round_status: RoundStatus::Failed,
                task_status: TaskStatus::Failed,
                response_message_id: None,
                response: Some("boom".to_owned()),
                error_code: Some(error_code.to_owned()),
                result_json: None,
            })
            .expect("setup fail");
    }

    fn assert_round_update_error_is_safe(error: &RoundUpdateError, secret: &str) {
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(!display.contains(secret), "Display leaked input: {display}");
        assert!(!debug.contains(secret), "Debug leaked input: {debug}");
    }

    #[test]
    fn round_transition_table_matches_contract_for_paths_used() {
        assert_eq!(ROUND_TRANSITIONS.len(), 15);
        let mut seen = Vec::new();
        for pair in ROUND_TRANSITIONS {
            assert!(RoundStatus::ALL.contains(&pair.0) && RoundStatus::ALL.contains(&pair.1));
            assert!(!seen.contains(&pair), "duplicate transition {pair:?}");
            seen.push(pair);
        }

        const ALLOWED: [(RoundStatus, RoundStatus); 15] = [
            (RoundStatus::Pending, RoundStatus::Sent),
            (RoundStatus::Pending, RoundStatus::Observing),
            (RoundStatus::Sent, RoundStatus::Observing),
            (RoundStatus::Observing, RoundStatus::Complete),
            (RoundStatus::Observing, RoundStatus::Failed),
            (RoundStatus::Observing, RoundStatus::NeedsUser),
            (RoundStatus::Observing, RoundStatus::DeliveryUnknown),
            (RoundStatus::NeedsUser, RoundStatus::Complete),
            (RoundStatus::NeedsUser, RoundStatus::Failed),
            (RoundStatus::NeedsUser, RoundStatus::NeedsUser),
            (RoundStatus::NeedsUser, RoundStatus::DeliveryUnknown),
            (RoundStatus::DeliveryUnknown, RoundStatus::Complete),
            (RoundStatus::DeliveryUnknown, RoundStatus::Failed),
            (RoundStatus::DeliveryUnknown, RoundStatus::NeedsUser),
            (RoundStatus::DeliveryUnknown, RoundStatus::DeliveryUnknown),
        ];
        for from in RoundStatus::ALL {
            for to in RoundStatus::ALL {
                assert_eq!(
                    round_transition_allowed(from, to),
                    ALLOWED.contains(&(from, to)),
                    "unexpected decision for {from} -> {to}"
                );
            }
        }
        // The recovery transition must not leak into the shared table.
        assert!(!round_transition_allowed(
            RoundStatus::Failed,
            RoundStatus::Observing
        ));
        assert!(!round_transition_allowed(
            RoundStatus::Complete,
            RoundStatus::Complete
        ));
        assert!(!round_transition_allowed(
            RoundStatus::Pending,
            RoundStatus::Complete
        ));
        assert!(!round_transition_allowed(
            RoundStatus::Sent,
            RoundStatus::Complete
        ));
    }

    #[test]
    fn round_lifecycle_full_delivery_flow_is_atomic() {
        let (_dir, mut storage) = open_query_storage("round-flow");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-flow"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);

        let started = storage
            .mark_worker_started(round.clone(), 30.0)
            .expect("worker started");
        assert_eq!(started.round.status, RoundStatus::Pending);
        assert!(started.round.worker_started_at.is_some());
        assert!(started.round.worker_deadline_at.is_some());
        assert!(started.round.worker_deadline_at > started.round.worker_started_at);
        assert_eq!(
            started.round.updated_at,
            started.round.worker_started_at.clone().expect("start")
        );

        let prepared = storage
            .prepare_round(round.clone(), "msg-1".to_owned())
            .expect("prepare");
        assert_eq!(prepared.round.status, RoundStatus::Pending);
        assert_eq!(prepared.round.outbound_message_id.as_deref(), Some("msg-1"));
        assert!(!prepared.round.attempted);

        let sent = storage.mark_round_sent(round.clone()).expect("sent");
        assert_eq!(sent.round.status, RoundStatus::Sent);
        assert!(sent.round.attempted);

        let observing = storage
            .mark_round_observing(round.clone())
            .expect("observing");
        assert_eq!(observing.round.status, RoundStatus::Observing);
        assert!(observing.round.attempted, "attempted must be preserved");

        let finished = storage
            .finish_round(FinishRoundInput {
                round: round.clone(),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: Some("msg-2".to_owned()),
                response: Some("done".to_owned()),
                error_code: None,
                result_json: Some(serde_json::json!({"changed_paths": []})),
            })
            .expect("finish");
        assert_eq!(finished.round.status, RoundStatus::Complete);
        assert_eq!(finished.round.response_message_id.as_deref(), Some("msg-2"));
        assert_eq!(finished.round.response.as_deref(), Some("done"));
        assert_eq!(
            finished.round.result_json,
            Some(serde_json::json!({"changed_paths": []}))
        );
        assert_eq!(finished.task.status, TaskStatus::AwaitingReview);
        assert_eq!(finished.round.updated_at, finished.task.updated_at);

        let events = event_rows(&storage, task_id);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, "created");
        assert_eq!(events[1].1, "complete");
        assert_eq!(events[1].0, Some(1));
    }

    #[test]
    fn round_pending_to_observing_without_send_preserves_attempted() {
        let (_dir, mut storage) = open_query_storage("round-observing");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-obs"))
            .expect("create");
        let observing = storage
            .mark_round_observing(round_ref(task_id, &project, 1))
            .expect("pending to observing");
        assert_eq!(observing.round.status, RoundStatus::Observing);
        assert!(!observing.round.attempted);
        assert_eq!(observing.round.outbound_message_id, None);
    }

    #[test]
    fn create_revision_round_sets_exact_fields_and_clears_session() {
        let (_dir, mut storage) = open_query_storage("round-revision");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        setup_awaiting_review(&mut storage, task_id, &project);
        storage
            .bind_round_session(round_ref(task_id, &project, 1), "ses-1".to_owned())
            .expect("bind");

        let outcome = storage
            .create_revision_round(CreateRevisionRoundInput {
                task_id,
                project_id: project.clone(),
                round_number: 2,
                request_id: "rev-1".to_owned(),
                payload_hash: "hash-rev-1".to_owned(),
                findings: Some("fix it".to_owned()),
            })
            .expect("revision");

        assert_eq!(outcome.round.round_number, 2);
        assert_eq!(outcome.round.kind, RoundKind::Revise);
        assert_eq!(outcome.round.status, RoundStatus::Pending);
        assert!(!outcome.round.attempted);
        assert_eq!(outcome.round.request_id, "rev-1");
        assert_eq!(outcome.round.payload_hash, "hash-rev-1");
        assert_eq!(outcome.round.findings.as_deref(), Some("fix it"));
        assert_eq!(outcome.round.session_id, None);
        assert_eq!(outcome.round.outbound_message_id, None);
        assert_eq!(outcome.round.created_at, outcome.round.updated_at);

        assert_eq!(outcome.task.status, TaskStatus::Revising);
        assert_eq!(outcome.task.session_id, None);
        assert_eq!(outcome.task.revision_count, 1);
        assert_eq!(outcome.task.updated_at, outcome.round.updated_at);

        let round_one = fetch_round(&storage, task_id, 1);
        assert_eq!(round_one.session_id.as_deref(), Some("ses-1"));

        let events = event_rows(&storage, task_id);
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].0, Some(2));
        assert_eq!(events[2].1, "created");
        assert_eq!(events[2].2, "round created (revise)");
    }

    #[test]
    fn create_revision_round_request_conflict_rolls_back() {
        let (_dir, mut storage) = open_query_storage("round-request-conflict");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        setup_awaiting_review(&mut storage, task_id, &project);
        storage
            .create_revision_round(CreateRevisionRoundInput {
                task_id,
                project_id: project.clone(),
                round_number: 2,
                request_id: "rev-1".to_owned(),
                payload_hash: "h1".to_owned(),
                findings: None,
            })
            .expect("first revision");

        let rounds_before = count_rows(&storage, "rounds");
        let events_before = count_rows(&storage, "events");
        let error = storage
            .create_revision_round(CreateRevisionRoundInput {
                task_id,
                project_id: project.clone(),
                round_number: 3,
                request_id: "rev-1".to_owned(),
                payload_hash: "h2".to_owned(),
                findings: None,
            })
            .expect_err("duplicate request must conflict");
        assert!(
            matches!(error, RoundUpdateError::RequestConflict),
            "{error:?}"
        );
        assert_eq!(count_rows(&storage, "rounds"), rounds_before);
        assert_eq!(count_rows(&storage, "events"), events_before);
    }

    #[test]
    fn create_revision_round_rejects_gap_and_duplicate_numbers() {
        let (_dir, mut storage) = open_query_storage("round-sequential");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        setup_awaiting_review(&mut storage, task_id, &project);

        for bad in [3_u32, 1_u32] {
            let error = storage
                .create_revision_round(CreateRevisionRoundInput {
                    task_id,
                    project_id: project.clone(),
                    round_number: bad,
                    request_id: format!("rev-{bad}"),
                    payload_hash: "h".to_owned(),
                    findings: None,
                })
                .expect_err("non-sequential round must fail");
            assert!(
                matches!(error, RoundUpdateError::NonSequentialRound),
                "{error:?}"
            );
        }
        assert_eq!(count_rows(&storage, "rounds"), 1);

        storage
            .create_revision_round(CreateRevisionRoundInput {
                task_id,
                project_id: project.clone(),
                round_number: 2,
                request_id: "rev-2".to_owned(),
                payload_hash: "h2".to_owned(),
                findings: None,
            })
            .expect("sequential revision");

        let error = storage
            .create_revision_round(CreateRevisionRoundInput {
                task_id,
                project_id: project.clone(),
                round_number: 2,
                request_id: "rev-2b".to_owned(),
                payload_hash: "h2b".to_owned(),
                findings: None,
            })
            .expect_err("duplicate round must fail");
        assert!(
            matches!(error, RoundUpdateError::NonSequentialRound),
            "{error:?}"
        );
        assert_eq!(count_rows(&storage, "rounds"), 2);
    }

    #[test]
    fn bind_round_session_updates_both_rows_atomically() {
        let (_dir, mut storage) = open_query_storage("round-bind");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-bind"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);

        let first = storage
            .bind_round_session(round.clone(), "ses-a".to_owned())
            .expect("bind a");
        assert_eq!(first.round.session_id.as_deref(), Some("ses-a"));
        assert_eq!(first.task.session_id.as_deref(), Some("ses-a"));
        assert_eq!(first.round.updated_at, first.task.updated_at);

        let second = storage
            .bind_round_session(round.clone(), "ses-b".to_owned())
            .expect("bind b");
        assert_eq!(second.round.session_id.as_deref(), Some("ses-b"));
        assert_eq!(second.task.session_id.as_deref(), Some("ses-b"));

        let before = dump_rows(&storage, "tasks");
        let error = storage
            .bind_round_session(round, String::new())
            .expect_err("empty session must fail");
        assert!(matches!(error, RoundUpdateError::InvalidInput), "{error:?}");
        assert_eq!(dump_rows(&storage, "tasks"), before);
    }

    #[test]
    fn round_lifecycle_rejects_missing_stale_and_mismatched_refs() {
        let (_dir, mut storage) = open_query_storage("round-refs");
        let project_a = query_project(QUERY_PROJECT_A);
        let project_b = query_project(QUERY_PROJECT_B);
        let task_id = query_task_id(1);
        setup_awaiting_review(&mut storage, task_id, &project_a);

        let error = storage
            .prepare_round(
                RoundRef {
                    task_id: query_task_id(9),
                    project_id: project_a.clone(),
                    round_number: 1,
                },
                "msg".to_owned(),
            )
            .expect_err("missing task");
        assert!(matches!(error, RoundUpdateError::MissingTask), "{error:?}");

        let error = storage
            .prepare_round(round_ref(task_id, &project_a, 5), "msg".to_owned())
            .expect_err("missing round");
        assert!(matches!(error, RoundUpdateError::MissingRound), "{error:?}");

        let error = storage
            .prepare_round(round_ref(task_id, &project_b, 1), "msg".to_owned())
            .expect_err("project mismatch");
        assert!(
            matches!(error, RoundUpdateError::ProjectMismatch),
            "{error:?}"
        );

        storage
            .create_revision_round(CreateRevisionRoundInput {
                task_id,
                project_id: project_a.clone(),
                round_number: 2,
                request_id: "rev-1".to_owned(),
                payload_hash: "h".to_owned(),
                findings: None,
            })
            .expect("revision");

        let error = storage
            .bind_round_session(round_ref(task_id, &project_a, 1), "ses".to_owned())
            .expect_err("stale round");
        assert!(
            matches!(error, RoundUpdateError::NotCurrentRound),
            "{error:?}"
        );
    }

    #[test]
    fn prepare_round_fails_closed_on_repeat_and_stale() {
        let (_dir, mut storage) = open_query_storage("round-prepare");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-prep"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);

        storage
            .prepare_round(round.clone(), "msg-1".to_owned())
            .expect("first prepare");
        let before = dump_rows(&storage, "rounds");
        let error = storage
            .prepare_round(round.clone(), "msg-2".to_owned())
            .expect_err("repeat prepare must fail");
        assert!(
            matches!(error, RoundUpdateError::AlreadyPrepared),
            "{error:?}"
        );
        assert_eq!(dump_rows(&storage, "rounds"), before);

        storage.mark_round_sent(round.clone()).expect("sent");
        let error = storage
            .prepare_round(round, "msg-3".to_owned())
            .expect_err("stale prepare must fail");
        assert!(
            matches!(error, RoundUpdateError::RoundNotPending),
            "{error:?}"
        );
    }

    #[test]
    fn mark_round_sent_requires_prepared_pending_round() {
        let (_dir, mut storage) = open_query_storage("round-sent");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-sent"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);

        let before = dump_rows(&storage, "rounds");
        let error = storage
            .mark_round_sent(round.clone())
            .expect_err("unprepared round must fail");
        assert!(
            matches!(error, RoundUpdateError::RoundNotPrepared),
            "{error:?}"
        );
        assert_eq!(dump_rows(&storage, "rounds"), before);

        storage
            .prepare_round(round.clone(), "msg-1".to_owned())
            .expect("prepare");
        let sent = storage.mark_round_sent(round.clone()).expect("sent");
        assert_eq!(sent.round.status, RoundStatus::Sent);
        assert!(sent.round.attempted);

        let error = storage
            .mark_round_sent(round)
            .expect_err("second send must fail");
        assert!(
            matches!(error, RoundUpdateError::InvalidRoundTransition),
            "{error:?}"
        );
    }

    #[test]
    fn mark_round_observing_only_from_pending_or_sent() {
        let (_dir, mut storage) = open_query_storage("round-observe-only");
        let project_a = query_project(QUERY_PROJECT_A);
        let task_a = query_task_id(1);
        storage
            .create_task(create_task_input(task_a, &project_a, "req-obs-a"))
            .expect("create a");
        let round_a = round_ref(task_a, &project_a, 1);
        storage
            .mark_round_observing(round_a.clone())
            .expect("pending to observing");
        let error = storage
            .mark_round_observing(round_a)
            .expect_err("observing again must fail");
        assert!(
            matches!(error, RoundUpdateError::InvalidRoundTransition),
            "{error:?}"
        );

        let project_b = query_project(QUERY_PROJECT_B);
        let task_b = query_task_id(2);
        storage
            .create_task(create_task_input(task_b, &project_b, "req-obs-b"))
            .expect("create b");
        let round_b = round_ref(task_b, &project_b, 1);
        storage
            .prepare_round(round_b.clone(), "msg".to_owned())
            .expect("prepare b");
        storage.mark_round_sent(round_b.clone()).expect("sent b");
        let observing = storage
            .mark_round_observing(round_b)
            .expect("sent to observing");
        assert_eq!(observing.round.status, RoundStatus::Observing);
        assert!(observing.round.attempted);
    }

    #[test]
    fn mark_worker_started_persists_single_clock_window() {
        let (_dir, mut storage) = open_query_storage("round-worker");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-worker"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);

        let outcome = storage
            .mark_worker_started(round.clone(), 30.0)
            .expect("started");
        let start = outcome.round.worker_started_at.clone().expect("start");
        let deadline = outcome.round.worker_deadline_at.clone().expect("deadline");
        assert!(is_rfc3339_millis_utc(&start));
        assert!(is_rfc3339_millis_utc(&deadline));
        assert!(
            deadline > start,
            "deadline {deadline} must be after start {start}"
        );
        assert_eq!(outcome.round.updated_at, start);

        for bad in [0.0_f64, -1.0, f64::NAN, f64::INFINITY] {
            let before = dump_rows(&storage, "rounds");
            let error = storage
                .mark_worker_started(round.clone(), bad)
                .expect_err("invalid deadline must fail");
            assert!(
                matches!(error, RoundUpdateError::InvalidDeadline),
                "{error:?}"
            );
            assert_eq!(dump_rows(&storage, "rounds"), before);
        }

        storage
            .mark_round_observing(round.clone())
            .expect("observing");
        storage
            .finish_round(FinishRoundInput {
                round: round.clone(),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect("finish");
        let error = storage
            .mark_worker_started(round, 30.0)
            .expect_err("closed round must fail");
        assert!(matches!(error, RoundUpdateError::RoundNotOpen), "{error:?}");
    }

    #[test]
    fn finish_round_validates_round_and_task_transitions() {
        let (_dir, mut storage) = open_query_storage("round-finish-transitions");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-fin"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);
        storage
            .mark_round_observing(round.clone())
            .expect("observing");

        let rounds_before = dump_rows(&storage, "rounds");
        let tasks_before = dump_rows(&storage, "tasks");
        let error = storage
            .finish_round(FinishRoundInput {
                round: round.clone(),
                round_status: RoundStatus::Sent,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect_err("forbidden round transition");
        assert!(
            matches!(error, RoundUpdateError::InvalidRoundTransition),
            "{error:?}"
        );
        assert_eq!(dump_rows(&storage, "rounds"), rounds_before);
        assert_eq!(dump_rows(&storage, "tasks"), tasks_before);

        let error = storage
            .finish_round(FinishRoundInput {
                round: round.clone(),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::Accepted,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect_err("forbidden task transition");
        assert!(
            matches!(error, RoundUpdateError::InvalidTaskTransition),
            "{error:?}"
        );
        assert_eq!(dump_rows(&storage, "rounds"), rounds_before);
        assert_eq!(dump_rows(&storage, "tasks"), tasks_before);

        let finished = storage
            .finish_round(FinishRoundInput {
                round,
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect("valid finish");
        assert_eq!(finished.round.status, RoundStatus::Complete);
        assert_eq!(finished.task.status, TaskStatus::AwaitingReview);
    }

    #[test]
    fn finish_round_result_json_shape_and_column_preservation() {
        let (_dir, mut storage) = open_query_storage("round-finish-json");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-json"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);
        storage
            .mark_round_observing(round.clone())
            .expect("observing");

        let before = dump_rows(&storage, "rounds");
        let error = storage
            .finish_round(FinishRoundInput {
                round: round.clone(),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: Some(serde_json::json!([1, 2, 3])),
            })
            .expect_err("array result must fail");
        assert!(matches!(error, RoundUpdateError::InvalidJson), "{error:?}");
        assert_eq!(dump_rows(&storage, "rounds"), before);

        let finished = storage
            .finish_round(FinishRoundInput {
                round: round.clone(),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: Some("msg-2".to_owned()),
                response: Some("answer".to_owned()),
                error_code: Some("code".to_owned()),
                result_json: Some(serde_json::json!({"changed_paths": ["a"]})),
            })
            .expect("valid finish");
        assert_eq!(
            finished.round.result_json,
            Some(serde_json::json!({"changed_paths": ["a"]}))
        );
        assert_eq!(finished.round.error_code.as_deref(), Some("code"));
        assert_eq!(finished.round.response.as_deref(), Some("answer"));

        let project_b = query_project(QUERY_PROJECT_B);
        let task_b = query_task_id(2);
        storage
            .create_task(create_task_input(task_b, &project_b, "req-json-b"))
            .expect("create b");
        storage
            .connection()
            .execute(
                "UPDATE rounds SET status = 'observing', error_code = 'pre', \
                 result_json = '{\"x\":1}' WHERE task_id = ?1 AND round_number = 1",
                rusqlite::params![task_b.to_string()],
            )
            .expect("seed observing round");
        let finished_b = storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_b, &project_b, 1),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: Some(Value::Null),
            })
            .expect("finish b");
        assert_eq!(
            finished_b.round.result_json, None,
            "supplied null must clear"
        );
        assert_eq!(
            finished_b.round.error_code.as_deref(),
            Some("pre"),
            "omitted error_code must be preserved"
        );
    }

    #[test]
    fn finish_round_allows_blocking_status_self_transitions() {
        let (_dir, mut storage) = open_query_storage("round-self");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-self"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);
        storage
            .mark_round_observing(round.clone())
            .expect("observing");

        let first = storage
            .finish_round(FinishRoundInput {
                round: round.clone(),
                round_status: RoundStatus::NeedsUser,
                task_status: TaskStatus::NeedsUser,
                response_message_id: None,
                response: None,
                error_code: Some("blocked".to_owned()),
                result_json: None,
            })
            .expect("needs_user");
        assert_eq!(first.round.status, RoundStatus::NeedsUser);
        assert_eq!(first.task.status, TaskStatus::NeedsUser);

        let second = storage
            .finish_round(FinishRoundInput {
                round,
                round_status: RoundStatus::NeedsUser,
                task_status: TaskStatus::NeedsUser,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect("needs_user self transition");
        assert_eq!(second.round.status, RoundStatus::NeedsUser);
        assert_eq!(second.task.status, TaskStatus::NeedsUser);
    }

    #[test]
    fn finish_round_cannot_reopen_a_failed_round() {
        // Regression for the 3.9c review: `failed -> observing` must be
        // reachable only through `reopen_failed_round` after its eligibility
        // guards, never through the generic `finish_round` path. Even a
        // recoverable `assistant_error` (and a task with a pending close) must
        // be rejected by `finish_round` with no partial write.
        for (label, error_code, close_requested) in [
            ("nonrecoverable", "workspace_mismatch", false),
            ("assistant_error", RECOVERABLE_FAILED_ERROR_CODE, false),
            ("pending_close", RECOVERABLE_FAILED_ERROR_CODE, true),
        ] {
            let (dir, mut storage) = open_query_storage("round-finish-reopen-guard");
            let path = dir.join("state.sqlite");
            let project = query_project(QUERY_PROJECT_A);
            let task_id = query_task_id(1);
            storage
                .create_task(create_task_input(task_id, &project, "req-fin-reopen"))
                .expect("create");
            fail_current_round(&mut storage, task_id, &project, 1, error_code);
            if close_requested {
                execute(
                    &path,
                    &format!(
                        "UPDATE tasks SET close_requested_at = '2026-01-01T00:00:00.000+00:00', \
                         close_reason = 'stuck' WHERE task_id = '{task_id}'"
                    ),
                );
            }

            let rounds_before = dump_rows(&storage, "rounds");
            let tasks_before = dump_rows(&storage, "tasks");
            let events_before = event_rows(&storage, task_id);

            let error = storage
                .finish_round(FinishRoundInput {
                    round: round_ref(task_id, &project, 1),
                    round_status: RoundStatus::Observing,
                    task_status: TaskStatus::Implementing,
                    response_message_id: None,
                    response: None,
                    error_code: None,
                    result_json: None,
                })
                .expect_err("finish_round must not reopen a failed round");
            assert!(
                matches!(error, RoundUpdateError::InvalidRoundTransition),
                "{label}: {error:?}"
            );
            assert_eq!(dump_rows(&storage, "rounds"), rounds_before, "{label}");
            assert_eq!(dump_rows(&storage, "tasks"), tasks_before, "{label}");
            assert_eq!(event_rows(&storage, task_id), events_before, "{label}");
        }
    }

    #[test]
    fn finish_round_is_all_or_nothing_on_event_failure() {
        let dir = TempDir::new("round-finish-rollback");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let mut storage = connect(&path).expect("connect");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-rb"))
            .expect("create");
        storage
            .mark_round_observing(round_ref(task_id, &project, 1))
            .expect("observing");
        execute(&path, "DROP TABLE events");

        let error = storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_id, &project, 1),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect_err("event insert must fail");
        assert!(matches!(error, RoundUpdateError::Database(_)), "{error:?}");

        assert_eq!(
            fetch_round(&storage, task_id, 1).status,
            RoundStatus::Observing
        );
        assert_eq!(
            fetch_task(&storage, task_id).status,
            TaskStatus::Implementing
        );
    }

    #[test]
    fn create_revision_round_is_all_or_nothing_on_event_failure() {
        let dir = TempDir::new("round-revision-rollback");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let mut storage = connect(&path).expect("connect");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        setup_awaiting_review(&mut storage, task_id, &project);
        let tasks_before = dump_rows(&storage, "tasks");
        execute(&path, "DROP TABLE events");

        let error = storage
            .create_revision_round(CreateRevisionRoundInput {
                task_id,
                project_id: project.clone(),
                round_number: 2,
                request_id: "rev-rb".to_owned(),
                payload_hash: "h".to_owned(),
                findings: None,
            })
            .expect_err("event insert must fail");
        assert!(matches!(error, RoundUpdateError::Database(_)), "{error:?}");
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(dump_rows(&storage, "tasks"), tasks_before);
    }

    #[test]
    fn round_update_errors_do_not_leak_input() {
        const SECRET: &str = "round-secret-token";
        let (_dir, mut storage) = open_query_storage("round-error-safety");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-safe"))
            .expect("create");

        let secret_project = query_project(SECRET);
        let error = storage
            .prepare_round(round_ref(task_id, &secret_project, 1), SECRET.to_owned())
            .expect_err("project mismatch");
        assert!(
            matches!(error, RoundUpdateError::ProjectMismatch),
            "{error:?}"
        );
        assert_round_update_error_is_safe(&error, SECRET);

        let error = storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_id, &project, 1),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: Some(SECRET.to_owned()),
                response: Some(SECRET.to_owned()),
                error_code: Some(SECRET.to_owned()),
                result_json: Some(serde_json::json!([SECRET])),
            })
            .expect_err("invalid json");
        assert!(matches!(error, RoundUpdateError::InvalidJson), "{error:?}");
        assert_round_update_error_is_safe(&error, SECRET);
    }

    #[test]
    fn concurrent_revision_round_creation_yields_one_winner() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("round-concurrent-revision");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        {
            let mut storage = connect(&path).expect("connect");
            setup_awaiting_review(&mut storage, task_id, &project);
        }
        drop(connect(&path).expect("pre-create WAL database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for n in 1..=2_u32 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let project = project.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut storage = connect(path.as_path()).expect("connect");
                storage.create_revision_round(CreateRevisionRoundInput {
                    task_id,
                    project_id: project,
                    round_number: 2,
                    request_id: format!("rev-{n}"),
                    payload_hash: format!("hash-{n}"),
                    findings: None,
                })
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread must not panic"))
            .collect();
        let winners = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(winners, 1, "{results:?}");

        let storage = connect(path.as_path()).expect("connect verify");
        assert_eq!(count_rows(&storage, "rounds"), 2);
        let task = fetch_task(&storage, task_id);
        assert_eq!(task.status, TaskStatus::Revising);
        assert_eq!(task.revision_count, 1);
        assert_eq!(count_rows(&storage, "events"), 3);
    }

    #[test]
    fn concurrent_finish_round_yields_one_winner() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("round-concurrent-finish");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        {
            let mut storage = connect(&path).expect("connect");
            storage
                .create_task(create_task_input(task_id, &project, "req-cf"))
                .expect("create");
            storage
                .mark_round_observing(round_ref(task_id, &project, 1))
                .expect("observing");
        }
        drop(connect(&path).expect("pre-create WAL database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let project = project.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut storage = connect(path.as_path()).expect("connect");
                storage.finish_round(FinishRoundInput {
                    round: RoundRef {
                        task_id,
                        project_id: project,
                        round_number: 1,
                    },
                    round_status: RoundStatus::Complete,
                    task_status: TaskStatus::AwaitingReview,
                    response_message_id: None,
                    response: None,
                    error_code: None,
                    result_json: None,
                })
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread must not panic"))
            .collect();
        let winners = results.iter().filter(|result| result.is_ok()).count();
        let losers = results
            .iter()
            .filter(|result| matches!(result, Err(RoundUpdateError::InvalidRoundTransition)))
            .count();
        assert_eq!(winners, 1, "{results:?}");
        assert_eq!(losers, 1, "{results:?}");

        let storage = connect(path.as_path()).expect("connect verify");
        assert_eq!(
            fetch_round(&storage, task_id, 1).status,
            RoundStatus::Complete
        );
        assert_eq!(
            fetch_task(&storage, task_id).status,
            TaskStatus::AwaitingReview
        );
        assert_eq!(count_rows(&storage, "events"), 2);
    }

    #[test]
    fn round_lifecycle_does_not_touch_committed_fixtures() {
        let fixtures = fixture_dir();
        let before: Vec<Vec<u8>> = FIXTURES
            .iter()
            .map(|name| std::fs::read(fixtures.join(name)).expect("read fixture"))
            .collect();

        let (_dir, mut storage) = open_query_storage("round-fixture-safety");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-fx"))
            .expect("create");
        storage
            .mark_round_observing(round_ref(task_id, &project, 1))
            .expect("observing");
        storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_id, &project, 1),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect("finish");

        let after: Vec<Vec<u8>> = FIXTURES
            .iter()
            .map(|name| std::fs::read(fixtures.join(name)).expect("read fixture"))
            .collect();
        assert_eq!(before, after);
        for name in FIXTURES {
            assert!(!sidecar(&fixtures.join(name), "-wal").exists());
            assert!(!sidecar(&fixtures.join(name), "-shm").exists());
        }
    }

    // -----------------------------------------------------------------------
    // Verifier persist-once (task 3.9b).
    // -----------------------------------------------------------------------

    fn sample_verification(status: VerificationStatus, log: &str) -> Verification {
        Verification {
            status,
            commands: Some(vec![VerificationCommand {
                command: "cargo test".to_owned(),
                timed_out: None,
                duration: Some(0.5),
                exit_code: Some(0),
                output_tail: None,
                reason: None,
            }]),
            index: None,
            reason: None,
            log: log.to_owned(),
            before: None,
            after: None,
            side_effects: None,
            repositories: None,
        }
    }

    fn assert_corrupt_round_row_error(label: &str, error: &RoundUpdateError) {
        let inner = match error {
            RoundUpdateError::RoundRow(inner) => inner,
            other => panic!("{label}: expected round row error, got {other:?}"),
        };
        match label {
            "done_without_json" | "running_with_json" => assert!(
                matches!(inner, RoundRowError::InconsistentVerifier),
                "{label}: {inner:?}"
            ),
            "unknown_state" => assert!(
                matches!(inner, RoundRowError::UnknownVerifierState),
                "{label}: {inner:?}"
            ),
            "malformed_json" => assert!(
                matches!(
                    inner,
                    RoundRowError::MalformedJson {
                        column: "verifier_json"
                    }
                ),
                "{label}: {inner:?}"
            ),
            "wrong_shape_json" => assert!(
                matches!(
                    inner,
                    RoundRowError::WrongJsonShape {
                        column: "verifier_json"
                    }
                ),
                "{label}: {inner:?}"
            ),
            other => panic!("unknown corrupt verifier case {other}"),
        }
    }

    #[test]
    fn begin_verifier_starts_refreshes_and_never_undoes_done() {
        let (_dir, mut storage) = open_query_storage("verifier-begin");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-vb"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);
        assert_eq!(count_rows(&storage, "events"), 1, "only the create event");

        let started = storage.begin_verifier(round.clone()).expect("begin");
        assert!(matches!(started, VerifierUpdateOutcome::Started(_)));
        let started_row = started.outcome();
        assert_eq!(
            started_row.round.verifier_state,
            Some(VerifierState::Running)
        );
        assert_eq!(started_row.round.verifier_json, None);
        assert!(is_rfc3339_millis_utc(&started_row.round.updated_at));

        let again = storage.begin_verifier(round.clone()).expect("begin again");
        assert!(matches!(again, VerifierUpdateOutcome::AlreadyRunning(_)));
        assert_eq!(
            again.outcome().round.verifier_state,
            Some(VerifierState::Running)
        );
        assert_eq!(
            count_rows(&storage, "events"),
            1,
            "verifier writes no events"
        );

        let verification = sample_verification(VerificationStatus::Passed, "log-a");
        let completed = storage
            .complete_verifier(CompleteVerifierInput {
                round: round.clone(),
                verification: verification.clone(),
            })
            .expect("complete");
        assert!(matches!(completed, VerifierUpdateOutcome::Completed(_)));
        assert_eq!(
            completed.outcome().round.verifier_json.as_ref(),
            Some(&verification)
        );
        assert_eq!(
            count_rows(&storage, "events"),
            1,
            "verifier writes no events"
        );

        let before = dump_rows(&storage, "rounds");
        let done = storage.begin_verifier(round).expect("begin after done");
        assert!(matches!(done, VerifierUpdateOutcome::AlreadyDone(_)));
        assert_eq!(
            done.outcome().round.verifier_json.as_ref(),
            Some(&verification)
        );
        assert_eq!(dump_rows(&storage, "rounds"), before);
    }

    #[test]
    fn complete_verifier_persists_without_prior_start() {
        let (_dir, mut storage) = open_query_storage("verifier-complete-fresh");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-vcf"))
            .expect("create");
        let verification = sample_verification(VerificationStatus::TimedOut, "log-fresh");

        let completed = storage
            .complete_verifier(CompleteVerifierInput {
                round: round_ref(task_id, &project, 1),
                verification: verification.clone(),
            })
            .expect("complete");
        assert!(matches!(completed, VerifierUpdateOutcome::Completed(_)));
        let row = fetch_round(&storage, task_id, 1);
        assert_eq!(row.verifier_state, Some(VerifierState::Done));
        assert_eq!(row.verifier_json.as_ref(), Some(&verification));
    }

    #[test]
    fn complete_verifier_replays_identical_and_rejects_conflict() {
        let (_dir, mut storage) = open_query_storage("verifier-replay");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-vr"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);
        let verification = sample_verification(VerificationStatus::Passed, "log-1");
        storage
            .complete_verifier(CompleteVerifierInput {
                round: round.clone(),
                verification: verification.clone(),
            })
            .expect("first complete");

        let before = dump_rows(&storage, "rounds");
        let replayed = storage
            .complete_verifier(CompleteVerifierInput {
                round: round.clone(),
                verification: verification.clone(),
            })
            .expect("identical replay");
        assert!(matches!(replayed, VerifierUpdateOutcome::Replayed(_)));
        assert_eq!(
            replayed.outcome().round.verifier_json.as_ref(),
            Some(&verification)
        );
        assert_eq!(dump_rows(&storage, "rounds"), before);

        let error = storage
            .complete_verifier(CompleteVerifierInput {
                round,
                verification: sample_verification(VerificationStatus::Failed, "log-2"),
            })
            .expect_err("conflicting result must fail");
        assert!(
            matches!(error, RoundUpdateError::VerifierResultConflict),
            "{error:?}"
        );
        assert_eq!(dump_rows(&storage, "rounds"), before);
        assert_eq!(
            fetch_round(&storage, task_id, 1)
                .verifier_json
                .map(|value| value.status),
            Some(VerificationStatus::Passed)
        );
    }

    #[test]
    fn verifier_updates_reject_missing_stale_and_mismatched_refs() {
        let (_dir, mut storage) = open_query_storage("verifier-refs");
        let project_a = query_project(QUERY_PROJECT_A);
        let project_b = query_project(QUERY_PROJECT_B);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project_a, "req-vref"))
            .expect("create");
        let verification = sample_verification(VerificationStatus::Passed, "log-ref");

        let before = dump_rows(&storage, "rounds");
        let error = storage
            .begin_verifier(round_ref(query_task_id(9), &project_a, 1))
            .expect_err("missing task");
        assert!(matches!(error, RoundUpdateError::MissingTask), "{error:?}");
        let error = storage
            .begin_verifier(round_ref(task_id, &project_a, 7))
            .expect_err("missing round");
        assert!(matches!(error, RoundUpdateError::MissingRound), "{error:?}");
        let error = storage
            .begin_verifier(round_ref(task_id, &project_b, 1))
            .expect_err("project mismatch");
        assert!(
            matches!(error, RoundUpdateError::ProjectMismatch),
            "{error:?}"
        );
        assert_eq!(dump_rows(&storage, "rounds"), before);

        storage
            .mark_round_observing(round_ref(task_id, &project_a, 1))
            .expect("observing");
        storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_id, &project_a, 1),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect("finish");
        storage
            .create_revision_round(CreateRevisionRoundInput {
                task_id,
                project_id: project_a.clone(),
                round_number: 2,
                request_id: "rev-2".to_owned(),
                payload_hash: "hash-2".to_owned(),
                findings: None,
            })
            .expect("revision");

        let before = dump_rows(&storage, "rounds");
        let error = storage
            .begin_verifier(round_ref(task_id, &project_a, 1))
            .expect_err("stale round");
        assert!(
            matches!(error, RoundUpdateError::NotCurrentRound),
            "{error:?}"
        );
        let error = storage
            .complete_verifier(CompleteVerifierInput {
                round: round_ref(task_id, &project_a, 1),
                verification,
            })
            .expect_err("stale round complete");
        assert!(
            matches!(error, RoundUpdateError::NotCurrentRound),
            "{error:?}"
        );
        assert_eq!(dump_rows(&storage, "rounds"), before);
    }

    #[test]
    fn verifier_updates_reject_corrupt_pair_fail_closed() {
        let (_dir, mut storage) = open_query_storage("verifier-corrupt");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-vcor"))
            .expect("create");
        let round = round_ref(task_id, &project, 1);
        let verification = sample_verification(VerificationStatus::Passed, "log-corrupt");

        let cases = [
            (
                "done_without_json",
                "UPDATE rounds SET verifier_state = 'done', verifier_json = NULL \
                 WHERE task_id = ?1 AND round_number = 1",
            ),
            (
                "running_with_json",
                "UPDATE rounds SET verifier_state = 'running', \
                 verifier_json = '{\"status\":\"passed\",\"log\":\"x\"}' \
                 WHERE task_id = ?1 AND round_number = 1",
            ),
            (
                "unknown_state",
                "UPDATE rounds SET verifier_state = 'bogus', verifier_json = NULL \
                 WHERE task_id = ?1 AND round_number = 1",
            ),
            (
                "malformed_json",
                "UPDATE rounds SET verifier_state = 'done', verifier_json = 'not-json' \
                 WHERE task_id = ?1 AND round_number = 1",
            ),
            (
                "wrong_shape_json",
                "UPDATE rounds SET verifier_state = 'done', verifier_json = '[]' \
                 WHERE task_id = ?1 AND round_number = 1",
            ),
        ];

        for (label, sql) in cases {
            storage
                .connection()
                .execute(sql, rusqlite::params![task_id.to_string()])
                .expect("seed corrupt state");
            let before = dump_rows(&storage, "rounds");

            let error = storage
                .begin_verifier(round.clone())
                .expect_err("begin must reject corrupt state");
            assert_corrupt_round_row_error(label, &error);
            assert_eq!(dump_rows(&storage, "rounds"), before);

            let error = storage
                .complete_verifier(CompleteVerifierInput {
                    round: round.clone(),
                    verification: verification.clone(),
                })
                .expect_err("complete must reject corrupt state");
            assert_corrupt_round_row_error(label, &error);
            assert_eq!(dump_rows(&storage, "rounds"), before);

            storage
                .connection()
                .execute(
                    "UPDATE rounds SET verifier_state = NULL, verifier_json = NULL \
                     WHERE task_id = ?1 AND round_number = 1",
                    rusqlite::params![task_id.to_string()],
                )
                .expect("reset corrupt state");
        }
    }

    #[test]
    fn verifier_update_errors_do_not_leak_input() {
        const SECRET: &str = "verifier-secret-token";
        let (_dir, mut storage) = open_query_storage("verifier-error-safety");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-vsafe"))
            .expect("create");

        let secret_project = query_project(SECRET);
        let error = storage
            .begin_verifier(round_ref(task_id, &secret_project, 1))
            .expect_err("project mismatch");
        assert!(
            matches!(error, RoundUpdateError::ProjectMismatch),
            "{error:?}"
        );
        assert_round_update_error_is_safe(&error, SECRET);

        storage
            .complete_verifier(CompleteVerifierInput {
                round: round_ref(task_id, &project, 1),
                verification: sample_verification(VerificationStatus::Passed, SECRET),
            })
            .expect("first complete");
        let error = storage
            .complete_verifier(CompleteVerifierInput {
                round: round_ref(task_id, &project, 1),
                verification: sample_verification(VerificationStatus::Failed, SECRET),
            })
            .expect_err("conflict");
        assert!(
            matches!(error, RoundUpdateError::VerifierResultConflict),
            "{error:?}"
        );
        assert_round_update_error_is_safe(&error, SECRET);
    }

    #[test]
    fn concurrent_complete_verifier_yields_one_winner_one_conflict() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("verifier-concurrent-conflict");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        {
            let mut storage = connect(&path).expect("connect");
            storage
                .create_task(create_task_input(task_id, &project, "req-vcon"))
                .expect("create");
        }
        drop(connect(&path).expect("pre-create WAL database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for n in 0..2_u32 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let project = project.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut storage = connect(path.as_path()).expect("connect");
                let status = if n == 0 {
                    VerificationStatus::Passed
                } else {
                    VerificationStatus::Failed
                };
                storage.complete_verifier(CompleteVerifierInput {
                    round: RoundRef {
                        task_id,
                        project_id: project,
                        round_number: 1,
                    },
                    verification: sample_verification(status, "log"),
                })
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread must not panic"))
            .collect();
        let winners = results
            .iter()
            .filter(|result| matches!(result, Ok(VerifierUpdateOutcome::Completed(_))))
            .count();
        let conflicts = results
            .iter()
            .filter(|result| matches!(result, Err(RoundUpdateError::VerifierResultConflict)))
            .count();
        assert_eq!(winners, 1, "{results:?}");
        assert_eq!(conflicts, 1, "{results:?}");

        let storage = connect(path.as_path()).expect("verify");
        let row = fetch_round(&storage, task_id, 1);
        assert_eq!(row.verifier_state, Some(VerifierState::Done));
        assert!(row.verifier_json.is_some());
    }

    #[test]
    fn concurrent_identical_complete_verifier_replays() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("verifier-concurrent-replay");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        {
            let mut storage = connect(&path).expect("connect");
            storage
                .create_task(create_task_input(task_id, &project, "req-vcon2"))
                .expect("create");
        }
        drop(connect(&path).expect("pre-create WAL database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let project = project.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut storage = connect(path.as_path()).expect("connect");
                storage.complete_verifier(CompleteVerifierInput {
                    round: RoundRef {
                        task_id,
                        project_id: project,
                        round_number: 1,
                    },
                    verification: sample_verification(VerificationStatus::Passed, "log"),
                })
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread must not panic"))
            .collect();
        let winners = results
            .iter()
            .filter(|result| matches!(result, Ok(VerifierUpdateOutcome::Completed(_))))
            .count();
        let replays = results
            .iter()
            .filter(|result| matches!(result, Ok(VerifierUpdateOutcome::Replayed(_))))
            .count();
        assert_eq!(winners, 1, "{results:?}");
        assert_eq!(replays, 1, "{results:?}");
    }

    #[test]
    fn reopen_failed_implement_round_restores_implementing() {
        let (_dir, mut storage) = open_query_storage("reopen-implement");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-reopen"))
            .expect("create");
        fail_current_round(
            &mut storage,
            task_id,
            &project,
            1,
            RECOVERABLE_FAILED_ERROR_CODE,
        );

        let outcome = storage
            .reopen_failed_round(task_id)
            .expect("reopen must succeed");
        let reopened = outcome.reopened().expect("must be reopened");
        assert_eq!(reopened.round.round_number, 1);
        assert_eq!(reopened.round.status, RoundStatus::Observing);
        assert_eq!(reopened.round.error_code, None);
        assert_eq!(reopened.task.status, TaskStatus::Implementing);
        assert_eq!(reopened.round.updated_at, reopened.task.updated_at);

        let events = event_rows(&storage, task_id);
        let reopened_events: Vec<_> = events
            .iter()
            .filter(|event| event.1 == "reopened")
            .collect();
        assert_eq!(reopened_events.len(), 1);
        assert_eq!(reopened_events[0].0, Some(1));
        assert_eq!(
            reopened_events[0].2,
            "failed assistant_error reopened for recovery"
        );

        // Idempotent: a repeated call after the transition is a typed no-op.
        assert_eq!(
            storage.reopen_failed_round(task_id).expect("repeat"),
            ReopenFailedRoundOutcome::NotEligible
        );
        assert_eq!(
            fetch_task(&storage, task_id).status,
            TaskStatus::Implementing
        );
        assert_eq!(event_rows(&storage, task_id).len(), events.len());
    }

    #[test]
    fn reopen_failed_revision_round_restores_revising() {
        let (_dir, mut storage) = open_query_storage("reopen-revision");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        setup_awaiting_review(&mut storage, task_id, &project);
        storage
            .create_revision_round(CreateRevisionRoundInput {
                task_id,
                project_id: project.clone(),
                round_number: 2,
                request_id: "rev-reopen".to_owned(),
                payload_hash: "hash-reopen".to_owned(),
                findings: Some("fix it".to_owned()),
            })
            .expect("revision");
        fail_current_round(
            &mut storage,
            task_id,
            &project,
            2,
            RECOVERABLE_FAILED_ERROR_CODE,
        );

        let outcome = storage
            .reopen_failed_round(task_id)
            .expect("reopen must succeed");
        let reopened = outcome.reopened().expect("must be reopened");
        assert_eq!(reopened.round.round_number, 2);
        assert_eq!(reopened.round.status, RoundStatus::Observing);
        assert_eq!(reopened.round.error_code, None);
        assert_eq!(reopened.task.status, TaskStatus::Revising);
        // The previous round keeps its recorded status.
        assert_eq!(
            fetch_round(&storage, task_id, 1).status,
            RoundStatus::Complete
        );
    }

    #[test]
    fn reopen_rejects_nonrecoverable_error_codes() {
        for error_code in [
            "workspace_mismatch",
            "session_not_found",
            "session_directory_mismatch",
            "worker_error",
        ] {
            let (_dir, mut storage) = open_query_storage("reopen-nonrecoverable");
            let project = query_project(QUERY_PROJECT_A);
            let task_id = query_task_id(1);
            storage
                .create_task(create_task_input(task_id, &project, "req-nonrec"))
                .expect("create");
            fail_current_round(&mut storage, task_id, &project, 1, error_code);

            assert_eq!(
                storage.reopen_failed_round(task_id).expect("reopen"),
                ReopenFailedRoundOutcome::NotEligible,
                "{error_code}"
            );
            let round = fetch_round(&storage, task_id, 1);
            assert_eq!(round.status, RoundStatus::Failed);
            assert_eq!(round.error_code.as_deref(), Some(error_code));
            assert_eq!(fetch_task(&storage, task_id).status, TaskStatus::Failed);
            assert!(
                event_rows(&storage, task_id)
                    .iter()
                    .all(|event| event.1 != "reopened"),
                "{error_code}"
            );
        }
    }

    #[test]
    fn reopen_requires_failed_task_and_failed_current_round() {
        let (dir, mut storage) = open_query_storage("reopen-requires");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-requires"))
            .expect("create");

        // A task not in `failed` is never reopened.
        assert_eq!(
            storage.reopen_failed_round(task_id).expect("reopen"),
            ReopenFailedRoundOutcome::NotEligible
        );

        fail_current_round(
            &mut storage,
            task_id,
            &project,
            1,
            RECOVERABLE_FAILED_ERROR_CODE,
        );
        // The task still says failed but the current round is no longer a failed
        // assistant error.
        execute(
            &path,
            &format!(
                "UPDATE rounds SET status = 'needs_user' \
                 WHERE task_id = '{task_id}' AND round_number = 1"
            ),
        );
        assert_eq!(
            storage.reopen_failed_round(task_id).expect("reopen"),
            ReopenFailedRoundOutcome::NotEligible
        );
        assert_eq!(fetch_task(&storage, task_id).status, TaskStatus::Failed);
        assert!(
            event_rows(&storage, task_id)
                .iter()
                .all(|event| event.1 != "reopened")
        );
    }

    #[test]
    fn reopen_unknown_task_is_not_eligible() {
        let (_dir, mut storage) = open_query_storage("reopen-unknown");
        assert_eq!(
            storage
                .reopen_failed_round(query_task_id(9))
                .expect("reopen"),
            ReopenFailedRoundOutcome::NotEligible
        );
        assert_eq!(count_rows(&storage, "events"), 0);
    }

    #[test]
    fn reopen_refuses_when_close_requested() {
        let (dir, mut storage) = open_query_storage("reopen-close");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-close"))
            .expect("create");
        fail_current_round(
            &mut storage,
            task_id,
            &project,
            1,
            RECOVERABLE_FAILED_ERROR_CODE,
        );
        execute(
            &path,
            &format!(
                "UPDATE tasks SET close_requested_at = '2026-01-01T00:00:00.000+00:00', \
                 close_reason = 'stuck' WHERE task_id = '{task_id}'"
            ),
        );

        assert_eq!(
            storage.reopen_failed_round(task_id).expect("reopen"),
            ReopenFailedRoundOutcome::NotEligible
        );
        assert_eq!(fetch_task(&storage, task_id).status, TaskStatus::Failed);
        assert_eq!(
            fetch_round(&storage, task_id, 1).status,
            RoundStatus::Failed
        );
        assert!(
            event_rows(&storage, task_id)
                .iter()
                .all(|event| event.1 != "reopened")
        );
    }

    #[test]
    fn reopen_does_not_reopen_a_non_current_failed_round() {
        let (dir, mut storage) = open_query_storage("reopen-noncurrent");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-noncurrent"))
            .expect("create");
        fail_current_round(
            &mut storage,
            task_id,
            &project,
            1,
            RECOVERABLE_FAILED_ERROR_CODE,
        );
        // A later round is now the current one, so the earlier failed round is
        // no longer eligible.
        seed_round_raw(
            &path,
            &task_id.to_string(),
            project.as_str(),
            2,
            "extra-req",
            "extra-hash",
            "revise",
        );

        assert_eq!(
            storage.reopen_failed_round(task_id).expect("reopen"),
            ReopenFailedRoundOutcome::NotEligible
        );
        assert_eq!(
            fetch_round(&storage, task_id, 1).status,
            RoundStatus::Failed
        );
        assert_eq!(
            fetch_round(&storage, task_id, 2).status,
            RoundStatus::Pending
        );
        assert_eq!(fetch_task(&storage, task_id).status, TaskStatus::Failed);
    }

    #[test]
    fn reopen_rejects_inconsistent_task_round_pair_fail_closed() {
        let (dir, mut storage) = open_query_storage("reopen-inconsistent");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-inconsistent"))
            .expect("create");
        fail_current_round(
            &mut storage,
            task_id,
            &project,
            1,
            RECOVERABLE_FAILED_ERROR_CODE,
        );
        execute(
            &path,
            &format!(
                "UPDATE rounds SET project_id = 'other-project' \
                 WHERE task_id = '{task_id}' AND round_number = 1"
            ),
        );

        let error = storage
            .reopen_failed_round(task_id)
            .expect_err("inconsistent pair must fail closed");
        assert!(
            matches!(error, RoundUpdateError::InvalidPersistedState),
            "{error:?}"
        );
        assert_eq!(fetch_task(&storage, task_id).status, TaskStatus::Failed);
        assert_eq!(
            fetch_round(&storage, task_id, 1).status,
            RoundStatus::Failed
        );
        assert!(
            event_rows(&storage, task_id)
                .iter()
                .all(|event| event.1 != "reopened")
        );
    }

    #[test]
    fn reopen_is_all_or_nothing_on_event_failure() {
        let (dir, mut storage) = open_query_storage("reopen-rollback");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-rb-reopen"))
            .expect("create");
        fail_current_round(
            &mut storage,
            task_id,
            &project,
            1,
            RECOVERABLE_FAILED_ERROR_CODE,
        );
        let task_before = fetch_task(&storage, task_id);
        let round_before = fetch_round(&storage, task_id, 1);
        execute(&path, "DROP TABLE events");

        let error = storage
            .reopen_failed_round(task_id)
            .expect_err("event insert must fail");
        assert!(matches!(error, RoundUpdateError::Database(_)), "{error:?}");
        assert_eq!(fetch_task(&storage, task_id), task_before);
        assert_eq!(fetch_round(&storage, task_id, 1), round_before);
    }

    #[test]
    fn reopen_errors_do_not_leak_input() {
        const SECRET: &str = "reopen-secret-token";
        let (dir, mut storage) = open_query_storage("reopen-error-safety");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-safe-reopen"))
            .expect("create");
        fail_current_round(
            &mut storage,
            task_id,
            &project,
            1,
            RECOVERABLE_FAILED_ERROR_CODE,
        );

        execute(
            &path,
            &format!(
                "UPDATE rounds SET kind = '{SECRET}' \
                 WHERE task_id = '{task_id}' AND round_number = 1"
            ),
        );
        let error = storage
            .reopen_failed_round(task_id)
            .expect_err("corrupt kind must fail closed");
        assert!(matches!(error, RoundUpdateError::RoundRow(_)), "{error:?}");
        assert_round_update_error_is_safe(&error, SECRET);

        execute(
            &path,
            &format!(
                "UPDATE rounds SET kind = 'implement', status = '{SECRET}' \
                 WHERE task_id = '{task_id}' AND round_number = 1"
            ),
        );
        let error = storage
            .reopen_failed_round(task_id)
            .expect_err("corrupt status must fail closed");
        assert!(matches!(error, RoundUpdateError::RoundRow(_)), "{error:?}");
        assert_round_update_error_is_safe(&error, SECRET);
    }

    #[test]
    fn concurrent_reopen_failed_round_yields_one_winner() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("reopen-concurrent");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        {
            let mut storage = connect(&path).expect("connect");
            storage
                .create_task(create_task_input(task_id, &project, "req-conc-reopen"))
                .expect("create");
            fail_current_round(
                &mut storage,
                task_id,
                &project,
                1,
                RECOVERABLE_FAILED_ERROR_CODE,
            );
        }
        drop(connect(&path).expect("pre-create WAL database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut storage = connect(path.as_path()).expect("connect");
                storage.reopen_failed_round(task_id)
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread must not panic"))
            .collect();
        let winners = results
            .iter()
            .filter(|result| matches!(result, Ok(ReopenFailedRoundOutcome::Reopened(_))))
            .count();
        let noops = results
            .iter()
            .filter(|result| matches!(result, Ok(ReopenFailedRoundOutcome::NotEligible)))
            .count();
        assert_eq!(winners, 1, "{results:?}");
        assert_eq!(noops, 3, "{results:?}");

        let storage = connect(path.as_path()).expect("connect verify");
        assert_eq!(
            fetch_task(&storage, task_id).status,
            TaskStatus::Implementing
        );
        assert_eq!(
            fetch_round(&storage, task_id, 1).status,
            RoundStatus::Observing
        );
        assert_eq!(count_rows(&storage, "events"), 3);
    }

    // -----------------------------------------------------------------------
    // Cooperative close (task 3.9d).
    // -----------------------------------------------------------------------

    #[test]
    fn request_task_close_persists_request_and_single_event() {
        let (_dir, mut storage) = open_query_storage("close-request-happy");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-close-happy"))
            .expect("create");
        let events_before = event_rows(&storage, task_id);

        let outcome = storage
            .request_task_close(task_id, "manual recovery")
            .expect("request close must succeed");
        let task = match &outcome {
            RequestTaskCloseOutcome::Requested(task) => task,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert!(outcome.is_close_requested());
        assert_eq!(task.status, TaskStatus::Implementing);
        assert_eq!(task.close_reason.as_deref(), Some("manual recovery"));
        let requested_at = task
            .close_requested_at
            .clone()
            .expect("close_requested_at must be set");
        assert!(is_rfc3339_millis_utc(&requested_at));
        assert_eq!(task.updated_at, requested_at);

        let events = event_rows(&storage, task_id);
        assert_eq!(events.len(), events_before.len() + 1);
        let new_event = events.last().expect("new event");
        assert_eq!(new_event.0, None);
        assert_eq!(new_event.1, "close_requested");
        assert_eq!(new_event.2, "task close requested");

        let created_at: String = storage
            .connection()
            .query_row(
                "SELECT created_at FROM events WHERE task_id = ?1 AND kind = 'close_requested'",
                rusqlite::params![task_id.to_string()],
                |row| row.get(0),
            )
            .expect("event created_at");
        assert_eq!(created_at, requested_at);
    }

    #[test]
    fn request_task_close_repeat_is_idempotent() {
        let (_dir, mut storage) = open_query_storage("close-request-repeat");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-close-repeat"))
            .expect("create");
        storage
            .request_task_close(task_id, "first reason")
            .expect("first request");
        let task_before = fetch_task(&storage, task_id);
        let events_before = event_rows(&storage, task_id);

        let outcome = storage
            .request_task_close(task_id, "second reason")
            .expect("repeat must succeed");
        let task = match &outcome {
            RequestTaskCloseOutcome::AlreadyRequested(task) => task,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert_eq!(task.close_reason.as_deref(), Some("first reason"));
        assert_eq!(task.close_requested_at, task_before.close_requested_at);
        assert_eq!(task.updated_at, task_before.updated_at);
        assert_eq!(fetch_task(&storage, task_id), task_before);
        assert_eq!(event_rows(&storage, task_id), events_before);
    }

    #[test]
    fn request_task_close_unknown_and_terminal_are_typed_noops() {
        let (_dir, mut storage) = open_query_storage("close-request-noop");
        assert_eq!(
            storage
                .request_task_close(query_task_id(9), "x")
                .expect("unknown task"),
            RequestTaskCloseOutcome::UnknownTask
        );
        assert_eq!(count_rows(&storage, "events"), 0);

        let project = query_project(QUERY_PROJECT_A);
        for (index, status) in [TaskStatus::Accepted, TaskStatus::Closed]
            .into_iter()
            .enumerate()
        {
            let task_id = query_task_id(2 + u32::try_from(index).expect("index"));
            insert_query_task(&storage, task_id, &project, status, TS_EARLY, TS_MID);
            let events_before = event_rows(&storage, task_id);

            assert_eq!(
                storage
                    .request_task_close(task_id, "x")
                    .expect("terminal task"),
                RequestTaskCloseOutcome::Terminal(status)
            );
            let task = fetch_task(&storage, task_id);
            assert_eq!(task.close_requested_at, None, "{status}");
            assert_eq!(task.close_reason, None, "{status}");
            assert_eq!(event_rows(&storage, task_id), events_before, "{status}");
        }
    }

    #[test]
    fn request_task_close_truncates_reason_unicode_safe() {
        let (_dir, mut storage) = open_query_storage("close-request-unicode");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-unicode"))
            .expect("create");

        let emoji = "\u{1F600}";
        let reason = emoji.repeat(CLOSE_REASON_MAX_CHARS + 50);
        let outcome = storage
            .request_task_close(task_id, &reason)
            .expect("request close");
        let task = outcome.task().expect("requested task");
        let stored = task.close_reason.as_deref().expect("stored reason");
        assert_eq!(stored.chars().count(), CLOSE_REASON_MAX_CHARS);
        assert_eq!(stored, emoji.repeat(CLOSE_REASON_MAX_CHARS));
        assert_eq!(
            stored.len(),
            CLOSE_REASON_MAX_CHARS * emoji.len(),
            "truncation must count Unicode scalar values, not bytes"
        );

        let ascii_id = query_task_id(2);
        let project_b = query_project(QUERY_PROJECT_B);
        storage
            .create_task(create_task_input(ascii_id, &project_b, "req-ascii"))
            .expect("create ascii");
        let ascii = "a".repeat(CLOSE_REASON_MAX_CHARS + 1);
        let outcome = storage
            .request_task_close(ascii_id, &ascii)
            .expect("request ascii close");
        assert_eq!(
            outcome.task().expect("task").close_reason.as_deref(),
            Some("a".repeat(CLOSE_REASON_MAX_CHARS).as_str())
        );
    }

    #[test]
    fn complete_requested_close_closes_with_reason_and_fallback() {
        let (dir, mut storage) = open_query_storage("close-complete-happy");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-complete"))
            .expect("create");
        storage
            .request_task_close(task_id, "manual recovery")
            .expect("request");

        let outcome = storage
            .complete_requested_close(task_id)
            .expect("complete must succeed");
        let task = match &outcome {
            CompleteRequestedCloseOutcome::Closed(task) => task,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert_eq!(task.status, TaskStatus::Closed);
        assert_eq!(task.close_reason.as_deref(), Some("manual recovery"));

        let events = event_rows(&storage, task_id);
        let closed_events: Vec<_> = events.iter().filter(|event| event.1 == "closed").collect();
        assert_eq!(closed_events.len(), 1);
        assert_eq!(closed_events[0].0, None);
        assert_eq!(closed_events[0].2, "task closed: manual recovery");

        let created_at: String = storage
            .connection()
            .query_row(
                "SELECT created_at FROM events WHERE task_id = ?1 AND kind = 'closed'",
                rusqlite::params![task_id.to_string()],
                |row| row.get(0),
            )
            .expect("closed event created_at");
        assert_eq!(created_at, task.updated_at);

        // An empty persisted reason uses the exact fallback.
        let empty_id = query_task_id(2);
        let project_b = query_project(QUERY_PROJECT_B);
        storage
            .create_task(create_task_input(empty_id, &project_b, "req-empty"))
            .expect("create empty");
        storage
            .request_task_close(empty_id, "")
            .expect("request empty");
        let outcome = storage
            .complete_requested_close(empty_id)
            .expect("complete empty");
        assert!(matches!(outcome, CompleteRequestedCloseOutcome::Closed(_)));
        let events = event_rows(&storage, empty_id);
        let closed: Vec<_> = events.iter().filter(|event| event.1 == "closed").collect();
        assert_eq!(closed[0].2, format!("task closed: {CLOSE_REASON_FALLBACK}"));

        // A missing persisted reason (NULL) also uses the fallback.
        let null_id = query_task_id(3);
        let project_c = query_project("query-project-c");
        storage
            .create_task(create_task_input(null_id, &project_c, "req-null"))
            .expect("create null");
        execute(
            &path,
            &format!(
                "UPDATE tasks SET close_requested_at = '2026-01-01T00:00:00.000+00:00' \
                 WHERE task_id = '{null_id}'"
            ),
        );
        let outcome = storage
            .complete_requested_close(null_id)
            .expect("complete null");
        assert!(matches!(outcome, CompleteRequestedCloseOutcome::Closed(_)));
        let events = event_rows(&storage, null_id);
        let closed: Vec<_> = events.iter().filter(|event| event.1 == "closed").collect();
        assert_eq!(closed[0].2, format!("task closed: {CLOSE_REASON_FALLBACK}"));
    }

    #[test]
    fn complete_requested_close_noop_cases_write_nothing() {
        let (_dir, mut storage) = open_query_storage("close-complete-noop");
        assert_eq!(
            storage
                .complete_requested_close(query_task_id(9))
                .expect("unknown task"),
            CompleteRequestedCloseOutcome::UnknownTask
        );

        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-noop"))
            .expect("create");
        let before = fetch_task(&storage, task_id);
        assert_eq!(
            storage
                .complete_requested_close(task_id)
                .expect("no request"),
            CompleteRequestedCloseOutcome::NoCloseRequest
        );
        assert_eq!(fetch_task(&storage, task_id), before);
        assert_eq!(count_rows(&storage, "events"), 1);

        let terminal_id = query_task_id(2);
        insert_query_task(
            &storage,
            terminal_id,
            &project,
            TaskStatus::Accepted,
            TS_EARLY,
            TS_MID,
        );
        assert_eq!(
            storage
                .complete_requested_close(terminal_id)
                .expect("terminal task"),
            CompleteRequestedCloseOutcome::Terminal(TaskStatus::Accepted)
        );

        storage
            .request_task_close(task_id, "r")
            .expect("request close");
        assert!(matches!(
            storage
                .complete_requested_close(task_id)
                .expect("first close"),
            CompleteRequestedCloseOutcome::Closed(_)
        ));
        let events_after_first = event_rows(&storage, task_id);
        assert_eq!(
            storage
                .complete_requested_close(task_id)
                .expect("repeat close"),
            CompleteRequestedCloseOutcome::Terminal(TaskStatus::Closed)
        );
        assert_eq!(event_rows(&storage, task_id), events_after_first);
    }

    #[test]
    fn finish_round_honours_pending_close_and_ignores_caller_task_status() {
        let (_dir, mut storage) = open_query_storage("close-finish-round");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-finish-close"))
            .expect("create");
        storage
            .mark_round_observing(round_ref(task_id, &project, 1))
            .expect("observing");
        storage
            .request_task_close(task_id, "stuck")
            .expect("request close");
        let events_before = event_rows(&storage, task_id);

        let outcome = storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_id, &project, 1),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: Some("msg-close".to_owned()),
                response: Some("answer".to_owned()),
                error_code: None,
                result_json: None,
            })
            .expect("finish must succeed");

        // The requested round fields/status are always written.
        assert_eq!(outcome.round.status, RoundStatus::Complete);
        assert_eq!(outcome.round.response.as_deref(), Some("answer"));
        // The caller-supplied task status is overridden by the pending close.
        assert_eq!(outcome.task.status, TaskStatus::Closed);
        assert_eq!(outcome.round.updated_at, outcome.task.updated_at);

        let events = event_rows(&storage, task_id);
        assert_eq!(events.len(), events_before.len() + 1);
        let new_event = events.last().expect("new event");
        assert_eq!(new_event.0, None);
        assert_eq!(new_event.1, "closed");
        assert_eq!(new_event.2, "task closed: stuck");
        assert!(
            events.iter().all(|event| event.1 != "complete"),
            "no round-finished event may be written for a pending close"
        );
    }

    #[test]
    fn finish_round_pending_close_validates_effective_transition() {
        let (dir, mut storage) = open_query_storage("close-finish-transition");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-transition"))
            .expect("create");
        storage
            .mark_round_observing(round_ref(task_id, &project, 1))
            .expect("observing");
        // A stale pending close on a terminal task must not be applied as an
        // illegal `accepted -> closed` transition.
        execute(
            &path,
            &format!(
                "UPDATE tasks SET status = 'accepted', \
                 close_requested_at = '2026-01-01T00:00:00.000+00:00', close_reason = 'stale' \
                 WHERE task_id = '{task_id}'"
            ),
        );
        let rounds_before = dump_rows(&storage, "rounds");
        let tasks_before = dump_rows(&storage, "tasks");
        let events_before = event_rows(&storage, task_id);

        let error = storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_id, &project, 1),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::Closed,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect_err("accepted -> closed must be rejected");
        assert!(
            matches!(error, RoundUpdateError::InvalidTaskTransition),
            "{error:?}"
        );
        assert_eq!(dump_rows(&storage, "rounds"), rounds_before);
        assert_eq!(dump_rows(&storage, "tasks"), tasks_before);
        assert_eq!(event_rows(&storage, task_id), events_before);
    }

    #[test]
    fn finish_round_without_close_keeps_round_finished_semantics() {
        let (_dir, mut storage) = open_query_storage("close-finish-none");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-finish-none"))
            .expect("create");
        storage
            .mark_round_observing(round_ref(task_id, &project, 1))
            .expect("observing");

        let outcome = storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_id, &project, 1),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect("finish");
        assert_eq!(outcome.round.status, RoundStatus::Complete);
        assert_eq!(outcome.task.status, TaskStatus::AwaitingReview);

        let events = event_rows(&storage, task_id);
        let finished: Vec<_> = events
            .iter()
            .filter(|event| event.1 == "complete")
            .collect();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].0, Some(1));
        assert_eq!(finished[0].2, "round finished: complete");
        assert!(events.iter().all(|event| event.1 != "closed"));
    }

    #[test]
    fn concurrent_request_task_close_yields_one_writer() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("close-request-concurrent");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        {
            let mut storage = connect(&path).expect("connect");
            storage
                .create_task(create_task_input(task_id, &project, "req-cc"))
                .expect("create");
        }
        drop(connect(&path).expect("pre-create WAL database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut storage = connect(path.as_path()).expect("connect");
                storage.request_task_close(task_id, "concurrent")
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread must not panic"))
            .collect();
        let requested = results
            .iter()
            .filter(|result| matches!(result, Ok(RequestTaskCloseOutcome::Requested(_))))
            .count();
        let already = results
            .iter()
            .filter(|result| matches!(result, Ok(RequestTaskCloseOutcome::AlreadyRequested(_))))
            .count();
        assert_eq!(requested, 1, "{results:?}");
        assert_eq!(already, 3, "{results:?}");

        let storage = connect(path.as_path()).expect("connect verify");
        let close_events = event_rows(&storage, task_id)
            .into_iter()
            .filter(|event| event.1 == "close_requested")
            .count();
        assert_eq!(close_events, 1);
    }

    #[test]
    fn concurrent_complete_requested_close_yields_one_closer() {
        use std::sync::{Arc, Barrier};

        let dir = TempDir::new("close-complete-concurrent");
        let path = dir.join("state.sqlite");
        create_v6(&path);
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        {
            let mut storage = connect(&path).expect("connect");
            storage
                .create_task(create_task_input(task_id, &project, "req-cc2"))
                .expect("create");
            storage
                .request_task_close(task_id, "concurrent")
                .expect("request");
        }
        drop(connect(&path).expect("pre-create WAL database"));

        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut storage = connect(path.as_path()).expect("connect");
                storage.complete_requested_close(task_id)
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread must not panic"))
            .collect();
        let closed = results
            .iter()
            .filter(|result| matches!(result, Ok(CompleteRequestedCloseOutcome::Closed(_))))
            .count();
        let terminal = results
            .iter()
            .filter(|result| matches!(result, Ok(CompleteRequestedCloseOutcome::Terminal(_))))
            .count();
        assert_eq!(closed, 1, "{results:?}");
        assert_eq!(terminal, 3, "{results:?}");

        let storage = connect(path.as_path()).expect("connect verify");
        let closed_events = event_rows(&storage, task_id)
            .into_iter()
            .filter(|event| event.1 == "closed")
            .count();
        assert_eq!(closed_events, 1);
    }

    #[test]
    fn request_task_close_rolls_back_on_event_failure_and_errors_are_safe() {
        const SECRET: &str = "close-request-secret-token";
        let (dir, mut storage) = open_query_storage("close-request-rollback");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-rb-close"))
            .expect("create");
        let before = fetch_task(&storage, task_id);
        execute(&path, "DROP TABLE events");

        let error = storage
            .request_task_close(task_id, SECRET)
            .expect_err("event insert must fail");
        assert!(matches!(error, RoundUpdateError::Database(_)), "{error:?}");
        assert_round_update_error_is_safe(&error, SECRET);
        assert_eq!(fetch_task(&storage, task_id), before);
    }

    #[test]
    fn complete_requested_close_rolls_back_on_event_failure_and_errors_are_safe() {
        const SECRET: &str = "close-complete-secret-token";
        let (dir, mut storage) = open_query_storage("close-complete-rollback");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-rb-complete"))
            .expect("create");
        storage
            .request_task_close(task_id, SECRET)
            .expect("request close");
        let before = fetch_task(&storage, task_id);
        execute(&path, "DROP TABLE events");

        let error = storage
            .complete_requested_close(task_id)
            .expect_err("event insert must fail");
        assert!(matches!(error, RoundUpdateError::Database(_)), "{error:?}");
        assert_round_update_error_is_safe(&error, SECRET);
        let after = fetch_task(&storage, task_id);
        assert_eq!(after.status, TaskStatus::Implementing);
        assert_eq!(after.close_requested_at, before.close_requested_at);
        assert_eq!(after.close_reason.as_deref(), Some(SECRET));
    }

    #[test]
    fn finish_round_pending_close_rolls_back_on_event_failure() {
        let (dir, mut storage) = open_query_storage("close-finish-rollback");
        let path = dir.join("state.sqlite");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-rb-finish"))
            .expect("create");
        storage
            .mark_round_observing(round_ref(task_id, &project, 1))
            .expect("observing");
        storage
            .request_task_close(task_id, "stuck")
            .expect("request close");
        let task_before = fetch_task(&storage, task_id);
        let round_before = fetch_round(&storage, task_id, 1);
        execute(&path, "DROP TABLE events");

        let error = storage
            .finish_round(FinishRoundInput {
                round: round_ref(task_id, &project, 1),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: None,
                response: None,
                error_code: None,
                result_json: None,
            })
            .expect_err("event insert must fail");
        assert!(matches!(error, RoundUpdateError::Database(_)), "{error:?}");
        assert_eq!(fetch_task(&storage, task_id), task_before);
        assert_eq!(fetch_round(&storage, task_id, 1), round_before);
    }

    #[test]
    fn request_task_close_blocks_reopen_failed_round() {
        let (_dir, mut storage) = open_query_storage("close-blocks-reopen");
        let project = query_project(QUERY_PROJECT_A);
        let task_id = query_task_id(1);
        storage
            .create_task(create_task_input(task_id, &project, "req-close-block"))
            .expect("create");
        fail_current_round(
            &mut storage,
            task_id,
            &project,
            1,
            RECOVERABLE_FAILED_ERROR_CODE,
        );
        assert!(matches!(
            storage
                .request_task_close(task_id, "stuck")
                .expect("request close"),
            RequestTaskCloseOutcome::Requested(_)
        ));

        assert_eq!(
            storage.reopen_failed_round(task_id).expect("reopen"),
            ReopenFailedRoundOutcome::NotEligible
        );
        assert_eq!(fetch_task(&storage, task_id).status, TaskStatus::Failed);
        assert_eq!(
            fetch_round(&storage, task_id, 1).status,
            RoundStatus::Failed
        );
        assert!(
            event_rows(&storage, task_id)
                .iter()
                .all(|event| event.1 != "reopened")
        );
    }

    /// Writes a Python-like schema v6 state with one distinctive task row and
    /// returns its exact bytes. This stands in for a Python-owned state root
    /// that Rust must never read, copy or modify.
    fn write_python_state(path: &Path) -> Vec<u8> {
        create_v6(path);
        execute(
            path,
            "INSERT INTO tasks (task_id, project_id, workspace, status, task, allowed_paths, \
             test_commands, created_at, updated_at, revision_count) VALUES \
             ('python-task', 'python-proj', '/python/workspace', 'implementing', 'python task', \
             '[]', '[]', '2026-01-01T00:00:00.000+00:00', '2026-01-01T00:00:00.000+00:00', 0);",
        );
        std::fs::read(path).expect("read python state bytes")
    }

    fn demo_layout(root: &Path) -> RustStateLayout {
        let project = ProjectId::from_str("demo").expect("demo project id");
        RustStateLayout::new(root.to_path_buf(), project).expect("layout must be built")
    }

    #[test]
    fn rust_state_layout_confines_every_runtime_path_to_the_rust_root() {
        let rust_root = TempDir::new("layout-rust");
        let python_root = TempDir::new("layout-python");
        let layout = demo_layout(&rust_root.path);

        let mut paths = vec![
            layout.project_dir(),
            layout.database(),
            layout.runtime_lock(),
        ];
        for kind in RuntimeLock::ALL {
            paths.push(layout.lock(kind));
        }
        for kind in RuntimeProcess::ALL {
            paths.push(layout.ownership_record(kind));
        }
        for kind in RuntimeLog::ALL {
            paths.push(layout.log(kind));
        }
        paths.push(layout.token_file("mcp.token").expect("token file path"));
        paths.push(layout.endpoint_record("mcp.json").expect("endpoint path"));

        for path in &paths {
            assert!(
                path.starts_with(&rust_root.path),
                "runtime path escaped the rust root: {path:?}"
            );
            assert!(
                !path.starts_with(&python_root.path),
                "runtime path entered the python root: {path:?}"
            );
        }

        let project_dir = rust_root.path.join("demo");
        assert_eq!(layout.project_dir(), project_dir);
        assert_eq!(layout.database(), project_dir.join("state.sqlite"));
        assert_eq!(layout.runtime_lock(), rust_root.path.join("runtime.lock"));
        assert_eq!(layout.lock(RuntimeLock::Mcp), project_dir.join("mcp.lock"));
        assert_eq!(
            layout.lock(RuntimeLock::Worker),
            project_dir.join("worker.lock")
        );
        assert_eq!(
            layout.ownership_record(RuntimeProcess::Worker),
            project_dir.join("worker.process.json")
        );
        assert_eq!(
            layout.log(RuntimeLog::McpServer),
            project_dir.join("mcp.server.log")
        );
        assert_eq!(
            layout.token_file("mcp.token").expect("token file path"),
            rust_root.path.join("secrets").join("mcp.token")
        );
        assert_eq!(
            layout.endpoint_record("mcp.json").expect("endpoint path"),
            rust_root.path.join("endpoints").join("mcp.json")
        );
    }

    #[test]
    fn rust_state_layout_rejects_escaping_components_without_leaking_them() {
        let root = TempDir::new("layout-escape");
        for id in ["..", "a/b", "/abs", "."] {
            let project = ProjectId::from_str(id).expect("non-empty project id parses");
            let error = RustStateLayout::new(root.path.clone(), project)
                .expect_err("unsafe project id must be rejected");
            assert!(
                matches!(error, StateLayoutError::UnsafeProjectId),
                "{error:?}"
            );
            let message = error.to_string();
            assert!(!message.contains(id), "project id leaked: {message}");
        }

        let layout = demo_layout(&root.path);
        for name in ["..", "a/b", "/abs"] {
            let error = layout
                .token_file(name)
                .expect_err("unsafe token name must be rejected");
            assert!(
                matches!(error, StateLayoutError::InvalidArtifactName),
                "{error:?}"
            );
            let message = error.to_string();
            assert!(!message.contains(name), "artifact name leaked: {message}");
        }
        assert!(matches!(
            layout
                .token_file("")
                .expect_err("empty name must be rejected"),
            StateLayoutError::InvalidArtifactName
        ));
    }

    #[test]
    fn rust_initialize_creates_only_its_own_empty_v15_state() {
        let rust_root = TempDir::new("isolate-rust");
        let python_root = TempDir::new("isolate-python");
        let python_state = python_root.join("state.sqlite");
        let python_before = write_python_state(&python_state);

        let layout = demo_layout(&rust_root.path);
        let rust_db = layout.database();
        assert!(!rust_db.exists(), "rust state must not exist yet");

        layout.initialize().expect("rust initialize must succeed");

        assert!(rust_db.exists(), "rust initialize must create its database");
        assert_compatible_empty_current(&rust_db);

        assert_eq!(
            std::fs::read(&python_state).expect("read python state bytes"),
            python_before,
            "rust initialize changed python state bytes"
        );
        let python = Connection::open(&python_state).expect("open python state");
        let python_tasks: i64 = python
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .expect("count python tasks");
        assert_eq!(python_tasks, 1, "python history must remain untouched");

        let rust = Connection::open(&rust_db).expect("open rust state");
        let rust_tasks: i64 = rust
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .expect("count rust tasks");
        assert_eq!(rust_tasks, 0, "new rust state must not contain python rows");
    }

    #[test]
    fn rust_writes_stay_in_rust_state_and_never_touch_python_state() {
        let rust_root = TempDir::new("isolate-writes-rust");
        let python_root = TempDir::new("isolate-writes-python");
        let python_state = python_root.join("state.sqlite");
        let python_before = write_python_state(&python_state);

        let layout = demo_layout(&rust_root.path);
        layout.initialize().expect("rust initialize must succeed");

        let project = ProjectId::from_str("demo").expect("demo project id");
        let task_id = TaskId::from_str(VALID_TASK_ID).expect("valid task id");
        {
            let mut storage = connect(layout.database()).expect("connect rust state");
            let outcome = storage
                .create_task(create_task_input(task_id, &project, "rust-request"))
                .expect("create rust task");
            assert!(outcome.is_created(), "{outcome:?}");
            assert_eq!(count_rows(&storage, "tasks"), 1);
        }

        assert_eq!(
            std::fs::read(&python_state).expect("read python state bytes"),
            python_before,
            "rust writes changed python state bytes"
        );
        let python = Connection::open(&python_state).expect("open python state");
        let python_tasks: i64 = python
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .expect("count python tasks");
        assert_eq!(python_tasks, 1, "python history must remain untouched");
    }

    #[test]
    fn rust_reinitialize_preserves_existing_rust_rows() {
        let rust_root = TempDir::new("reinit-rust");
        let layout = demo_layout(&rust_root.path);
        layout.initialize().expect("first initialize must succeed");

        let project = ProjectId::from_str("demo").expect("demo project id");
        let task_id = TaskId::from_str(VALID_TASK_ID).expect("valid task id");
        {
            let mut storage = connect(layout.database()).expect("connect rust state");
            storage
                .create_task(create_task_input(task_id, &project, "rust-request"))
                .expect("create rust task");
        }

        layout
            .initialize()
            .expect("second initialize must be an idempotent no-op");

        let storage = connect(layout.database()).expect("reconnect rust state");
        let task = storage
            .get_task(task_id)
            .expect("get_task must succeed")
            .expect("rust task must be preserved");
        assert_eq!(task.task_id, task_id);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);
    }

    // -----------------------------------------------------------------------
    // Ownership/format marker and fail-closed guard (task 3.11).
    // -----------------------------------------------------------------------

    /// Creates the project state directory that `create_v6`/marker writes need.
    fn ensure_project_dir(layout: &RustStateLayout) {
        std::fs::create_dir_all(layout.project_dir()).expect("create project state dir");
    }

    /// Writes a sidecar marker with explicit fields, bypassing production code.
    fn write_marker_fields(
        layout: &RustStateLayout,
        implementation: &str,
        format_version: u64,
        project_id: &str,
    ) {
        let value = serde_json::json!({
            "implementation": implementation,
            "format_version": format_version,
            "project_id": project_id,
            "state_root": encode_state_root(layout.state_root()),
        });
        write_marker_json(layout, &value);
    }

    /// Writes an arbitrary sidecar marker JSON value, bypassing production code.
    fn write_marker_json(layout: &RustStateLayout, value: &Value) {
        ensure_project_dir(layout);
        std::fs::write(
            layout.marker(),
            serde_json::to_vec(value).expect("serialize marker"),
        )
        .expect("write marker");
    }

    /// The exact bytes of a file, or `None` when it does not exist.
    fn file_bytes(path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    /// Asserts that neither the database nor the sidecar marker changed.
    fn assert_state_unchanged(
        layout: &RustStateLayout,
        before_db: &Option<Vec<u8>>,
        before_marker: &Option<Vec<u8>>,
    ) {
        assert_eq!(
            &file_bytes(&layout.database()),
            before_db,
            "database bytes changed"
        );
        assert_eq!(
            &file_bytes(&layout.marker()),
            before_marker,
            "marker bytes changed"
        );
    }

    /// Reads `meta.runtime_owner`, or `None` when the row is absent.
    fn runtime_owner(storage: &StorageConnection) -> Option<String> {
        storage
            .connection()
            .query_row(
                "SELECT value FROM meta WHERE key = 'runtime_owner'",
                [],
                |row| row.get(0),
            )
            .ok()
    }

    #[test]
    fn encode_state_root_normalization_is_component_aware_and_collision_free() {
        // The expected values are the exact uppercase-hex keys of the normalized
        // paths, which pins both the component-aware normalization and the
        // stable Unix byte encoding of the persisted marker field.
        let cases = [
            (".", ""),
            ("a/../b", "62"),
            ("../a", "2E2E2F61"),
            ("../../a", "2E2E2F2E2E2F61"),
            ("a/../../b", "2E2E2F62"),
            ("a", "61"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                encode_state_root(Path::new(input)),
                expected,
                "unexpected namespace key for {input:?}"
            );
        }

        // Distinct relative roots must never share a key: unmatched leading
        // `..` components are preserved, so they cannot collapse onto `a`.
        assert_ne!(
            encode_state_root(Path::new("../a")),
            encode_state_root(Path::new("a"))
        );
        assert_ne!(
            encode_state_root(Path::new("../../a")),
            encode_state_root(Path::new("a"))
        );
        assert_ne!(
            encode_state_root(Path::new("../../a")),
            encode_state_root(Path::new("../a"))
        );
    }

    #[test]
    fn rust_initialize_writes_ownership_markers_and_open_succeeds() {
        let root = TempDir::new("marker-fresh");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("rust initialize must succeed");

        assert!(layout.marker().exists(), "sidecar marker must be created");
        let marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(layout.marker()).expect("read marker"))
                .expect("marker must be valid JSON");
        assert_eq!(marker["implementation"].as_str(), Some("rust"));
        assert_eq!(marker["format_version"].as_u64(), Some(1));
        assert_eq!(marker["project_id"].as_str(), Some("demo"));

        let storage = layout.open().expect("open rust state");
        assert_eq!(runtime_owner(&storage).as_deref(), Some(RUNTIME_OWNER));
        let journal_mode: String = storage
            .connection()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read journal_mode");
        assert_eq!(journal_mode, "wal");
        let user_version: i64 = storage
            .connection()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version");
        assert_eq!(user_version, super::RUST_SCHEMA_VERSION);
        drop(storage);

        assert_compatible_empty_current(&layout.database());
    }

    #[test]
    fn rust_open_after_close_preserves_markers_and_rows() {
        let root = TempDir::new("marker-reopen");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("initialize");
        let project = ProjectId::from_str("demo").expect("project");
        let task_id = TaskId::from_str(VALID_TASK_ID).expect("task id");

        {
            let mut storage = layout.open().expect("first open");
            storage
                .create_task(create_task_input(task_id, &project, "rust-request"))
                .expect("create task");
        }
        let marker_before = file_bytes(&layout.marker());

        let storage = layout.open().expect("reopen");
        let task = storage
            .get_task(task_id)
            .expect("get_task")
            .expect("task preserved");
        assert_eq!(task.task_id, task_id);
        assert_eq!(count_rows(&storage, "tasks"), 1);
        drop(storage);

        assert_eq!(
            file_bytes(&layout.marker()),
            marker_before,
            "marker changed on reopen"
        );
    }

    #[test]
    fn rust_reinitialize_preserves_markers_and_rows() {
        let root = TempDir::new("marker-idempotent");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("initialize");
        let project = ProjectId::from_str("demo").expect("project");
        let task_id = TaskId::from_str(VALID_TASK_ID).expect("task id");
        {
            let mut storage = layout.open().expect("open");
            storage
                .create_task(create_task_input(task_id, &project, "rust-request"))
                .expect("create task");
        }
        let marker_before = file_bytes(&layout.marker());
        let database_before = file_bytes(&layout.database());

        layout.initialize().expect("second initialize");

        assert_eq!(
            file_bytes(&layout.marker()),
            marker_before,
            "marker changed"
        );
        assert_eq!(
            file_bytes(&layout.database()),
            database_before,
            "database changed"
        );
        let storage = layout.open().expect("reopen");
        assert_eq!(runtime_owner(&storage).as_deref(), Some(RUNTIME_OWNER));
        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert_eq!(count_rows(&storage, "events"), 1);
    }

    #[test]
    fn rust_open_without_marker_fails_closed() {
        let root = TempDir::new("marker-missing");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("initialize");
        std::fs::remove_file(layout.marker()).expect("remove marker");
        let database_before = file_bytes(&layout.database());

        let error = layout.open().expect_err("missing marker must fail");
        assert!(matches!(error, RustStateError::MissingMarker), "{error:?}");

        let error = layout
            .initialize()
            .expect_err("unmarked state must not be adopted");
        assert!(matches!(error, RustStateError::UnmarkedState), "{error:?}");

        assert_eq!(
            file_bytes(&layout.database()),
            database_before,
            "database changed"
        );
        assert!(!layout.marker().exists(), "marker must not be recreated");
    }

    #[test]
    fn rust_open_with_malformed_marker_fails_closed() {
        let root = TempDir::new("marker-malformed");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("initialize");
        std::fs::write(layout.marker(), b"{not json").expect("write marker");
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout.open().expect_err("malformed marker must fail");
        assert!(
            matches!(error, RustStateError::MalformedMarker),
            "{error:?}"
        );
        let error = layout.initialize().expect_err("malformed marker must fail");
        assert!(
            matches!(error, RustStateError::MalformedMarker),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_state_rejects_foreign_implementation_without_writes() {
        let root = TempDir::new("marker-foreign-impl");
        let layout = demo_layout(&root.path);
        write_marker_fields(&layout, "python", 1, "demo");
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout.open().expect_err("foreign implementation must fail");
        assert!(
            matches!(error, RustStateError::ForeignImplementation),
            "{error:?}"
        );
        let error = layout
            .initialize()
            .expect_err("foreign implementation must fail");
        assert!(
            matches!(error, RustStateError::ForeignImplementation),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_state_rejects_unsupported_format_version_without_writes() {
        let root = TempDir::new("marker-format");
        let layout = demo_layout(&root.path);
        write_marker_fields(&layout, "rust", 999, "demo");
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout.open().expect_err("unsupported format must fail");
        assert!(
            matches!(
                error,
                RustStateError::UnsupportedFormatVersion { found: 999 }
            ),
            "{error:?}"
        );
        let error = layout
            .initialize()
            .expect_err("unsupported format must fail");
        assert!(
            matches!(
                error,
                RustStateError::UnsupportedFormatVersion { found: 999 }
            ),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_state_rejects_namespace_mismatch_without_writes() {
        let root = TempDir::new("marker-namespace");
        let layout = demo_layout(&root.path);
        write_marker_fields(&layout, "rust", 1, "other-project");
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout.open().expect_err("namespace mismatch must fail");
        assert!(
            matches!(error, RustStateError::NamespaceMismatch),
            "{error:?}"
        );
        let error = layout
            .initialize()
            .expect_err("namespace mismatch must fail");
        assert!(
            matches!(error, RustStateError::NamespaceMismatch),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_open_rejects_missing_runtime_owner_without_writes() {
        let root = TempDir::new("owner-missing");
        let layout = demo_layout(&root.path);
        ensure_project_dir(&layout);
        create_v6(&layout.database());
        write_marker_fields(&layout, "rust", 1, "demo");
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout.open().expect_err("missing runtime owner must fail");
        assert!(
            matches!(error, RustStateError::MissingRuntimeOwner),
            "{error:?}"
        );
        let error = layout
            .initialize()
            .expect_err("missing runtime owner must fail");
        assert!(
            matches!(error, RustStateError::MissingRuntimeOwner),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_open_rejects_foreign_runtime_owner_without_writes() {
        let root = TempDir::new("owner-foreign");
        let layout = demo_layout(&root.path);
        ensure_project_dir(&layout);
        create_v6(&layout.database());
        execute(
            &layout.database(),
            "INSERT INTO meta (key, value) VALUES ('runtime_owner', 'python');",
        );
        write_marker_fields(&layout, "rust", 1, "demo");
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout.open().expect_err("foreign runtime owner must fail");
        assert!(
            matches!(error, RustStateError::ForeignRuntimeOwner),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_state_rejects_sidecar_database_disagreement_without_writes() {
        let root = TempDir::new("disagreement");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("initialize");
        execute(
            &layout.database(),
            "DELETE FROM meta WHERE key = 'runtime_owner';",
        );
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout.open().expect_err("disagreement must fail");
        assert!(
            matches!(error, RustStateError::MissingRuntimeOwner),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_open_rejects_incompatible_schema_without_writes() {
        let root = TempDir::new("schema-incompatible");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("initialize");
        execute(&layout.database(), "DROP INDEX ix_events_task;");
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout.open().expect_err("incompatible schema must fail");
        assert!(
            matches!(error, RustStateError::IncompatibleSchema(_)),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_state_does_not_adopt_python_or_unmarked_state() {
        let root = TempDir::new("adopt");
        let layout = demo_layout(&root.path);
        ensure_project_dir(&layout);
        let python_before = write_python_state(&layout.database());

        let error = layout.open().expect_err("unmarked python state must fail");
        assert!(matches!(error, RustStateError::MissingMarker), "{error:?}");
        let error = layout
            .initialize()
            .expect_err("unmarked python state must not be adopted");
        assert!(matches!(error, RustStateError::UnmarkedState), "{error:?}");

        assert_eq!(
            file_bytes(&layout.database()),
            Some(python_before),
            "python state bytes changed"
        );
        assert!(
            !layout.marker().exists(),
            "marker must not be created for python state"
        );
    }

    #[test]
    fn rust_initialize_recovers_marker_without_database() {
        let root = TempDir::new("recover");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("initialize");
        let marker_before = file_bytes(&layout.marker());
        std::fs::remove_file(layout.database()).expect("remove database");

        let error = layout.open().expect_err("missing database must fail");
        assert!(
            matches!(error, RustStateError::MissingDatabase),
            "{error:?}"
        );
        assert!(
            !layout.database().exists(),
            "open must not recreate the database"
        );

        layout
            .initialize()
            .expect("initialize must recover the interrupted state");
        assert!(layout.database().exists(), "database must be recreated");
        assert_eq!(
            file_bytes(&layout.marker()),
            marker_before,
            "marker changed during recovery"
        );
        let storage = layout.open().expect("open recovered state");
        assert_eq!(count_rows(&storage, "tasks"), 0);
    }

    #[test]
    fn rust_initialize_recovers_marker_with_empty_database() {
        let root = TempDir::new("recover-empty");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("initialize");
        let marker_before = file_bytes(&layout.marker());
        std::fs::remove_file(layout.database()).expect("remove database");
        std::fs::write(layout.database(), b"").expect("create empty database file");

        let error = layout.open().expect_err("empty database must fail");
        assert!(
            matches!(error, RustStateError::IncompatibleSchema(_)),
            "{error:?}"
        );

        layout
            .initialize()
            .expect("initialize must recover the interrupted state");
        assert_eq!(
            file_bytes(&layout.marker()),
            marker_before,
            "marker changed during recovery"
        );
        let storage = layout.open().expect("open recovered state");
        assert_eq!(runtime_owner(&storage).as_deref(), Some(RUNTIME_OWNER));
        assert_eq!(count_rows(&storage, "tasks"), 0);
    }

    #[test]
    fn rust_initialize_never_overwrites_foreign_marker_or_state() {
        let root = TempDir::new("overwrite");
        let layout = demo_layout(&root.path);
        ensure_project_dir(&layout);
        create_v6(&layout.database());
        write_marker_fields(&layout, "python", 1, "demo");
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout
            .initialize()
            .expect_err("foreign marker must not be overwritten");
        assert!(
            matches!(error, RustStateError::ForeignImplementation),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_initialize_rejects_contradictory_marker_and_database() {
        let root = TempDir::new("contradictory");
        let layout = demo_layout(&root.path);
        ensure_project_dir(&layout);
        create_v6(&layout.database());
        write_marker_fields(&layout, "rust", 1, "demo");
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout
            .initialize()
            .expect_err("contradictory state must fail closed");
        assert!(
            matches!(error, RustStateError::MissingRuntimeOwner),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn generic_initialize_stays_free_of_runtime_owner() {
        let dir = TempDir::new("generic-init");
        let path = dir.join("state.sqlite");
        initialize(&path).expect("generic initialize");

        let connection = Connection::open(&path).expect("open generic state");
        let owners: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM meta WHERE key = 'runtime_owner'",
                [],
                |row| row.get(0),
            )
            .expect("count runtime_owner rows");
        assert_eq!(owners, 0, "generic initialize must not add a runtime owner");
        drop(connection);

        assert_compatible_empty_v6(&path);
    }

    #[test]
    fn rust_state_errors_do_not_leak_sensitive_values() {
        let root = TempDir::new("no-leak");
        let layout = demo_layout(&root.path);

        write_marker_fields(&layout, "python", 1, "secret-project");
        let error = layout.open().expect_err("foreign implementation must fail");
        let message = error.to_string();
        assert!(
            !message.contains("secret-project"),
            "namespace leaked: {message}"
        );
        assert!(
            !message.contains(&root.path.to_string_lossy().into_owned()),
            "path leaked: {message}"
        );

        write_marker_fields(&layout, "rust", 1, "secret-project");
        let error = layout.open().expect_err("namespace mismatch must fail");
        assert!(
            matches!(error, RustStateError::NamespaceMismatch),
            "{error:?}"
        );
        assert!(
            !error.to_string().contains("secret-project"),
            "namespace leaked: {}",
            error
        );
    }

    #[test]
    fn rust_state_rejects_missing_state_root_without_writes() {
        let root = TempDir::new("marker-missing-root");
        let layout = demo_layout(&root.path);
        write_marker_json(
            &layout,
            &serde_json::json!({
                "implementation": "rust",
                "format_version": 1,
                "project_id": "demo",
            }),
        );
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout.open().expect_err("missing state root must fail");
        assert!(
            matches!(error, RustStateError::MalformedMarker),
            "{error:?}"
        );
        let error = layout
            .initialize()
            .expect_err("missing state root must fail");
        assert!(
            matches!(error, RustStateError::MalformedMarker),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_state_rejects_state_copied_under_another_root() {
        let source_root = TempDir::new("copy-source");
        let other_root = TempDir::new("copy-other");
        let source = demo_layout(&source_root.path);
        source.initialize().expect("initialize source");

        let project = ProjectId::from_str("demo").expect("project");
        let other = RustStateLayout::new(other_root.path.clone(), project).expect("other layout");
        std::fs::create_dir_all(other.project_dir()).expect("create other project dir");
        std::fs::copy(source.database(), other.database()).expect("copy database");
        std::fs::copy(source.marker(), other.marker()).expect("copy marker");
        let database_before = file_bytes(&other.database());
        let marker_before = file_bytes(&other.marker());

        let error = other.open().expect_err("copied state must fail");
        assert!(
            matches!(error, RustStateError::NamespaceMismatch),
            "{error:?}"
        );
        let error = other
            .initialize()
            .expect_err("copied state must not be adopted");
        assert!(
            matches!(error, RustStateError::NamespaceMismatch),
            "{error:?}"
        );
        assert_state_unchanged(&other, &database_before, &marker_before);
    }

    #[test]
    fn rust_state_rejects_marker_with_foreign_state_root_without_writes() {
        let root = TempDir::new("marker-foreign-root");
        let other_root = TempDir::new("marker-foreign-root-other");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("initialize");
        let project = ProjectId::from_str("demo").expect("project");
        let other = RustStateLayout::new(other_root.path.clone(), project).expect("other layout");
        write_marker_fields(&other, "rust", 1, "demo");
        std::fs::copy(other.marker(), layout.marker()).expect("overwrite marker");
        let database_before = file_bytes(&layout.database());
        let marker_before = file_bytes(&layout.marker());

        let error = layout.open().expect_err("foreign state root must fail");
        assert!(
            matches!(error, RustStateError::NamespaceMismatch),
            "{error:?}"
        );
        let error = layout
            .initialize()
            .expect_err("foreign state root must fail");
        assert!(
            matches!(error, RustStateError::NamespaceMismatch),
            "{error:?}"
        );
        assert_state_unchanged(&layout, &database_before, &marker_before);
    }

    #[test]
    fn rust_concurrent_initialize_is_safe_and_consistent() {
        let root = TempDir::new("concurrent-init");
        let layout = demo_layout(&root.path);
        let workers = 8;
        // Pre-create the database in WAL mode so threads contend only on the
        // marker no-clobber and the writer transaction, not on the generic
        // journal-mode switch (see `concurrent_initialize_serializes_to_one_schema`).
        drop(connect(layout.database()).expect("pre-create rust database"));

        let results: Vec<Result<(), RustStateError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    let layout = layout.clone();
                    scope.spawn(move || layout.initialize())
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("initialize thread"))
                .collect()
        });

        for result in &results {
            assert!(result.is_ok(), "concurrent initialize failed: {result:?}");
        }

        let marker: Value =
            serde_json::from_slice(&std::fs::read(layout.marker()).expect("read marker"))
                .expect("marker must be valid JSON");
        assert_eq!(marker["implementation"].as_str(), Some("rust"));
        let expected_root = encode_state_root(&root.path);
        assert_eq!(marker["state_root"].as_str(), Some(expected_root.as_str()));

        let storage = layout.open().expect("open after concurrent initialize");
        assert_eq!(runtime_owner(&storage).as_deref(), Some(RUNTIME_OWNER));
        assert_eq!(count_rows(&storage, "tasks"), 0);
        drop(storage);
        assert_compatible_empty_current(&layout.database());
    }

    fn expected_v15() -> Value {
        serde_json::from_str(include_str!(
            "../../../docs/fixtures/sqlite/expected-v15.json"
        ))
        .expect("v15 manifest")
    }

    fn assert_compatible_empty_current(path: &Path) {
        let observed = inspect(path).expect("inspect current");
        assert_eq!(observed.user_version(), super::RUST_SCHEMA_VERSION);
        assert_eq!(observed.meta_schema_version(), "17");
        let mut expected = expected_contract(&expected_v15());
        expected
            .tables
            .iter_mut()
            .find(|table| table.name == "tasks")
            .unwrap()
            .columns
            .push(super::column("delivery_mode", "TEXT", true, 0));
        expected.tables.push(Table {
            name: "automation_runs".into(),
            columns: vec![
                super::column("run_id", "TEXT", false, 1),
                super::column("status", "TEXT", true, 0),
                super::column("control", "TEXT", true, 0),
                super::column("document", "TEXT", true, 0),
                super::column("created_at", "TEXT", true, 0),
                super::column("updated_at", "TEXT", true, 0),
            ],
        });
        expected.indexes.push(super::index(
            "ux_automation_unfinished",
            "automation_runs",
            &[""],
            true,
            true,
        ));
        assert_eq!(
            normalize(Contract {
                tables: observed.tables().to_vec(),
                indexes: observed.indexes().to_vec(),
                foreign_keys: observed.foreign_keys().to_vec(),
            }),
            normalize(expected)
        );
        let connection = Connection::open(path).unwrap();
        for table in observed
            .tables()
            .iter()
            .filter(|table| table.name != "meta")
        {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {}", table.name), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
    }

    fn assert_compatible_empty_v15(path: &Path) {
        let observed = inspect(path).expect("inspect v15");
        assert_eq!(observed.user_version(), 15);
        assert_eq!(observed.meta_schema_version(), "15");
        assert_eq!(
            normalize(Contract {
                tables: observed.tables().to_vec(),
                indexes: observed.indexes().to_vec(),
                foreign_keys: observed.foreign_keys().to_vec(),
            }),
            expected_contract(&expected_v15())
        );
        let connection = Connection::open(path).expect("open v15");
        // Defaults are independently frozen in the Python-generated manifest.
        for (table, columns) in expected_v15()["schema"]["tables"]
            .as_object()
            .expect("tables")
        {
            for column in columns.as_array().expect("columns") {
                let default: Option<String> = connection
                    .query_row(
                        "SELECT dflt_value FROM pragma_table_info(?1) WHERE name=?2",
                        rusqlite::params![table, column["name"].as_str().expect("name")],
                        |row| row.get(0),
                    )
                    .expect("read default");
                assert_eq!(
                    default.as_deref(),
                    column["default"].as_str(),
                    "{table}.{}",
                    column["name"]
                );
            }
        }
        for table in observed
            .tables()
            .iter()
            .filter(|table| table.name != "meta")
        {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {}", table.name), [], |row| {
                    row.get(0)
                })
                .expect("count v15 rows");
            assert_eq!(count, 0, "{} must be empty", table.name);
        }
    }

    #[test]
    fn v15_contract_matches_frozen_reference_and_fixture_is_read_only() {
        assert_eq!(
            normalize(super::schema_v15::contract()),
            expected_contract(&expected_v15())
        );
        let path = fixture_dir().join("empty-v15.sqlite");
        let before = file_bytes(&path);
        assert_compatible_empty_v15(&path);
        assert_eq!(file_bytes(&path), before);
    }

    #[test]
    fn rust_v6_upgrade_preserves_all_legacy_rows_and_null_semantics() {
        for fixture in FIXTURES {
            let root = TempDir::new("upgrade-legacy");
            let layout = demo_layout(&root.path);
            ensure_project_dir(&layout);
            let source = fixture_dir().join(fixture);
            let source_before = file_bytes(&source);
            std::fs::copy(&source, layout.database()).expect("copy legacy fixture");
            execute(
                &layout.database(),
                "INSERT INTO meta VALUES ('runtime_owner','rust')",
            );
            write_marker_fields(&layout, "rust", 1, "demo");
            let marker_before = file_bytes(&layout.marker());
            let old = Connection::open(layout.database()).expect("legacy");
            let snapshot = |connection: &Connection, table: &str, columns: &str| {
                let mut statement = connection
                    .prepare(&format!("SELECT {columns} FROM {table} ORDER BY rowid"))
                    .expect("snapshot");
                let width = statement.column_count();
                statement
                    .query_map([], |row| {
                        (0..width)
                            .map(|i| row.get::<_, SqlValue>(i))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .expect("query snapshot")
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .expect("rows")
            };
            let original: Vec<_> = ["tasks", "rounds", "events"]
                .iter()
                .map(|table| {
                    let columns = super::read_columns(&old, table)
                        .expect("legacy columns")
                        .into_iter()
                        .map(|column| column.name)
                        .collect::<Vec<_>>()
                        .join(",");
                    (*table, columns.clone(), snapshot(&old, table, &columns))
                })
                .collect();
            drop(old);
            layout.initialize().expect("guarded upgrade");
            let storage = layout.open().expect("upgraded state");
            assert_eq!(
                query_user_version(storage.connection()).expect("version"),
                super::RUST_SCHEMA_VERSION
            );
            for (table, columns, rows) in original {
                assert_eq!(snapshot(storage.connection(), table, &columns), rows);
            }
            let nondefault: i64 = storage.connection().query_row(
                "SELECT COUNT(*) FROM tasks WHERE execution_mode!='direct' OR budget_json IS NOT NULL OR workflow_id IS NOT NULL OR depends_on IS NOT NULL OR profile IS NOT NULL OR profile_json IS NOT NULL OR profile_hash IS NOT NULL OR profile_source IS NOT NULL",
                [], |row| row.get(0),
            ).expect("legacy task defaults");
            assert_eq!(nondefault, 0);
            let rounds: i64 = storage.connection().query_row(
                "SELECT COUNT(*) FROM rounds WHERE checkpoint_json IS NOT NULL OR structured_findings IS NOT NULL", [], |row| row.get(0),
            ).expect("legacy round defaults");
            assert_eq!(rounds, 0);
            for table in ["worktrees", "worktree_quarantine"] {
                assert_eq!(count_rows(&storage, table), 0);
            }
            let writers: i64 = storage.connection().query_row("SELECT COUNT(*) FROM tasks WHERE status IN ('implementing','awaiting_review','revising','needs_user','failed','delivery_unknown')", [], |row| row.get(0)).expect("legacy writers");
            assert_eq!(count_rows(&storage, "active_writers"), writers);
            drop(storage);
            let upgraded = file_bytes(&layout.database());
            layout.initialize().expect("idempotent v15");
            assert_state_unchanged(&layout, &upgraded, &marker_before);
            assert_eq!(file_bytes(&source), source_before);
        }
    }

    #[test]
    fn rust_v15_keeps_single_task_admission_after_index_removal() {
        let root = TempDir::new("v15-admission");
        let layout = demo_layout(&root.path);
        layout.initialize().expect("v15");
        let project = ProjectId::from_str("demo").expect("project");
        let mut storage = layout.open().expect("open");
        let first = TaskId::from_str(VALID_TASK_ID).expect("task");
        storage
            .create_task(create_task_input(first, &project, "first"))
            .expect("first task");
        let second = TaskId::from_str("22222222-2222-4222-8222-222222222222").expect("second");
        let error = storage
            .create_task(create_task_input(second, &project, "second"))
            .expect_err("busy");
        assert!(matches!(error, CreateTaskError::ProjectBusy));
        assert_eq!(count_rows(&storage, "tasks"), 1);
        assert_eq!(count_rows(&storage, "rounds"), 1);
        assert!(
            storage
                .create_task(create_task_input(first, &project, "first"))
                .expect("replay")
                .is_replayed()
        );
    }

    #[test]
    fn v15_additive_upgrade_rolls_back_every_ddl_and_marker_on_failure() {
        let root = TempDir::new("v15-rollback");
        let path = root.join("state.sqlite");
        create_v6(&path);
        let before = logical_state(&path);
        let mut connection = Connection::open(&path).expect("open");
        {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("transaction");
            super::schema_v15::upgrade(&transaction).expect("DDL");
            assert!(
                transaction
                    .execute_batch("CREATE TABLE tasks (bad TEXT)")
                    .is_err()
            );
            // Drop rolls back even after all version markers were written.
        }
        drop(connection);
        assert_eq!(logical_state(&path), before);
        assert_compatible_empty_v6(&path);
    }

    #[test]
    fn rust_v6_upgrade_metadata_failure_rolls_back_and_can_retry() {
        let root = TempDir::new("upgrade-failure");
        let layout = demo_layout(&root.path);
        ensure_project_dir(&layout);
        create_v6(&layout.database());
        execute(
            &layout.database(),
            "INSERT INTO meta VALUES ('runtime_owner','rust'); CREATE TRIGGER block_upgrade BEFORE UPDATE ON meta WHEN NEW.key='schema_version' BEGIN SELECT RAISE(ABORT,'blocked'); END",
        );
        write_marker_fields(&layout, "rust", 1, "demo");
        let marker = file_bytes(&layout.marker());
        let before = logical_state(&layout.database());
        assert!(matches!(
            layout.initialize(),
            Err(RustStateError::Database(_))
        ));
        assert_eq!(logical_state(&layout.database()), before);
        assert_compatible_empty_v6(&layout.database());
        assert_eq!(file_bytes(&layout.marker()), marker);
        execute(&layout.database(), "DROP TRIGGER block_upgrade");
        layout.initialize().expect("retry succeeds");
        assert_compatible_empty_current(&layout.database());
    }

    #[test]
    fn rust_concurrent_v6_upgrade_serializes_and_preserves_owner() {
        let root = TempDir::new("concurrent-upgrade");
        let layout = demo_layout(&root.path);
        ensure_project_dir(&layout);
        initialize(layout.database()).expect("legacy v6");
        execute(
            &layout.database(),
            "INSERT INTO meta VALUES ('runtime_owner','rust')",
        );
        write_marker_fields(&layout, "rust", 1, "demo");
        let marker = file_bytes(&layout.marker());
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| layout.initialize()))
                .collect();
            for handle in handles {
                handle.join().expect("thread").expect("concurrent upgrade");
            }
        });
        assert_compatible_empty_current(&layout.database());
        assert_eq!(file_bytes(&layout.marker()), marker);
        assert_eq!(
            runtime_owner(&layout.open().expect("open")).as_deref(),
            Some(RUNTIME_OWNER)
        );
    }

    #[test]
    fn rust_v15_rejects_wrong_defaults_and_writer_predicate_without_writes() {
        for mutation in [
            "DROP INDEX ux_active_writers_single; CREATE UNIQUE INDEX ux_active_writers_single ON active_writers(project_id) WHERE parallel=1",
            "ALTER TABLE active_writers RENAME TO old_writers; CREATE TABLE active_writers (task_id TEXT PRIMARY KEY, project_id TEXT NOT NULL, scopes_json TEXT NOT NULL, created_at TEXT NOT NULL, parallel INTEGER NOT NULL DEFAULT 1); DROP TABLE old_writers; CREATE INDEX ix_active_writers_project ON active_writers(project_id); CREATE UNIQUE INDEX ux_active_writers_single ON active_writers(project_id) WHERE parallel=0",
        ] {
            let root = TempDir::new("v15-reject");
            let layout = demo_layout(&root.path);
            layout.initialize().expect("fresh");
            execute(&layout.database(), mutation);
            let before = file_bytes(&layout.database());
            let marker = file_bytes(&layout.marker());
            assert!(matches!(
                layout.open(),
                Err(RustStateError::IncompatibleSchema(_))
            ));
            assert!(matches!(
                layout.initialize(),
                Err(RustStateError::IncompatibleSchema(_))
            ));
            assert_state_unchanged(&layout, &before, &marker);
        }
    }
}

// ---------------------------------------------------------------------------
// Atomic round/task lifecycle transitions (task 3.9a).
//
// The public API and its supporting types live together here; every method
// shares the local table-driven [`ROUND_TRANSITIONS`] validation and the
// [`TaskStatus::require_transition`] check for task status changes.
// ---------------------------------------------------------------------------

impl StorageConnection {
    /// Atomically creates the next sequential revision round of an existing
    /// task and moves the task to `revising`.
    ///
    /// The whole operation runs inside one `BEGIN IMMEDIATE` transaction. The
    /// task must exist and belong to `input.project_id`; the round number must
    /// be exactly one greater than the current maximum round number (so no
    /// duplicate or gap can be created); the request id must not already be
    /// used in the project; and the task status transition to
    /// [`TaskStatus::Revising`] must be allowed by
    /// [`TaskStatus::require_transition`]. The new round is always a
    /// [`RoundKind::Revise`] `pending` round with `attempted = 0`.
    ///
    /// On success the transaction writes the round, the `round created (revise)`
    /// event and the task update (`session_id = NULL`, `status = revising`,
    /// `revision_count + 1`) with one shared timestamp, and returns the persisted
    /// [`RoundRow`] and [`Task`] read back through the production mapping. A
    /// request that already exists is never replayed: it is a
    /// [`RoundUpdateError::RequestConflict`]. Every failure rolls the whole
    /// transaction back.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]). No error message
    /// contains ids, request id, payload hash, findings, session or message ids,
    /// SQL, JSON or paths.
    pub fn create_revision_round(
        &mut self,
        input: CreateRevisionRoundInput,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        self.create_revision_round_inner(input, None, false, false)
            .map(|outcome| outcome.state)
    }

    /// Creates a v7+ revision atomically with validated structured findings.
    /// Textual findings remain mandatory. Identical project-scoped request/hash
    /// retries return the original round without another event or task update.
    /// The caller must authorize paths and calculate the canonical payload hash.
    pub fn create_revision_round_with_findings(
        &mut self,
        input: CreateRevisionRoundInput,
        structured: Option<bridge_domain::StructuredFindings>,
    ) -> Result<RevisionRoundOutcome, RoundUpdateError> {
        if input
            .findings
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(RoundUpdateError::InvalidInput);
        }
        self.create_revision_round_inner(input, structured, true, false)
    }

    /// Explicit one-round budget override. The caller holds review fences and
    /// has evaluated budget exhaustion/corruption; other gates still apply.
    pub fn create_revision_round_with_budget_override(
        &mut self,
        input: CreateRevisionRoundInput,
        structured: Option<bridge_domain::StructuredFindings>,
    ) -> Result<RevisionRoundOutcome, RoundUpdateError> {
        if input
            .findings
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(RoundUpdateError::InvalidInput);
        }
        self.create_revision_round_inner(input, structured, true, true)
    }

    /// Read-only structured findings for a project-scoped round. SQL NULL is
    /// text-only; every non-NULL value must be a strictly validated JSON array.
    /// Scope authorization belongs to the consumer with its current workspace.
    pub fn get_round_structured_findings(
        &self,
        round: &RoundRef,
    ) -> Result<Option<bridge_domain::StructuredFindings>, RoundUpdateError> {
        let raw: Result<Option<String>, RoundUpdateError> = self.connection.query_row(
            "SELECT structured_findings FROM rounds WHERE task_id=?1 AND project_id=?2 AND round_number=?3",
            params![round.task_id.to_string(), round.project_id.as_str(), round.round_number],
            |row| Ok(match row.get_ref(0)? {
                rusqlite::types::ValueRef::Null => Ok(None),
                rusqlite::types::ValueRef::Text(bytes) => std::str::from_utf8(bytes)
                    .map(|s| Some(s.to_owned())).map_err(|_| RoundUpdateError::InvalidPersistedState),
                _ => Err(RoundUpdateError::InvalidPersistedState),
            }),
        ).optional().map_err(RoundUpdateError::Database)?
            .ok_or(RoundUpdateError::MissingRound)?;
        let raw = raw?;
        raw.map(|s| serde_json::from_str(&s).map_err(|_| RoundUpdateError::InvalidPersistedState))
            .transpose()
    }

    fn create_revision_round_inner(
        &mut self,
        input: CreateRevisionRoundInput,
        structured: Option<bridge_domain::StructuredFindings>,
        replay: bool,
        budget_override: bool,
    ) -> Result<RevisionRoundOutcome, RoundUpdateError> {
        if input.request_id.is_empty() || input.payload_hash.is_empty() || input.round_number == 0 {
            return Err(RoundUpdateError::InvalidInput);
        }
        let structured_json = structured
            .as_ref()
            .map(|value| {
                bridge_domain::StructuredFindings::try_from(value.as_slice().to_vec())
                    .map_err(|_| RoundUpdateError::InvalidInput)?;
                serde_json::to_string(value).map_err(|_| RoundUpdateError::InvalidJson)
            })
            .transpose()?;

        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let task = if budget_override {
            transaction
                .query_row(
                    "SELECT * FROM tasks WHERE task_id=?1",
                    [input.task_id.to_string()],
                    |row| Ok(Task::from_row_for_status(row)),
                )
                .optional()
                .map_err(RoundUpdateError::Database)?
                .ok_or(RoundUpdateError::MissingTask)?
                .map_err(RoundUpdateError::TaskRow)?
        } else {
            load_task_for_update(&transaction, input.task_id)?
        };
        if task.project_id != input.project_id {
            return Err(RoundUpdateError::ProjectMismatch);
        }

        let existing: Option<Result<RoundRow, RoundRowError>> = transaction
            .query_row(
                &format!(
                    "SELECT {ROUND_COLUMNS} FROM rounds WHERE project_id = ?1 AND request_id = ?2"
                ),
                params![input.project_id.as_str(), input.request_id],
                |row| Ok(RoundRow::from_row(row)),
            )
            .optional()
            .map_err(RoundUpdateError::Database)?;
        if let Some(existing) = existing {
            if !replay {
                return Err(RoundUpdateError::RequestConflict);
            }
            let existing = existing.map_err(RoundUpdateError::RoundRow)?;
            if replay
                && existing.task_id == input.task_id
                && existing.kind == RoundKind::Revise
                && existing.payload_hash == input.payload_hash
            {
                let outcome =
                    read_round_update_outcome(&transaction, input.task_id, existing.round_number)?;
                transaction.commit().map_err(RoundUpdateError::Database)?;
                return Ok(RevisionRoundOutcome {
                    state: outcome,
                    replayed: true,
                });
            }
            return Err(RoundUpdateError::RequestConflict);
        }

        let expected = current_round_number(&transaction, input.task_id)?
            .checked_add(1)
            .ok_or(RoundUpdateError::InvalidPersistedState)?;
        if input.round_number != expected {
            return Err(RoundUpdateError::NonSequentialRound);
        }

        if replay
            && (task.status != TaskStatus::AwaitingReview || task.close_requested_at.is_some())
        {
            return Err(RoundUpdateError::InvalidTaskTransition);
        }

        task.status
            .require_transition(TaskStatus::Revising)
            .map_err(|_| RoundUpdateError::InvalidTaskTransition)?;

        let revision_count = task
            .revision_count
            .checked_add(1)
            .ok_or(RoundUpdateError::InvalidPersistedState)?;

        transaction
            .execute(
                &format!(
                    "INSERT INTO rounds ({ROUND_COLUMNS}) VALUES \
                     (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)"
                ),
                params![
                    input.task_id.to_string(),
                    input.project_id.as_str(),
                    i64::from(input.round_number),
                    input.request_id,
                    input.payload_hash,
                    RoundKind::Revise.as_str(),
                    RoundStatus::Pending.as_str(),
                    Null,
                    0_i64,
                    Null,
                    Null,
                    Null,
                    Null,
                    input.findings,
                    Null,
                    Null,
                    Null,
                    Null,
                    Null,
                    now,
                    now,
                ],
            )
            .map_err(classify_revision_round_insert_error)?;

        if replay {
            transaction
                .execute(
                    "UPDATE rounds SET structured_findings=?1 WHERE task_id=?2 AND round_number=?3",
                    params![
                        structured_json,
                        input.task_id.to_string(),
                        input.round_number
                    ],
                )
                .map_err(RoundUpdateError::Database)?;
        }

        transaction
            .execute(
                "INSERT INTO events (task_id, round_number, kind, message, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    input.task_id.to_string(),
                    i64::from(input.round_number),
                    "created",
                    format!("round created ({})", RoundKind::Revise.as_str()),
                    now,
                ],
            )
            .map_err(RoundUpdateError::Database)?;

        transaction
            .execute(
                "UPDATE tasks SET session_id = NULL, status = ?1, revision_count = ?2, \
                 updated_at = ?3 WHERE task_id = ?4",
                params![
                    TaskStatus::Revising.as_str(),
                    revision_count,
                    now,
                    input.task_id.to_string(),
                ],
            )
            .map_err(RoundUpdateError::Database)?;

        if budget_override {
            transaction.execute("INSERT INTO events(task_id,round_number,kind,message,created_at) VALUES (?1,?2,'budget_override','explicit one-round budget override',?3)",params![input.task_id.to_string(),input.round_number,now]).map_err(RoundUpdateError::Database)?;
        }
        let outcome = read_round_update_outcome(&transaction, input.task_id, input.round_number)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(RevisionRoundOutcome {
            state: outcome,
            replayed: false,
        })
    }

    /// Atomically binds `session_id` to a round and makes it the task's current
    /// session.
    ///
    /// The round must exist, belong to `round.project_id`, and be the current
    /// (highest-numbered) round of the task. The `rounds.session_id` and
    /// `tasks.session_id` columns are updated with one shared timestamp, so the
    /// task pointer can never disagree with the round that owns the session.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]). No error message
    /// contains ids, session or message ids, SQL, JSON or paths.
    pub fn bind_round_session(
        &mut self,
        round: RoundRef,
        session_id: String,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        if session_id.is_empty() {
            return Err(RoundUpdateError::InvalidInput);
        }

        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        validate_current_round(&transaction, &round)?;

        transaction
            .execute(
                "UPDATE rounds SET session_id = ?1, updated_at = ?2 \
                 WHERE task_id = ?3 AND round_number = ?4",
                params![
                    &session_id,
                    now,
                    round.task_id.to_string(),
                    i64::from(round.round_number)
                ],
            )
            .map_err(RoundUpdateError::Database)?;
        transaction
            .execute(
                "UPDATE tasks SET session_id = ?1, updated_at = ?2 WHERE task_id = ?3",
                params![&session_id, now, round.task_id.to_string()],
            )
            .map_err(RoundUpdateError::Database)?;

        let outcome = read_round_update_outcome(&transaction, round.task_id, round.round_number)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(outcome)
    }

    /// Persists the allocated outbound message id of a pending, unattempted
    /// round without claiming a delivery attempt.
    ///
    /// The round must be the current round and `pending` with `attempted = 0`.
    /// A round that is not pending, is already prepared, or already attempted
    /// fails closed instead of overwriting the persisted id, so a stale or
    /// repeated prepare can never repoint a delivery.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]). No error message
    /// contains ids, message ids, SQL, JSON or paths.
    pub fn prepare_round(
        &mut self,
        round: RoundRef,
        outbound_message_id: String,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        if outbound_message_id.is_empty() {
            return Err(RoundUpdateError::InvalidInput);
        }

        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let (_task, row) = validate_current_round(&transaction, &round)?;
        if row.status != RoundStatus::Pending || row.attempted {
            return Err(RoundUpdateError::RoundNotPending);
        }
        if row.outbound_message_id.is_some() {
            return Err(RoundUpdateError::AlreadyPrepared);
        }

        transaction
            .execute(
                "UPDATE rounds SET outbound_message_id = ?1, updated_at = ?2 \
                 WHERE task_id = ?3 AND round_number = ?4",
                params![
                    outbound_message_id,
                    now,
                    round.task_id.to_string(),
                    i64::from(round.round_number)
                ],
            )
            .map_err(RoundUpdateError::Database)?;

        let outcome = read_round_update_outcome(&transaction, round.task_id, round.round_number)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(outcome)
    }

    /// Records that the prompt delivery attempt of a prepared round has started.
    ///
    /// Only a `pending` round with a persisted outbound message id and
    /// `attempted = 0` may move to `sent`; the transition is checked against
    /// [`ROUND_TRANSITIONS`]. `attempted` becomes `1` so a `sent` round is never
    /// resent even if a later status change moves it to a blocking status.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]). No error message
    /// contains ids, message ids, SQL, JSON or paths.
    pub fn mark_round_sent(
        &mut self,
        round: RoundRef,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let (_task, row) = validate_current_round(&transaction, &round)?;
        require_round_transition(row.status, RoundStatus::Sent)?;
        if row.outbound_message_id.is_none() {
            return Err(RoundUpdateError::RoundNotPrepared);
        }
        if row.attempted {
            return Err(RoundUpdateError::InvalidRoundTransition);
        }

        transaction
            .execute(
                "UPDATE rounds SET status = ?1, attempted = 1, updated_at = ?2 \
                 WHERE task_id = ?3 AND round_number = ?4",
                params![
                    RoundStatus::Sent.as_str(),
                    now,
                    round.task_id.to_string(),
                    i64::from(round.round_number)
                ],
            )
            .map_err(RoundUpdateError::Database)?;

        let outcome = read_round_update_outcome(&transaction, round.task_id, round.round_number)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(outcome)
    }

    /// Moves a `pending` or `sent` round to `observing`, preserving `attempted`.
    ///
    /// The transition is checked against [`ROUND_TRANSITIONS`]: only
    /// `pending -> observing` and `sent -> observing` are allowed. The
    /// `attempted` flag is never modified here, so a prepared-but-unsent round
    /// and a sent round stay distinguishable after the move.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]). No error message
    /// contains ids, message ids, SQL, JSON or paths.
    pub fn mark_round_observing(
        &mut self,
        round: RoundRef,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let (_task, row) = validate_current_round(&transaction, &round)?;
        require_round_transition(row.status, RoundStatus::Observing)?;

        transaction
            .execute(
                "UPDATE rounds SET status = ?1, updated_at = ?2 \
                 WHERE task_id = ?3 AND round_number = ?4",
                params![
                    RoundStatus::Observing.as_str(),
                    now,
                    round.task_id.to_string(),
                    i64::from(round.round_number)
                ],
            )
            .map_err(RoundUpdateError::Database)?;

        let outcome = read_round_update_outcome(&transaction, round.task_id, round.round_number)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(outcome)
    }

    /// Atomically persists the observation window of an existing open round.
    ///
    /// `deadline_seconds` must be finite and positive. One clock sample is taken
    /// and used for `worker_started_at`, `worker_deadline_at` and `updated_at`,
    /// so the deadline is strictly later than the start at millisecond
    /// precision. The round must be the current round and open.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]). No error message
    /// contains ids, timestamps, SQL, JSON or paths.
    pub fn mark_worker_started(
        &mut self,
        round: RoundRef,
        deadline_seconds: f64,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        if !deadline_seconds.is_finite() || deadline_seconds <= 0.0 {
            return Err(RoundUpdateError::InvalidDeadline);
        }
        let started = SystemTime::now();
        let deadline = started
            .checked_add(Duration::from_secs_f64(deadline_seconds))
            .ok_or(RoundUpdateError::InvalidDeadline)?;
        let started_at = format_rfc3339_millis(started);
        let deadline_at = format_rfc3339_millis(deadline);
        if deadline_at <= started_at {
            return Err(RoundUpdateError::InvalidDeadline);
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let (_task, row) = validate_current_round(&transaction, &round)?;
        if !row.status.is_open() {
            return Err(RoundUpdateError::RoundNotOpen);
        }
        // A worker starting in the claim's same millisecond must overwrite the
        // lease distinctly, so a failed-spawn rollback cannot cancel it.
        let (started_at, deadline_at) =
            if row.worker_started_at.as_deref() == Some(started_at.as_str()) {
                let next = started
                    .checked_add(Duration::from_millis(1))
                    .ok_or(RoundUpdateError::InvalidDeadline)?;
                let next_deadline = deadline
                    .checked_add(Duration::from_millis(1))
                    .ok_or(RoundUpdateError::InvalidDeadline)?;
                (
                    format_rfc3339_millis(next),
                    format_rfc3339_millis(next_deadline),
                )
            } else {
                (started_at, deadline_at)
            };

        transaction
            .execute(
                "UPDATE rounds SET worker_started_at = ?1, worker_deadline_at = ?2, \
                 updated_at = ?1 WHERE task_id = ?3 AND round_number = ?4",
                params![
                    started_at,
                    deadline_at,
                    round.task_id.to_string(),
                    i64::from(round.round_number)
                ],
            )
            .map_err(RoundUpdateError::Database)?;

        let outcome = read_round_update_outcome(&transaction, round.task_id, round.round_number)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(outcome)
    }

    /// Finishes a round and its task atomically, recording exactly one event.
    ///
    /// Inside one `BEGIN IMMEDIATE` transaction the current task and round are
    /// read, the round status change is validated against [`ROUND_TRANSITIONS`]
    /// and the *actually applied* task status change is validated through
    /// [`TaskStatus::require_transition`] (a no-op change of an already equal
    /// status is allowed). The allowed `response_message_id`, `response`,
    /// `error_code` and `result_json` columns are updated only when supplied;
    /// `result_json` must be a JSON object or `null`. The round status, task
    /// status and one event share a single timestamp, so round completion and
    /// the task update can never be observed separately.
    ///
    /// A pending cooperative close (task 3.9d) takes precedence over the
    /// caller-supplied `task_status`: the requested round fields/status are
    /// always written, but the task is moved to `closed` (validated as the
    /// effective transition) and exactly one `closed` event
    /// (`round_number = NULL`, message `task closed: <reason>`, using the exact
    /// fallback when the persisted reason is absent or empty) is written instead
    /// of the round-finished event. A caller-supplied `task_status` can therefore
    /// never bypass a pending close.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]). No error message
    /// contains ids, response, findings, session or message ids, SQL, JSON or
    /// paths.
    pub fn finish_round(
        &mut self,
        input: FinishRoundInput,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        self.finish_round_inner(input, false, None)
    }

    /// Atomically persists a complete validated checkpoint (or unavailable SQL
    /// NULL) with the ordinary round/task/event finish. Legacy finish calls leave
    /// the diagnostic column untouched. A checkpoint is not rollback state.
    pub fn finish_round_with_checkpoint(
        &mut self,
        input: FinishRoundInput,
        checkpoint: Option<&bridge_domain::RoundCheckpoint>,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        let json = checkpoint
            .map(|value| {
                value
                    .validate()
                    .map_err(|_| RoundUpdateError::InvalidInput)?;
                serde_json::to_string(value).map_err(|_| RoundUpdateError::InvalidJson)
            })
            .transpose()?;
        self.finish_round_inner(input, false, Some(json))
    }

    /// Read-only diagnostic parsing: absent/corrupt/incompatible checkpoints
    /// are unavailable. No older checkpoint is substituted for a missing round.
    pub fn get_round_checkpoint(
        &self,
        round: &RoundRef,
    ) -> Result<Option<bridge_domain::RoundCheckpoint>, RoundUpdateError> {
        let raw = self.connection.query_row(
            "SELECT checkpoint_json FROM rounds WHERE task_id=?1 AND project_id=?2 AND round_number=?3",
            params![round.task_id.to_string(),round.project_id.as_str(),round.round_number],
            |row| Ok(row.get::<_,Option<String>>(0).ok().flatten()),
        ).optional().map_err(RoundUpdateError::Database)?.flatten();
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
    }

    /// Fails a pending, unattempted revision on corrupt findings before send.
    /// This narrow invariant transition preserves the historical transition
    /// table and atomically checks current/project/task guards and close priority.
    pub fn fail_revision_findings(
        &mut self,
        round: RoundRef,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        self.finish_round_inner(
            FinishRoundInput {
                round,
                round_status: RoundStatus::Failed,
                task_status: TaskStatus::Failed,
                response_message_id: None,
                response: None,
                error_code: Some("structured_findings_invariant".to_owned()),
                result_json: None,
            },
            true,
            None,
        )
    }

    /// Worker-only pre-send outcomes. No fabricated outbound/attempted state
    /// and no generic pending -> terminal transition are introduced.
    pub fn finish_worker_pre_send(
        &mut self,
        input: FinishRoundInput,
        checkpoint: Option<&bridge_domain::RoundCheckpoint>,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        let allowed = matches!(
            (
                input.error_code.as_deref(),
                input.round_status,
                input.task_status
            ),
            (
                Some("workspace_mismatch" | "session_directory_mismatch" | "session_not_found"),
                RoundStatus::Failed,
                TaskStatus::Failed
            ) | (
                Some("session_unknown" | "session_ambiguous" | "transient_error"),
                RoundStatus::NeedsUser,
                TaskStatus::NeedsUser
            )
        );
        if !allowed || input.response.is_some() || input.response_message_id.is_some() {
            return Err(RoundUpdateError::InvalidPersistedState);
        }
        let checkpoint = checkpoint
            .map(|value| {
                value
                    .validate()
                    .map_err(|_| RoundUpdateError::InvalidInput)?;
                serde_json::to_string(value).map_err(|_| RoundUpdateError::InvalidJson)
            })
            .transpose()?;
        self.finish_round_inner(input, true, Some(checkpoint))
    }

    fn finish_round_inner(
        &mut self,
        input: FinishRoundInput,
        pre_send_invariant: bool,
        checkpoint: Option<Option<String>>,
    ) -> Result<RoundUpdateOutcome, RoundUpdateError> {
        let result_column = match &input.result_json {
            None => None,
            Some(value) if value.is_null() => Some(SqlValue::Null),
            Some(value) if value.is_object() => Some(SqlValue::Text(
                serde_json::to_string(value).map_err(|_| RoundUpdateError::InvalidJson)?,
            )),
            Some(_) => return Err(RoundUpdateError::InvalidJson),
        };

        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let (task, row) = validate_current_round(&transaction, &input.round)?;
        if pre_send_invariant {
            if !matches!(
                (row.kind, task.status),
                (RoundKind::Revise, TaskStatus::Revising)
                    | (RoundKind::Implement, TaskStatus::Implementing)
            ) || (input.error_code.as_deref() == Some("structured_findings_invariant")
                && row.kind != RoundKind::Revise)
                || row.status != RoundStatus::Pending
                || row.attempted
            {
                return Err(RoundUpdateError::InvalidPersistedState);
            }
        } else {
            require_round_transition(row.status, input.round_status)?;
        }
        let close_pending = task.close_requested_at.is_some();
        let effective_task_status = if close_pending {
            TaskStatus::Closed
        } else {
            input.task_status
        };
        if effective_task_status == TaskStatus::Closed {
            require_removed_worktree_for_close(&transaction, task.task_id)?;
        }
        if effective_task_status != task.status {
            task.status
                .require_transition(effective_task_status)
                .map_err(|_| RoundUpdateError::InvalidTaskTransition)?;
        }

        let mut sets: Vec<&str> = vec!["status = ?", "updated_at = ?"];
        let mut values: Vec<SqlValue> = vec![
            SqlValue::Text(input.round_status.as_str().to_owned()),
            SqlValue::Text(now.clone()),
        ];
        if let Some(value) = &input.response_message_id {
            sets.push("response_message_id = ?");
            values.push(SqlValue::Text(value.clone()));
        }
        if let Some(value) = &input.response {
            sets.push("response = ?");
            values.push(SqlValue::Text(value.clone()));
        }
        if let Some(value) = &input.error_code {
            sets.push("error_code = ?");
            values.push(SqlValue::Text(value.clone()));
        }
        if let Some(value) = result_column {
            sets.push("result_json = ?");
            values.push(value);
        }
        if let Some(value) = checkpoint {
            sets.push("checkpoint_json = ?");
            values.push(value.map(SqlValue::Text).unwrap_or(SqlValue::Null));
        }
        values.push(SqlValue::Text(input.round.task_id.to_string()));
        values.push(SqlValue::Integer(i64::from(input.round.round_number)));

        transaction
            .execute(
                &format!(
                    "UPDATE rounds SET {} WHERE task_id = ? AND round_number = ?",
                    sets.join(", ")
                ),
                params_from_iter(values),
            )
            .map_err(RoundUpdateError::Database)?;

        if close_pending {
            transaction
                .execute(
                    "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE task_id = ?3",
                    params![
                        TaskStatus::Closed.as_str(),
                        now,
                        input.round.task_id.to_string()
                    ],
                )
                .map_err(RoundUpdateError::Database)?;
            let message = close_event_message(task.close_reason.as_deref());
            transaction
                .execute(
                    "INSERT INTO events (task_id, round_number, kind, message, created_at) \
                     VALUES (?1, NULL, 'closed', ?2, ?3)",
                    params![input.round.task_id.to_string(), message, now],
                )
                .map_err(RoundUpdateError::Database)?;
        } else {
            transaction
                .execute(
                    "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE task_id = ?3",
                    params![
                        input.task_status.as_str(),
                        now,
                        input.round.task_id.to_string()
                    ],
                )
                .map_err(RoundUpdateError::Database)?;

            transaction
                .execute(
                    "INSERT INTO events (task_id, round_number, kind, message, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        input.round.task_id.to_string(),
                        i64::from(input.round.round_number),
                        input.round_status.as_str(),
                        format!("round finished: {}", input.round_status.as_str()),
                        now,
                    ],
                )
                .map_err(RoundUpdateError::Database)?;
        }

        writers::release_terminal(&transaction, input.round.task_id, effective_task_status)
            .map_err(RoundUpdateError::Database)?;
        let outcome =
            read_round_update_outcome(&transaction, input.round.task_id, input.round.round_number)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(outcome)
    }
}

// ---------------------------------------------------------------------------
// Verifier persist-once (task 3.9b).
//
// Both methods run in one `BEGIN IMMEDIATE` transaction and reuse the 3.9a
// round/project/current-round validation, which also enforces the
// `verifier_state`/`verifier_json` pair through [`RoundRow::from_row`]. No
// method ever overwrites a persisted `done` result.
// ---------------------------------------------------------------------------

impl StorageConnection {
    /// Atomically marks the current round's verifier as running.
    ///
    /// The round must exist, belong to `round.project_id`, be the current round
    /// of its task and carry a consistent `verifier_state`/`verifier_json` pair.
    /// A round without a verifier state is moved to [`VerifierState::Running`];
    /// an already-`running` round has its marker refreshed (the previous attempt
    /// may have died mid-flight); a completed ([`VerifierState::Done`]) round is
    /// left untouched and its persisted result is returned, so a stale `begin`
    /// can never undo a finished run.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]). No error message
    /// contains ids, project, verifier payload, SQL, JSON or paths.
    pub fn begin_verifier(
        &mut self,
        round: RoundRef,
    ) -> Result<VerifierUpdateOutcome, RoundUpdateError> {
        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let (task, row) = validate_current_round(&transaction, &round)?;
        let prior = row.verifier_state;
        if prior == Some(VerifierState::Done) {
            return Ok(VerifierUpdateOutcome::AlreadyDone(RoundUpdateOutcome {
                round: row,
                task,
            }));
        }

        transaction
            .execute(
                "UPDATE rounds SET verifier_state = ?1, updated_at = ?2 \
                 WHERE task_id = ?3 AND round_number = ?4",
                params![
                    VerifierState::Running.as_str(),
                    now,
                    round.task_id.to_string(),
                    i64::from(round.round_number)
                ],
            )
            .map_err(RoundUpdateError::Database)?;

        let persisted = read_round_update_outcome(&transaction, round.task_id, round.round_number)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        if prior == Some(VerifierState::Running) {
            Ok(VerifierUpdateOutcome::AlreadyRunning(persisted))
        } else {
            Ok(VerifierUpdateOutcome::Started(persisted))
        }
    }

    /// Atomically persists a completed verifier result exactly once.
    ///
    /// The round must exist, belong to `input.round.project_id`, be the current
    /// round of its task and carry a consistent `verifier_state`/`verifier_json`
    /// pair. A round without a completed result is moved to
    /// [`VerifierState::Done`] with the serialized [`Verification`]. A round
    /// whose persisted `done` result is identical to `input.verification` is
    /// replayed without any write, matching the reference implementation that
    /// reuses a finished run verbatim. A different persisted result is rejected
    /// fail closed and nothing is written, so a completed result can never be
    /// silently replaced.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]). No error message
    /// contains ids, project, verifier payload, SQL, JSON or paths.
    pub fn complete_verifier(
        &mut self,
        input: CompleteVerifierInput,
    ) -> Result<VerifierUpdateOutcome, RoundUpdateError> {
        let verifier_json = serde_json::to_string(&input.verification)
            .map_err(|_| RoundUpdateError::InvalidVerifier)?;

        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let (task, row) = validate_current_round(&transaction, &input.round)?;
        if row.verifier_state == Some(VerifierState::Done) {
            return match row.verifier_json.as_ref() {
                Some(existing) if existing == &input.verification => {
                    Ok(VerifierUpdateOutcome::Replayed(RoundUpdateOutcome {
                        round: row,
                        task,
                    }))
                }
                _ => Err(RoundUpdateError::VerifierResultConflict),
            };
        }

        transaction
            .execute(
                "UPDATE rounds SET verifier_state = ?1, verifier_json = ?2, updated_at = ?3 \
                 WHERE task_id = ?4 AND round_number = ?5",
                params![
                    VerifierState::Done.as_str(),
                    verifier_json,
                    now,
                    input.round.task_id.to_string(),
                    i64::from(input.round.round_number)
                ],
            )
            .map_err(RoundUpdateError::Database)?;

        let persisted =
            read_round_update_outcome(&transaction, input.round.task_id, input.round.round_number)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(VerifierUpdateOutcome::Completed(persisted))
    }
}

// ---------------------------------------------------------------------------
// Reopen failed round (task 3.9c).
//
// Recovery of an OpenCode assistant error runs in one `BEGIN IMMEDIATE`
// transaction: the task and its current round are read, eligibility is checked
// against the exact recoverable code and the round/task flip plus the single
// `reopened` event share one timestamp. Every non-eligible state is a typed
// no-op that writes nothing; corrupted rows fail closed.
// ---------------------------------------------------------------------------

impl StorageConnection {
    /// Atomically reopens the current failed `assistant_error` round.
    ///
    /// Only an OpenCode assistant error is recoverable: the user may resolve or
    /// continue the errored turn in the project TUI, after which the same bound
    /// session has a later result. Every infrastructure or invariant failure
    /// (`workspace_mismatch`, `session_not_found`, `session_directory_mismatch`,
    /// generic worker errors) stays terminal.
    ///
    /// Inside one `BEGIN IMMEDIATE` transaction the task is read and its current
    /// (highest-numbered) round is mapped through [`RoundRow::from_row`]. The
    /// round is reopened only when the task is `failed`, no cooperative close is
    /// pending, the current round is `failed` with
    /// [`RECOVERABLE_FAILED_ERROR_CODE`] and the round is consistent with its
    /// task. The round moves to `observing`, its `error_code` is cleared, the
    /// task is restored to its correct in-flight status (`implementing` for an
    /// implement round, `revising` for a revision round) and exactly one
    /// `reopened` event is written; all writes share one timestamp.
    ///
    /// A missing task, a task that is not an eligible recoverable failure and a
    /// repeated call after an already-applied transition are
    /// [`ReopenFailedRoundOutcome::NotEligible`] and write nothing, matching the
    /// reference implementation's `None`. A corrupted task/round row or an
    /// inconsistent task/round pair fails closed without any partial write.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]). No error message
    /// contains ids, project, response, findings, session or message ids, SQL,
    /// JSON or paths.
    pub fn reopen_failed_round(
        &mut self,
        task_id: TaskId,
    ) -> Result<ReopenFailedRoundOutcome, RoundUpdateError> {
        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let task = transaction
            .query_row(
                "SELECT * FROM tasks WHERE task_id = ?1",
                params![task_id.to_string()],
                |row| Ok(Task::from_row(row)),
            )
            .optional()
            .map_err(RoundUpdateError::Database)?;
        let Some(task) = task else {
            return Ok(ReopenFailedRoundOutcome::NotEligible);
        };
        let task = task.map_err(RoundUpdateError::TaskRow)?;
        if task.status != TaskStatus::Failed || task.close_requested_at.is_some() {
            return Ok(ReopenFailedRoundOutcome::NotEligible);
        }

        let max: Option<i64> = transaction
            .query_row(
                "SELECT MAX(round_number) FROM rounds WHERE task_id = ?1",
                params![task_id.to_string()],
                |row| row.get(0),
            )
            .map_err(RoundUpdateError::Database)?;
        let Some(max) = max else {
            return Ok(ReopenFailedRoundOutcome::NotEligible);
        };
        let round_number = u32::try_from(max)
            .ok()
            .filter(|value| *value >= 1)
            .ok_or(RoundUpdateError::InvalidPersistedState)?;

        let row = transaction
            .query_row(
                &format!(
                    "SELECT {ROUND_COLUMNS} FROM rounds WHERE task_id = ?1 AND round_number = ?2"
                ),
                params![task_id.to_string(), i64::from(round_number)],
                |row| Ok(RoundRow::from_row(row)),
            )
            .optional()
            .map_err(RoundUpdateError::Database)?;
        let Some(row) = row else {
            return Ok(ReopenFailedRoundOutcome::NotEligible);
        };
        let row = row.map_err(RoundUpdateError::RoundRow)?;
        if row.task_id != task.task_id || row.project_id != task.project_id {
            return Err(RoundUpdateError::InvalidPersistedState);
        }
        if row.status != RoundStatus::Failed
            || row.error_code.as_deref() != Some(RECOVERABLE_FAILED_ERROR_CODE)
        {
            return Ok(ReopenFailedRoundOutcome::NotEligible);
        }

        let task_status = match row.kind {
            RoundKind::Implement => TaskStatus::Implementing,
            RoundKind::Revise => TaskStatus::Revising,
            _ => return Err(RoundUpdateError::InvalidPersistedState),
        };
        // The `failed -> observing` recovery path is intentionally absent from
        // the shared [`ROUND_TRANSITIONS`] table so that the generic lifecycle
        // methods (notably [`StorageConnection::finish_round`]) can never
        // perform it. It is validated locally here, after the eligibility
        // guards above have established the exact recoverable state.
        if row.status != RoundStatus::Failed {
            return Err(RoundUpdateError::InvalidPersistedState);
        }
        task.status
            .require_transition(task_status)
            .map_err(|_| RoundUpdateError::InvalidTaskTransition)?;

        transaction
            .execute(
                "UPDATE rounds SET status = ?1, error_code = NULL, updated_at = ?2 \
                 WHERE task_id = ?3 AND round_number = ?4",
                params![
                    RoundStatus::Observing.as_str(),
                    now,
                    task_id.to_string(),
                    i64::from(round_number)
                ],
            )
            .map_err(RoundUpdateError::Database)?;
        transaction
            .execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE task_id = ?3",
                params![task_status.as_str(), now, task_id.to_string()],
            )
            .map_err(RoundUpdateError::Database)?;
        transaction
            .execute(
                "INSERT INTO events (task_id, round_number, kind, message, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    task_id.to_string(),
                    i64::from(round_number),
                    "reopened",
                    "failed assistant_error reopened for recovery",
                    now,
                ],
            )
            .map_err(RoundUpdateError::Database)?;

        let outcome = read_round_update_outcome(&transaction, task_id, round_number)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(ReopenFailedRoundOutcome::Reopened(Box::new(outcome)))
    }
}

// ---------------------------------------------------------------------------
// Cooperative close (task 3.9d).
//
// Both methods and the pending-close branch of `finish_round` run in one
// `BEGIN IMMEDIATE` transaction, read the task through the production
// [`Task::from_row`] contract, persist at most one event and share one
// timestamp. The reason is truncated to 300 Unicode scalar values exactly like
// the Python `reason[:300]` slice.
// ---------------------------------------------------------------------------

// Closing a worktree task is allowed only after the physical lifecycle finished.
// Runs in the same writer transaction as the terminal transition.
fn require_removed_worktree_for_close(
    connection: &Connection,
    task: TaskId,
) -> Result<(), RoundUpdateError> {
    let has_execution: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tasks') WHERE name='execution_mode')",
            [],
            |r| r.get(0),
        )
        .map_err(RoundUpdateError::Database)?;
    if !has_execution {
        // Historical v6 lifecycle has no execution mode or worktree registry.
        let has_registry: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='worktrees')",
            [], |r| r.get(0),
        ).map_err(RoundUpdateError::Database)?;
        return if has_registry {
            Err(RoundUpdateError::InvalidPersistedState)
        } else {
            Ok(())
        };
    }
    let execution: String = connection
        .query_row(
            "SELECT execution_mode FROM tasks WHERE task_id=?1",
            [task.to_string()],
            |r| r.get(0),
        )
        .map_err(RoundUpdateError::Database)?;
    match execution.as_str() {
        "direct" => Ok(()),
        "worktree" => {
            let removed: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM worktrees WHERE task_id=?1 AND status='removed')",
                    [task.to_string()],
                    |r| r.get(0),
                )
                .map_err(RoundUpdateError::Database)?;
            if removed {
                Ok(())
            } else {
                Err(RoundUpdateError::InvalidPersistedState)
            }
        }
        _ => Err(RoundUpdateError::InvalidPersistedState),
    }
}

impl StorageConnection {
    /// Atomically persists a cooperative close request for a running worker.
    ///
    /// Inside one `BEGIN IMMEDIATE` transaction the task is read through
    /// [`Task::from_row`]. The result mirrors Python `Storage.request_task_close`:
    ///
    /// * a missing task is [`RequestTaskCloseOutcome::UnknownTask`];
    /// * an already terminal task is
    ///   [`RequestTaskCloseOutcome::Terminal`] carrying its status;
    /// * a task without a pending request is
    ///   [`RequestTaskCloseOutcome::Requested`]: `close_requested_at`,
    ///   `close_reason` (truncated to 300 Unicode scalar values like Python
    ///   `reason[:300]`), `updated_at` and exactly one `close_requested` event
    ///   (`round_number = NULL`, message `task close requested`) are written with
    ///   one shared timestamp;
    /// * a task that already has a pending request is
    ///   [`RequestTaskCloseOutcome::AlreadyRequested`]: nothing is written, so
    ///   the original timestamp and reason are preserved and no event is
    ///   duplicated.
    ///
    /// The read and the write share one writer transaction, so concurrent
    /// identical requests yield exactly one [`RequestTaskCloseOutcome::Requested`]
    /// and one [`RequestTaskCloseOutcome::AlreadyRequested`], and exactly one
    /// event.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]) for an unexpected
    /// SQLite failure or a corrupted `tasks` row. No error message contains ids,
    /// reason, SQL, JSON or paths.
    pub fn request_task_close(
        &mut self,
        task_id: TaskId,
        reason: &str,
    ) -> Result<RequestTaskCloseOutcome, RoundUpdateError> {
        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let task = load_optional_task_for_update(&transaction, task_id)?;
        let Some(task) = task else {
            return Ok(RequestTaskCloseOutcome::UnknownTask);
        };
        if task.status.is_terminal() {
            return Ok(RequestTaskCloseOutcome::Terminal(task.status));
        }
        if task.close_requested_at.is_some() {
            return Ok(RequestTaskCloseOutcome::AlreadyRequested(task));
        }

        let stored_reason = truncate_close_reason(reason);
        transaction
            .execute(
                "UPDATE tasks SET close_requested_at = ?1, close_reason = ?2, updated_at = ?1 \
                 WHERE task_id = ?3",
                params![now, stored_reason, task_id.to_string()],
            )
            .map_err(RoundUpdateError::Database)?;
        transaction
            .execute(
                "INSERT INTO events (task_id, round_number, kind, message, created_at) \
                 VALUES (?1, NULL, 'close_requested', 'task close requested', ?2)",
                params![task_id.to_string(), now],
            )
            .map_err(RoundUpdateError::Database)?;

        let persisted = load_task_for_update(&transaction, task_id)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(RequestTaskCloseOutcome::Requested(persisted))
    }

    /// Atomically closes a task once its worker observes a pending close request.
    ///
    /// Inside one `BEGIN IMMEDIATE` transaction the task is read through
    /// [`Task::from_row`]. The result mirrors Python
    /// `Storage.complete_requested_close`:
    ///
    /// * a missing task is [`CompleteRequestedCloseOutcome::UnknownTask`];
    /// * an already terminal task is [`CompleteRequestedCloseOutcome::Terminal`]
    ///   carrying its status;
    /// * a non-terminal task without a pending request is
    ///   [`CompleteRequestedCloseOutcome::NoCloseRequest`] and nothing is
    ///   written;
    /// * a non-terminal task with a pending request is
    ///   [`CompleteRequestedCloseOutcome::Closed`]: the task moves to `closed`
    ///   and exactly one `closed` event (`round_number = NULL`) is written with
    ///   the message `task closed: <reason>` where `<reason>` is the persisted
    ///   `close_reason` (truncated to 300 Unicode scalar values) or the exact
    ///   fallback `requested while worker was running` when it is absent or
    ///   empty. The actually applied transition is validated through
    ///   [`TaskStatus::require_transition`].
    ///
    /// A repeat after the task is `closed` returns
    /// [`CompleteRequestedCloseOutcome::Terminal`] and writes nothing.
    /// Modern worktree tasks require a `removed` registry row before closing;
    /// the lifecycle service completes cleanup first. Historical v6 is direct.
    ///
    /// # Errors
    ///
    /// Returns a typed category (see [`RoundUpdateError`]) for an unexpected
    /// SQLite failure or a corrupted `tasks` row. No error message contains ids,
    /// reason, response, SQL, JSON or paths.
    pub fn complete_requested_close(
        &mut self,
        task_id: TaskId,
    ) -> Result<CompleteRequestedCloseOutcome, RoundUpdateError> {
        let now = utc_now_rfc3339_millis();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;

        let task = load_optional_task_for_update(&transaction, task_id)?;
        let Some(task) = task else {
            return Ok(CompleteRequestedCloseOutcome::UnknownTask);
        };
        if task.status.is_terminal() {
            return Ok(CompleteRequestedCloseOutcome::Terminal(task.status));
        }
        if task.close_requested_at.is_none() {
            return Ok(CompleteRequestedCloseOutcome::NoCloseRequest);
        }
        require_removed_worktree_for_close(&transaction, task_id)?;
        task.status
            .require_transition(TaskStatus::Closed)
            .map_err(|_| RoundUpdateError::InvalidTaskTransition)?;

        transaction
            .execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE task_id = ?3",
                params![TaskStatus::Closed.as_str(), now, task_id.to_string()],
            )
            .map_err(RoundUpdateError::Database)?;
        let message = close_event_message(task.close_reason.as_deref());
        transaction
            .execute(
                "INSERT INTO events (task_id, round_number, kind, message, created_at) \
                 VALUES (?1, NULL, 'closed', ?2, ?3)",
                params![task_id.to_string(), message, now],
            )
            .map_err(RoundUpdateError::Database)?;

        writers::release_terminal(&transaction, task_id, TaskStatus::Closed)
            .map_err(RoundUpdateError::Database)?;
        let persisted = load_task_for_update(&transaction, task_id)?;
        transaction.commit().map_err(RoundUpdateError::Database)?;
        Ok(CompleteRequestedCloseOutcome::Closed(Box::new(persisted)))
    }
}

/// A typed reference to one existing round.
///
/// The project id is part of the reference so every lifecycle method can reject
/// a task/round that belongs to another project without a second caller lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundRef {
    /// Owning task.
    pub task_id: TaskId,
    /// Owning project.
    pub project_id: ProjectId,
    /// One-based round number within the task.
    pub round_number: u32,
}

/// Input for [`StorageConnection::create_revision_round`].
///
/// The round kind is intentionally not a field: revision rounds are always
/// [`RoundKind::Revise`]. `round_number` must be exactly the next sequential
/// number for the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRevisionRoundInput {
    /// Task that owns the new round.
    pub task_id: TaskId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Proposed one-based round number; must be `current_max + 1`.
    pub round_number: u32,
    /// Idempotency key for the new round.
    pub request_id: String,
    /// Hash of the revision request payload.
    pub payload_hash: String,
    /// Revision findings text, preserved verbatim when present.
    pub findings: Option<String>,
}

/// Result of creating or replaying a modern revision request.
/// A replay must not cause the caller to spawn another worker.
#[derive(Clone, PartialEq)]
pub struct RevisionRoundOutcome {
    /// Persisted task and original revision round.
    pub state: RoundUpdateOutcome,
    /// Whether the identical request already existed (no writes performed).
    pub replayed: bool,
}
impl fmt::Debug for RevisionRoundOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RevisionRoundOutcome")
            .field("replayed", &self.replayed)
            .finish_non_exhaustive()
    }
}

/// Input for [`StorageConnection::finish_round`].
///
/// `result_json` must be `None`, a JSON object or JSON `null`; any other shape
/// is rejected before the transaction starts. Supplied optional columns are
/// written, omitted ones are left untouched.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishRoundInput {
    /// Round to finish.
    pub round: RoundRef,
    /// Target round status; must be reachable through [`ROUND_TRANSITIONS`].
    pub round_status: RoundStatus,
    /// Target task status; must be reachable through
    /// [`TaskStatus::require_transition`] unless it equals the current status.
    pub task_status: TaskStatus,
    /// Response OpenCode message id, written only when supplied.
    pub response_message_id: Option<String>,
    /// Assistant response text, written only when supplied.
    pub response: Option<String>,
    /// Machine-readable failure code, written only when supplied.
    pub error_code: Option<String>,
    /// Change-collection result, written only when supplied.
    pub result_json: Option<serde_json::Value>,
}

/// The persisted result of one atomic round/task lifecycle transition.
///
/// Both rows are read back inside the same transaction through the production
/// [`RoundRow::from_row`]/[`Task::from_row`] mapping, so the value is the exact
/// committed state and is sufficient for later worker integration.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundUpdateOutcome {
    /// The persisted round after the transition.
    pub round: RoundRow,
    /// The persisted task after the transition.
    pub task: Task,
}

/// The only recoverable round failure code.
///
/// This is the exact Python `RECOVERABLE_FAILED_ERROR_CODE` spelling. A failed
/// round carrying any other `error_code` (for example `workspace_mismatch`,
/// `session_not_found` or `session_directory_mismatch`) stays terminal and is
/// never reopened by [`StorageConnection::reopen_failed_round`].
pub const RECOVERABLE_FAILED_ERROR_CODE: &str = "assistant_error";

/// The single, table-driven source of truth for allowed [`RoundStatus`]
/// transitions used by this crate.
///
/// Each entry is an allowed `(from, to)` pair transcribed from
/// `domain.round_transitions`. The `failed -> observing` recovery path is
/// deliberately absent: it is validated locally inside
/// [`StorageConnection::reopen_failed_round`] after its eligibility guards so
/// that the generic lifecycle methods (notably
/// [`StorageConnection::finish_round`]) can never perform it. Cooperative close
/// is also deliberately absent.
pub const ROUND_TRANSITIONS: [(RoundStatus, RoundStatus); 15] = [
    (RoundStatus::Pending, RoundStatus::Sent),
    (RoundStatus::Pending, RoundStatus::Observing),
    (RoundStatus::Sent, RoundStatus::Observing),
    (RoundStatus::Observing, RoundStatus::Complete),
    (RoundStatus::Observing, RoundStatus::Failed),
    (RoundStatus::Observing, RoundStatus::NeedsUser),
    (RoundStatus::Observing, RoundStatus::DeliveryUnknown),
    (RoundStatus::NeedsUser, RoundStatus::Complete),
    (RoundStatus::NeedsUser, RoundStatus::Failed),
    (RoundStatus::NeedsUser, RoundStatus::NeedsUser),
    (RoundStatus::NeedsUser, RoundStatus::DeliveryUnknown),
    (RoundStatus::DeliveryUnknown, RoundStatus::Complete),
    (RoundStatus::DeliveryUnknown, RoundStatus::Failed),
    (RoundStatus::DeliveryUnknown, RoundStatus::NeedsUser),
    (RoundStatus::DeliveryUnknown, RoundStatus::DeliveryUnknown),
];

/// Whether moving from `from` to `to` is an allowed round transition.
///
/// The answer is derived solely from [`ROUND_TRANSITIONS`].
#[must_use]
pub fn round_transition_allowed(from: RoundStatus, to: RoundStatus) -> bool {
    ROUND_TRANSITIONS
        .iter()
        .any(|(allowed_from, allowed_to)| *allowed_from == from && *allowed_to == to)
}

/// A typed, safe error raised while applying an atomic round/task transition.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message that never contains ids, project, request id, payload hash, findings,
/// response, session or message ids, SQL, JSON or paths. The underlying
/// row-mapping or SQLite error, when present, is reachable only through
/// [`Error::source`].
#[non_exhaustive]
pub enum RoundUpdateError {
    /// The referenced task does not exist.
    MissingTask,
    /// The referenced round does not exist.
    MissingRound,
    /// The task or round belongs to a different project.
    ProjectMismatch,
    /// The round is not the current (highest-numbered) round of the task.
    NotCurrentRound,
    /// The requested round number is not the next sequential number.
    NonSequentialRound,
    /// The request id is already used in this project.
    RequestConflict,
    /// The round status transition is not allowed by [`ROUND_TRANSITIONS`].
    InvalidRoundTransition,
    /// The task status transition is not allowed by
    /// [`TaskStatus::require_transition`].
    InvalidTaskTransition,
    /// The round is not `pending` (or is already attempted).
    RoundNotPending,
    /// The round has no prepared outbound message id.
    RoundNotPrepared,
    /// The round already has an outbound message id.
    AlreadyPrepared,
    /// The round is not open.
    RoundNotOpen,
    /// The input is invalid (for example empty required text or a zero round).
    InvalidInput,
    /// A supplied `result_json` is neither a JSON object nor JSON `null`.
    InvalidJson,
    /// The worker deadline input is not finite, positive or strictly later.
    InvalidDeadline,
    /// A different verifier result is already persisted for this round.
    VerifierResultConflict,
    /// The verifier result cannot be represented as persisted JSON.
    InvalidVerifier,
    /// The persisted state is internally inconsistent.
    InvalidPersistedState,
    /// A `tasks` row could not be mapped.
    TaskRow(TaskRowError),
    /// A `rounds` row could not be mapped.
    RoundRow(RoundRowError),
    /// An unexpected SQLite failure.
    Database(rusqlite::Error),
}

impl fmt::Display for RoundUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTask => f.write_str("task does not exist"),
            Self::MissingRound => f.write_str("round does not exist"),
            Self::ProjectMismatch => f.write_str("task or round belongs to a different project"),
            Self::NotCurrentRound => f.write_str("round is not the current round"),
            Self::NonSequentialRound => {
                f.write_str("round number is not the next sequential round")
            }
            Self::RequestConflict => f.write_str("request id is already used in this project"),
            Self::InvalidRoundTransition => f.write_str("round status transition is not allowed"),
            Self::InvalidTaskTransition => f.write_str("task status transition is not allowed"),
            Self::RoundNotPending => f.write_str("round is not pending"),
            Self::RoundNotPrepared => f.write_str("round has no prepared outbound message"),
            Self::AlreadyPrepared => f.write_str("round already has an outbound message"),
            Self::RoundNotOpen => f.write_str("round is not open"),
            Self::InvalidInput => f.write_str("round update input is invalid"),
            Self::InvalidJson => f.write_str("round result json is invalid"),
            Self::InvalidDeadline => f.write_str("worker deadline is invalid"),
            Self::VerifierResultConflict => {
                f.write_str("a verifier result is already persisted for this round")
            }
            Self::InvalidVerifier => f.write_str("verifier result is invalid"),
            Self::InvalidPersistedState => f.write_str("persisted round state is invalid"),
            Self::TaskRow(_) => f.write_str("task row could not be mapped"),
            Self::RoundRow(_) => f.write_str("round row could not be mapped"),
            Self::Database(_) => f.write_str("storage database error"),
        }
    }
}

impl fmt::Debug for RoundUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Error for RoundUpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TaskRow(error) => Some(error),
            Self::RoundRow(error) => Some(error),
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

/// Input for [`StorageConnection::complete_verifier`] (task 3.9b).
///
/// The round is the exact current round of its task; the verification is the
/// compact outcome the reference implementation persists as `verifier_json`.
#[derive(Debug, Clone, PartialEq)]
pub struct CompleteVerifierInput {
    /// Round whose verifier result is being persisted.
    pub round: RoundRef,
    /// Completed verifier outcome to persist once.
    pub verification: Verification,
}

/// The persisted result of one atomic verifier lifecycle transition (task 3.9b).
///
/// Every variant wraps the persisted [`RoundUpdateOutcome`] read back through
/// the production [`RoundRow::from_row`]/[`Task::from_row`] mapping, so the
/// value is the exact committed state. [`VerifierUpdateOutcome::Replayed`] and
/// [`VerifierUpdateOutcome::AlreadyDone`] carry the original persisted result
/// and were produced without mutating any row.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifierUpdateOutcome {
    /// `begin_verifier`: the round had no verifier state and is now `running`.
    Started(RoundUpdateOutcome),
    /// `begin_verifier`: the round was already `running`; the marker was
    /// refreshed.
    AlreadyRunning(RoundUpdateOutcome),
    /// `begin_verifier`: the round was already `done`; the result was left
    /// untouched.
    AlreadyDone(RoundUpdateOutcome),
    /// `complete_verifier`: the result was persisted for the first time.
    Completed(RoundUpdateOutcome),
    /// `complete_verifier`: an identical result was already persisted; no write.
    Replayed(RoundUpdateOutcome),
}

impl VerifierUpdateOutcome {
    /// Returns the persisted round/task pair carried by this outcome.
    #[must_use]
    pub fn outcome(&self) -> &RoundUpdateOutcome {
        match self {
            Self::Started(outcome)
            | Self::AlreadyRunning(outcome)
            | Self::AlreadyDone(outcome)
            | Self::Completed(outcome)
            | Self::Replayed(outcome) => outcome,
        }
    }
}

/// The outcome of [`StorageConnection::reopen_failed_round`] (task 3.9c).
///
/// [`ReopenFailedRoundOutcome::Reopened`] carries the persisted round/task pair
/// read back through the production [`RoundRow::from_row`]/[`Task::from_row`]
/// mapping after the atomic transition. [`ReopenFailedRoundOutcome::NotEligible`]
/// is the typed no-op for a missing task, a task that is not a recoverable
/// failed `assistant_error` or a repeated call; it never writes.
#[derive(Debug, Clone, PartialEq)]
pub enum ReopenFailedRoundOutcome {
    /// The current failed `assistant_error` round was reopened atomically.
    Reopened(Box<RoundUpdateOutcome>),
    /// The task is not an eligible recoverable failure; nothing changed.
    NotEligible,
}

impl ReopenFailedRoundOutcome {
    /// Returns the persisted round/task pair when the round was reopened, or
    /// `None` for the no-op outcome.
    #[must_use]
    pub fn reopened(&self) -> Option<&RoundUpdateOutcome> {
        match self {
            Self::Reopened(outcome) => Some(outcome.as_ref()),
            Self::NotEligible => None,
        }
    }
}

/// The outcome of [`StorageConnection::request_task_close`] (task 3.9d).
///
/// [`Requested`](Self::Requested) is the first persisted request and carries the
/// persisted [`Task`]; [`AlreadyRequested`](Self::AlreadyRequested) is the
/// idempotent repeat that wrote nothing. Both correspond to the Python
/// `"close_requested"` result. [`UnknownTask`](Self::UnknownTask) and
/// [`Terminal`](Self::Terminal) mirror the Python `"unknown_task"` and terminal
/// status results.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RequestTaskCloseOutcome {
    /// The close request was newly persisted with exactly one `close_requested`
    /// event.
    Requested(Task),
    /// A close request was already persisted; nothing changed.
    AlreadyRequested(Task),
    /// The task does not exist.
    UnknownTask,
    /// The task is already terminal; carries its terminal status.
    Terminal(TaskStatus),
}

impl RequestTaskCloseOutcome {
    /// Returns the persisted task when the outcome is
    /// [`RequestTaskCloseOutcome::Requested`] or
    /// [`RequestTaskCloseOutcome::AlreadyRequested`].
    #[must_use]
    pub fn task(&self) -> Option<&Task> {
        match self {
            Self::Requested(task) | Self::AlreadyRequested(task) => Some(task),
            Self::UnknownTask | Self::Terminal(_) => None,
        }
    }

    /// Whether a close request is pending after this call (first or repeat).
    #[must_use]
    pub fn is_close_requested(&self) -> bool {
        matches!(self, Self::Requested(_) | Self::AlreadyRequested(_))
    }
}

/// The outcome of [`StorageConnection::complete_requested_close`] (task 3.9d).
///
/// [`Closed`](Self::Closed) is the first application of a pending request and
/// carries the persisted [`Task`]. Every other variant is a typed no-op that
/// writes nothing, matching the Python `False` result.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CompleteRequestedCloseOutcome {
    /// The pending close request was applied; the task is now `closed`.
    Closed(Box<Task>),
    /// The task has no pending close request; nothing changed.
    NoCloseRequest,
    /// The task is already terminal; nothing changed.
    Terminal(TaskStatus),
    /// The task does not exist.
    UnknownTask,
}

impl CompleteRequestedCloseOutcome {
    /// Returns the persisted task when the task was closed.
    #[must_use]
    pub fn task(&self) -> Option<&Task> {
        match self {
            Self::Closed(task) => Some(task.as_ref()),
            Self::NoCloseRequest | Self::Terminal(_) | Self::UnknownTask => None,
        }
    }
}

/// The maximum number of Unicode scalar values persisted for a close reason,
/// matching the Python `reason[:300]` slice (which counts code points).
pub const CLOSE_REASON_MAX_CHARS: usize = 300;

/// The exact fallback close reason used when no non-empty reason is persisted,
/// matching the Python `"requested while worker was running"` literal.
pub const CLOSE_REASON_FALLBACK: &str = "requested while worker was running";

/// Loads one `tasks` row inside a transaction, mapping it through
/// [`Task::from_row`], or `None` when the task is absent.
fn map_task_runtime(connection: &Connection, row: &Row<'_>) -> Result<Task, TaskRowError> {
    match Task::from_row(row) {
        Ok(task) => Ok(task),
        Err(error) => {
            let Ok(task) = Task::from_row_for_status(row) else {
                return Err(error);
            };
            let authorized=connection.query_row("SELECT EXISTS(SELECT 1 FROM events e JOIN rounds r ON r.task_id=e.task_id AND r.round_number=e.round_number WHERE e.task_id=?1 AND e.kind='budget_override' AND e.message='explicit one-round budget override' AND r.kind='revise' AND r.round_number=(SELECT MAX(round_number) FROM rounds WHERE task_id=?1))",[task.task_id.to_string()],|r|r.get::<_,bool>(0)).unwrap_or(false);
            if authorized { Ok(task) } else { Err(error) }
        }
    }
}

fn load_optional_task_for_update(
    connection: &Connection,
    task_id: TaskId,
) -> Result<Option<Task>, RoundUpdateError> {
    let row = connection
        .query_row(
            "SELECT * FROM tasks WHERE task_id = ?1",
            params![task_id.to_string()],
            |row| Ok(map_task_runtime(connection, row)),
        )
        .optional()
        .map_err(RoundUpdateError::Database)?;
    match row {
        None => Ok(None),
        Some(Ok(task)) => Ok(Some(task)),
        Some(Err(error)) => Err(RoundUpdateError::TaskRow(error)),
    }
}

/// Loads one `tasks` row inside a transaction, mapping it through
/// [`Task::from_row`].
fn load_task_for_update(
    connection: &Connection,
    task_id: TaskId,
) -> Result<Task, RoundUpdateError> {
    connection
        .query_row(
            "SELECT * FROM tasks WHERE task_id = ?1",
            params![task_id.to_string()],
            |row| Ok(map_task_runtime(connection, row)),
        )
        .optional()
        .map_err(RoundUpdateError::Database)?
        .ok_or(RoundUpdateError::MissingTask)?
        .map_err(RoundUpdateError::TaskRow)
}

/// Truncates a close reason to [`CLOSE_REASON_MAX_CHARS`] Unicode scalar values,
/// matching the Python `reason[:300]` slice that counts code points rather than
/// bytes.
fn truncate_close_reason(reason: &str) -> String {
    reason.chars().take(CLOSE_REASON_MAX_CHARS).collect()
}

/// Builds the `closed` event message `task closed: <reason>` from an optional
/// persisted close reason, using [`CLOSE_REASON_FALLBACK`] for an absent or
/// empty reason and truncating the reason to [`CLOSE_REASON_MAX_CHARS`] Unicode
/// scalar values, matching Python.
fn close_event_message(reason: Option<&str>) -> String {
    let reason = match reason {
        Some(reason) if !reason.is_empty() => truncate_close_reason(reason),
        _ => CLOSE_REASON_FALLBACK.to_owned(),
    };
    format!("task closed: {reason}")
}

/// Loads one `rounds` row inside a transaction, mapping it through
/// [`RoundRow::from_row`].
fn load_round_for_update(
    connection: &Connection,
    task_id: TaskId,
    round_number: u32,
) -> Result<RoundRow, RoundUpdateError> {
    connection
        .query_row(
            &format!("SELECT {ROUND_COLUMNS} FROM rounds WHERE task_id = ?1 AND round_number = ?2"),
            params![task_id.to_string(), i64::from(round_number)],
            |row| Ok(RoundRow::from_row(row)),
        )
        .optional()
        .map_err(RoundUpdateError::Database)?
        .ok_or(RoundUpdateError::MissingRound)?
        .map_err(RoundUpdateError::RoundRow)
}

/// Returns the highest round number of `task_id`, or [`RoundUpdateError::MissingRound`]
/// when the task has no rounds.
fn current_round_number(connection: &Connection, task_id: TaskId) -> Result<u32, RoundUpdateError> {
    let max: Option<i64> = connection
        .query_row(
            "SELECT MAX(round_number) FROM rounds WHERE task_id = ?1",
            params![task_id.to_string()],
            |row| row.get(0),
        )
        .map_err(RoundUpdateError::Database)?;
    let max = max.ok_or(RoundUpdateError::MissingRound)?;
    u32::try_from(max)
        .ok()
        .filter(|value| *value >= 1)
        .ok_or(RoundUpdateError::InvalidPersistedState)
}

/// Validates that `round` exists, belongs to its declared task and project and
/// is the current round of the task, returning the mapped task and round.
fn validate_current_round(
    connection: &Connection,
    round: &RoundRef,
) -> Result<(Task, RoundRow), RoundUpdateError> {
    let task = load_task_for_update(connection, round.task_id)?;
    if task.project_id != round.project_id {
        return Err(RoundUpdateError::ProjectMismatch);
    }
    let row = load_round_for_update(connection, round.task_id, round.round_number)?;
    if row.project_id != round.project_id || row.task_id != round.task_id {
        return Err(RoundUpdateError::ProjectMismatch);
    }
    if current_round_number(connection, round.task_id)? != round.round_number {
        return Err(RoundUpdateError::NotCurrentRound);
    }
    Ok((task, row))
}

/// Reads the persisted round and task of a completed transition.
fn read_round_update_outcome(
    connection: &Connection,
    task_id: TaskId,
    round_number: u32,
) -> Result<RoundUpdateOutcome, RoundUpdateError> {
    let round = load_round_for_update(connection, task_id, round_number)?;
    let task = load_task_for_update(connection, task_id)?;
    Ok(RoundUpdateOutcome { round, task })
}

/// Requires that `from -> to` is an allowed round transition.
fn require_round_transition(from: RoundStatus, to: RoundStatus) -> Result<(), RoundUpdateError> {
    if round_transition_allowed(from, to) {
        Ok(())
    } else {
        Err(RoundUpdateError::InvalidRoundTransition)
    }
}

/// Classifies a failed revision `rounds` insert as the project-scoped request
/// conflict or a non-sequential round, falling back to
/// [`RoundUpdateError::Database`].
fn classify_revision_round_insert_error(error: rusqlite::Error) -> RoundUpdateError {
    if let rusqlite::Error::SqliteFailure(inner, message) = &error {
        if let Some(message) = message {
            if message.contains("rounds.request_id") || message.contains("rounds.project_id") {
                return RoundUpdateError::RequestConflict;
            }
            if message.contains("rounds.task_id") {
                return RoundUpdateError::NonSequentialRound;
            }
        }
        if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY {
            return RoundUpdateError::NonSequentialRound;
        }
        if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE {
            return RoundUpdateError::RequestConflict;
        }
    }
    RoundUpdateError::Database(error)
}
