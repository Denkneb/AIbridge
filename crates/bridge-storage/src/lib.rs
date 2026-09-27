//! Read-only inspection of an existing `agent-bridge` SQLite state database.
//!
//! This crate opens an existing database strictly read-only (SQLite URI
//! `mode=ro&immutable=1`) and checks that it matches the frozen schema v6
//! contract described by `docs/fixtures/sqlite/expected.json`:
//!
//! * `PRAGMA user_version = 6` **and** `meta.schema_version = "6"` as a
//!   consistent pair;
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
//! provided by [`initialize`]. Schema migrations and all list/get/pagination or
//! write APIs remain out of scope (tasks 3.6+).

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use bridge_domain::{
    ProjectId, RoundKind, RoundStatus, TaskId, TaskStatus, Verification, VerifierState,
};
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Row, TransactionBehavior, params,
};

/// The only supported `PRAGMA user_version` / `meta.schema_version`.
pub const SCHEMA_VERSION: i64 = 6;

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
/// that both version markers and the whole schema v6 contract matched.
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

/// A structural schema mismatch against the frozen v6 contract.
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
    /// `PRAGMA user_version` is not [`SCHEMA_VERSION`].
    UnsupportedUserVersion { found: i64 },
    /// The `meta` table or the `schema_version` key is missing.
    MissingSchemaVersion,
    /// `meta.schema_version` is not an integer.
    MalformedSchemaVersion,
    /// `meta.schema_version` does not agree with `PRAGMA user_version`.
    MismatchedSchemaVersion { user_version: i64, meta: String },
    /// The database does not match the frozen schema v6 contract.
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
                write!(
                    f,
                    "database schema is not compatible with schema v6: {mismatch}"
                )
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

/// Opens `path` strictly read-only and validates it against schema v6.
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

/// Validates an open connection against the frozen schema v6 contract.
///
/// This is the shared core of [`inspect`] and [`initialize`]: it reads the
/// version markers, the user tables, the named indexes and the foreign keys and
/// checks all of them against the contract. It performs no writes.
fn validate_database(connection: &Connection) -> Result<Inspection, InspectError> {
    let user_version = read_user_version(connection)?;
    let tables = read_tables(connection)?;

    if user_version != SCHEMA_VERSION {
        return Err(InspectError::UnsupportedUserVersion {
            found: user_version,
        });
    }

    let meta_schema_version = read_meta_schema_version(connection, &tables)?;
    validate_version_pair(user_version, &meta_schema_version)?;

    let indexes = read_indexes(connection)?;
    let foreign_keys = read_foreign_keys(connection, &tables)?;

    validate_schema(&tables, &indexes, &foreign_keys).map_err(InspectError::IncompatibleSchema)?;

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
    format!("file:{encoded}?mode=ro&immutable=1")
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

fn configure(connection: &Connection) -> Result<(), ConnectError> {
    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(classify_configure_error)?;
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

/// A fully typed view of one schema v6 `tasks` row.
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
}

impl Task {
    /// Maps one `tasks` row into a [`Task`].
    ///
    /// The row must expose the fifteen schema v6 `tasks` columns by name; a
    /// missing column is reported as [`TaskRowError::MissingColumn`].
    ///
    /// # Errors
    ///
    /// Returns a typed category for every kind of corrupted persisted data
    /// (see [`TaskRowError`]). No error message contains row data, task text,
    /// workspace, paths, identifiers or JSON payloads.
    pub fn from_row(row: &Row<'_>) -> Result<Self, TaskRowError> {
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
    use super::{
        BUSY_TIMEOUT_MS, Column, ConnectError, Contract, ForeignKey, Index, InitializeError,
        InspectError, RoundRow, RoundRowError, SCHEMA_VERSION, SchemaMismatch, Table, Task,
        TaskRowError, V6_SCHEMA_DDL, apply_schema_v6, connect, initialize, inspect, open_read_only,
        query_user_version, v6_contract,
    };
    use bridge_domain::{RoundKind, RoundStatus, TaskStatus, VerifierState};
    use rusqlite::types::Value as SqlValue;
    use rusqlite::{Connection, TransactionBehavior};
    use serde_json::Value;
    use std::path::{Path, PathBuf};
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

    const TASK_ROW_COLUMNS: &str = "task_id, project_id, workspace, status, session_id, task, \
         allowed_paths, test_commands, created_at, updated_at, base_head, snapshot, \
         revision_count, close_requested_at, close_reason";

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
            .query_row(
                &format!("SELECT {TASK_ROW_COLUMNS} FROM tasks"),
                [],
                |row| Ok(Task::from_row(row)),
            )
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
}
