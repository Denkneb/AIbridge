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
//! creating or migrating any schema. Row mapping, initialization/migrations and
//! all write/query APIs remain out of scope (tasks 3.3+).

use std::error::Error;
use std::fmt;
use std::path::Path;

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};

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
    let user_version = read_user_version(&connection)?;
    let tables = read_tables(&connection)?;

    if user_version != SCHEMA_VERSION {
        return Err(InspectError::UnsupportedUserVersion {
            found: user_version,
        });
    }

    let meta_schema_version = read_meta_schema_version(&connection, &tables)?;
    validate_version_pair(user_version, &meta_schema_version)?;

    let indexes = read_indexes(&connection)?;
    let foreign_keys = read_foreign_keys(&connection, &tables)?;

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
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(classify_error)
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

#[cfg(test)]
mod tests {
    use super::{
        BUSY_TIMEOUT_MS, Column, ConnectError, Contract, ForeignKey, Index, InspectError,
        SCHEMA_VERSION, SchemaMismatch, Table, connect, inspect, v6_contract,
    };
    use rusqlite::Connection;
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

    /// The schema v6 DDL, byte-for-byte equivalent to
    /// `docs/fixtures/sqlite/generate.py` (minus `IF NOT EXISTS`, since the
    /// synthetic databases are always created fresh).
    const V6_SCHEMA: &str = r#"
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE tasks (
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
CREATE UNIQUE INDEX ux_tasks_active
    ON tasks(project_id) WHERE status IN
    ('implementing','awaiting_review','revising','needs_user','failed','delivery_unknown');
CREATE TABLE rounds (
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
CREATE UNIQUE INDEX ux_rounds_request ON rounds(project_id, request_id);
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL,
    round_number INTEGER,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX ix_events_task ON events(task_id, id);
"#;

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
