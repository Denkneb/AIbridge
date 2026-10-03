//! Additive v6 → v15 DDL for guarded Rust state only.

use super::{Contract, ForeignKey, Index, InspectError, SchemaMismatch, Table, column, index};
use rusqlite::{Connection, params};

const ADDITIVE_DDL: &str = r#"
ALTER TABLE rounds ADD COLUMN structured_findings TEXT;
ALTER TABLE tasks ADD COLUMN budget_json TEXT;
ALTER TABLE tasks ADD COLUMN workflow_id TEXT;
ALTER TABLE tasks ADD COLUMN depends_on TEXT;
ALTER TABLE tasks ADD COLUMN execution_mode TEXT NOT NULL DEFAULT 'direct';
ALTER TABLE rounds ADD COLUMN checkpoint_json TEXT;
ALTER TABLE tasks ADD COLUMN profile TEXT;
ALTER TABLE tasks ADD COLUMN profile_json TEXT;
ALTER TABLE tasks ADD COLUMN profile_hash TEXT;
ALTER TABLE tasks ADD COLUMN profile_source TEXT;
CREATE TABLE worktrees (
    task_id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    runtime_dir TEXT,
    base_head TEXT,
    baseline_json TEXT,
    server_endpoint TEXT,
    server_port INTEGER,
    server_process_record TEXT,
    status TEXT NOT NULL,
    created_at TEXT,
    updated_at TEXT,
    removed_at TEXT,
    cleanup_reason TEXT,
    delivery_state TEXT,
    delivery_journal_path TEXT,
    delivered_at TEXT,
    FOREIGN KEY (task_id) REFERENCES tasks(task_id)
);
CREATE TABLE worktree_quarantine (
    entry_id TEXT PRIMARY KEY,
    original_path TEXT NOT NULL,
    quarantined_path TEXT,
    reason TEXT,
    found_at TEXT,
    status TEXT NOT NULL
);
CREATE TABLE active_writers (
    task_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    scopes_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    parallel INTEGER NOT NULL DEFAULT 0
);
DROP INDEX ux_tasks_active;
CREATE INDEX ix_tasks_project_status ON tasks(project_id, status);
CREATE INDEX ix_active_writers_project ON active_writers(project_id);
CREATE UNIQUE INDEX ux_active_writers_single ON active_writers(project_id) WHERE parallel=0;
PRAGMA user_version=15;
UPDATE meta SET value='15' WHERE key='schema_version';
"#;

/// The caller validates ownership and v6 structure and holds BEGIN IMMEDIATE.
/// No row data is rewritten or fabricated; ledger reconciliation is task 3.12d.
pub(super) fn upgrade(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(ADDITIVE_DDL)
}

pub(super) fn contract() -> Contract {
    let mut contract = super::v6_contract();
    for table in &mut contract.tables {
        match table.name.as_str() {
            "tasks" => {
                for name in ["budget_json", "workflow_id", "depends_on"] {
                    table.columns.push(column(name, "TEXT", false, 0));
                }
                table
                    .columns
                    .push(column("execution_mode", "TEXT", true, 0));
                for name in ["profile", "profile_json", "profile_hash", "profile_source"] {
                    table.columns.push(column(name, "TEXT", false, 0));
                }
            }
            "rounds" => {
                table
                    .columns
                    .push(column("structured_findings", "TEXT", false, 0));
                table
                    .columns
                    .push(column("checkpoint_json", "TEXT", false, 0));
            }
            _ => {}
        }
    }
    contract.tables.extend([
        Table {
            name: "worktrees".into(),
            columns: vec![
                column("task_id", "TEXT", false, 1),
                column("path", "TEXT", true, 0),
                column("runtime_dir", "TEXT", false, 0),
                column("base_head", "TEXT", false, 0),
                column("baseline_json", "TEXT", false, 0),
                column("server_endpoint", "TEXT", false, 0),
                column("server_port", "INTEGER", false, 0),
                column("server_process_record", "TEXT", false, 0),
                column("status", "TEXT", true, 0),
                column("created_at", "TEXT", false, 0),
                column("updated_at", "TEXT", false, 0),
                column("removed_at", "TEXT", false, 0),
                column("cleanup_reason", "TEXT", false, 0),
                column("delivery_state", "TEXT", false, 0),
                column("delivery_journal_path", "TEXT", false, 0),
                column("delivered_at", "TEXT", false, 0),
            ],
        },
        Table {
            name: "worktree_quarantine".into(),
            columns: vec![
                column("entry_id", "TEXT", false, 1),
                column("original_path", "TEXT", true, 0),
                column("quarantined_path", "TEXT", false, 0),
                column("reason", "TEXT", false, 0),
                column("found_at", "TEXT", false, 0),
                column("status", "TEXT", true, 0),
            ],
        },
        Table {
            name: "active_writers".into(),
            columns: vec![
                column("task_id", "TEXT", false, 1),
                column("project_id", "TEXT", true, 0),
                column("scopes_json", "TEXT", true, 0),
                column("created_at", "TEXT", true, 0),
                column("parallel", "INTEGER", true, 0),
            ],
        },
    ]);
    contract
        .indexes
        .retain(|entry| entry.name != "ux_tasks_active");
    contract.indexes.extend([
        index(
            "ix_tasks_project_status",
            "tasks",
            &["project_id", "status"],
            false,
            false,
        ),
        index(
            "ix_active_writers_project",
            "active_writers",
            &["project_id"],
            false,
            false,
        ),
        index(
            "ux_active_writers_single",
            "active_writers",
            &["project_id"],
            true,
            true,
        ),
    ]);
    contract.foreign_keys.push(ForeignKey {
        table: "worktrees".into(),
        from: "task_id".into(),
        to_table: "tasks".into(),
        to_column: "task_id".into(),
        on_update: "NO ACTION".into(),
        on_delete: "NO ACTION".into(),
    });
    contract
}

pub(super) fn validate(
    connection: &Connection,
    tables: &[Table],
    indexes: &[Index],
    foreign_keys: &[ForeignKey],
) -> Result<(), InspectError> {
    let contract = contract();
    super::validate_tables(&contract.tables, tables).map_err(InspectError::IncompatibleSchema)?;
    super::validate_indexes(&contract.indexes, indexes)
        .map_err(InspectError::IncompatibleSchema)?;
    super::validate_foreign_keys(&contract.foreign_keys, foreign_keys)
        .map_err(InspectError::IncompatibleSchema)?;
    // Defaults affect historical row meaning, so they are part of the guard.
    for table in tables {
        for column in &table.columns {
            let observed: Option<String> = connection
                .query_row(
                    "SELECT dflt_value FROM pragma_table_info(?1) WHERE name=?2",
                    params![table.name, column.name],
                    |row| row.get(0),
                )
                .map_err(super::classify_error)?;
            let expected = match (table.name.as_str(), column.name.as_str()) {
                ("tasks", "execution_mode") => Some("'direct'"),
                ("tasks", "revision_count")
                | ("rounds", "attempted")
                | ("active_writers", "parallel") => Some("0"),
                _ => None,
            };
            if observed.as_deref() != expected {
                return Err(InspectError::IncompatibleSchema(
                    SchemaMismatch::ColumnDefinition {
                        table: table.name.clone(),
                        column: column.name.clone(),
                    },
                ));
            }
        }
    }
    let sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='ux_active_writers_single'",
            [],
            |row| row.get(0),
        )
        .map_err(super::classify_error)?;
    let normalized: String = sql
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if !normalized.ends_with("whereparallel=0") {
        return Err(InspectError::IncompatibleSchema(
            SchemaMismatch::IndexDefinition {
                index: "ux_active_writers_single".into(),
            },
        ));
    }
    Ok(())
}
