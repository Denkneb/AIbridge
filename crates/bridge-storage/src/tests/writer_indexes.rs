//! Independent intermediate fixtures derived from a copy of frozen Python v15,
//! not from the production migration DDL. Never modify committed fixtures.
use super::*;

fn intermediate(layout: &RustStateLayout, version: i64) {
    ensure_project_dir(layout);
    std::fs::copy(fixture_dir().join("empty-v15.sqlite"), layout.database()).expect("copy fixture");
    let connection = Connection::open(layout.database()).expect("open copy");
    connection.execute_batch(
        "DROP INDEX ux_active_writers_single; DROP INDEX ix_active_writers_project; DROP INDEX ix_tasks_project_status;",
    ).expect("remove v15 indexes");
    if version == 11 {
        connection.execute_batch(
            "DROP TABLE active_writers; ALTER TABLE rounds DROP COLUMN checkpoint_json; \
             ALTER TABLE tasks DROP COLUMN profile; ALTER TABLE tasks DROP COLUMN profile_json; \
             ALTER TABLE tasks DROP COLUMN profile_hash; ALTER TABLE tasks DROP COLUMN profile_source; \
             CREATE UNIQUE INDEX ux_tasks_active ON tasks(project_id) WHERE status IN \
             ('waiting_dependencies','implementing','awaiting_review','revising','needs_user','failed','delivery_unknown');",
        ).expect("v11 reference structure");
    } else {
        assert_eq!(version, 14);
        connection.execute_batch(
            "ALTER TABLE active_writers DROP COLUMN parallel; \
             CREATE UNIQUE INDEX ux_active_writers_project ON active_writers(project_id); \
             CREATE UNIQUE INDEX ux_tasks_active ON tasks(project_id) WHERE status IN \
             ('implementing','awaiting_review','revising','needs_user','failed','delivery_unknown');",
        ).expect("v14 reference structure");
    }
    connection.execute_batch(&format!(
        "PRAGMA user_version={version}; UPDATE meta SET value='{version}' WHERE key='schema_version'; \
         INSERT INTO meta VALUES ('runtime_owner','rust');",
    )).expect("legacy version and owner");
    drop(connection);
    write_marker_fields(layout, "rust", 1, "demo");
}

fn task(connection: &Connection, id: &str, status: &str) -> rusqlite::Result<usize> {
    connection.execute(
        "INSERT INTO tasks(task_id,project_id,workspace,status,task,allowed_paths,test_commands,created_at,updated_at) \
         VALUES (?1,'demo','/fixture/workspace',?2,'legacy','[\"src/one.rs\"]','[]','created','updated')",
        rusqlite::params![id, status],
    )
}

fn writer(
    connection: &Connection,
    id: &str,
    project: &str,
    parallel: i64,
) -> rusqlite::Result<usize> {
    connection.execute(
        "INSERT INTO active_writers(task_id,project_id,scopes_json,created_at,parallel) VALUES (?1,?2,?3,'created',?4)",
        rusqlite::params![id, project, format!("[\"src/{id}.rs\"]"), parallel],
    )
}

fn snapshot(connection: &Connection, table: &str, columns: &str) -> Vec<Vec<SqlValue>> {
    let mut statement = connection
        .prepare(&format!("SELECT {columns} FROM {table} ORDER BY rowid"))
        .expect("snapshot");
    let width = statement.column_count();
    statement
        .query_map([], |row| {
            (0..width)
                .map(|index| row.get(index))
                .collect::<rusqlite::Result<Vec<SqlValue>>>()
        })
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("rows")
}

#[test]
fn historical_writer_status_indexes_keep_v11_and_v14_distinct() {
    for version in [11, 14] {
        let root = TempDir::new("historical-index");
        let layout = demo_layout(&root.path);
        intermediate(&layout, version);
        assert_eq!(
            inspect(layout.database())
                .expect("legacy inspection")
                .user_version(),
            version
        );
        let connection = Connection::open(layout.database()).expect("open");
        task(&connection, "first", "implementing").expect("writer");
        for status in [
            "implementing",
            "awaiting_review",
            "revising",
            "needs_user",
            "failed",
            "delivery_unknown",
        ] {
            assert!(
                task(&connection, "second", status).is_err(),
                "v{version} {status}"
            );
        }
        if version == 11 {
            assert!(task(&connection, "waiting", "waiting_dependencies").is_err());
        } else {
            task(&connection, "waiting-1", "waiting_dependencies").expect("queued v14");
            task(&connection, "waiting-2", "waiting_dependencies").expect("another queued v14");
        }
        task(&connection, "accepted", "accepted").expect("terminal");
        task(&connection, "closed", "closed").expect("terminal");
    }
}

#[test]
fn intermediate_upgrades_preserve_every_column_and_v14_reservations() {
    for version in [11, 14] {
        let root = TempDir::new("intermediate-upgrade");
        let layout = demo_layout(&root.path);
        intermediate(&layout, version);
        let connection = Connection::open(layout.database()).expect("legacy");
        task(&connection, VALID_TASK_ID, "implementing").expect("legacy writer");
        if version == 14 {
            task(&connection, "waiting", "waiting_dependencies").expect("queued");
            connection.execute("INSERT INTO active_writers VALUES (?1,'demo','[\"src/one.rs\"]','original-time')", [VALID_TASK_ID]).expect("legacy reservation");
            connection.execute_batch("UPDATE tasks SET profile='custom',profile_json='opaque legacy snapshot',profile_hash='legacy hash',profile_source='config' WHERE status='implementing'").expect("profile metadata");
        }
        let original: Vec<_> = super::super::read_tables(&connection)
            .expect("tables")
            .into_iter()
            .filter(|table| table.name != "meta")
            .map(|table| {
                let columns = table
                    .columns
                    .into_iter()
                    .map(|column| column.name)
                    .collect::<Vec<_>>()
                    .join(",");
                let rows = snapshot(&connection, &table.name, &columns);
                (table.name, columns, rows)
            })
            .collect();
        drop(connection);
        let marker = file_bytes(&layout.marker());
        layout.initialize().expect("guarded upgrade");
        let storage = layout.open().expect("v15 open");
        for (table, columns, rows) in original {
            assert_eq!(
                snapshot(storage.connection(), &table, &columns),
                rows,
                "v{version} {table}"
            );
        }
        let flags: Vec<i64> = storage
            .connection()
            .prepare("SELECT parallel FROM active_writers")
            .expect("flags")
            .query_map([], |row| row.get(0))
            .expect("flags query")
            .collect::<rusqlite::Result<_>>()
            .expect("flags rows");
        assert_eq!(flags, vec![0]);
        assert_eq!(
            query_user_version(storage.connection()).expect("version"),
            15
        );
        let inspection = inspect(layout.database()).expect("v15 inspect");
        assert!(!inspection.indexes().iter().any(|entry| matches!(
            entry.name.as_str(),
            "ux_tasks_active" | "ux_active_writers_project"
        )));
        drop(storage);
        let upgraded = file_bytes(&layout.database());
        layout.initialize().expect("repeat");
        assert_state_unchanged(&layout, &upgraded, &marker);
    }
}

#[test]
fn v15_partial_unique_index_allows_parallel_rows_and_restricts_singletons() {
    for origin in [6, 11, 14, 15] {
        for (left, right, allowed) in [(0, 0, false), (0, 1, true), (1, 0, true), (1, 1, true)] {
            let root = TempDir::new("writer-index-matrix");
            let layout = demo_layout(&root.path);
            if matches!(origin, 11 | 14) {
                intermediate(&layout, origin);
            } else if origin == 6 {
                ensure_project_dir(&layout);
                create_v6(&layout.database());
                execute(
                    &layout.database(),
                    "INSERT INTO meta VALUES ('runtime_owner','rust')",
                );
                write_marker_fields(&layout, "rust", 1, "demo");
            }
            layout.initialize().expect("v15");
            let storage = layout.open().expect("open");
            let connection = storage.connection();
            task(connection, "first", "implementing").expect("first task");
            task(connection, "second", "revising").expect("v15 tasks have no project unique index");
            writer(connection, "first", "demo", left).expect("first writer");
            assert_eq!(
                writer(connection, "second", "demo", right).is_ok(),
                allowed,
                "origin {origin}: {left}/{right}"
            );
            writer(connection, "other", "other-project", 0).expect("project isolation");
            assert!(
                writer(connection, "first", "other-project", 1).is_err(),
                "task identity remains unique"
            );
        }
    }
}

#[test]
fn intermediate_owner_and_schema_failures_never_modify_state() {
    for version in [11, 14] {
        for mutation in [
            "DELETE FROM meta WHERE key='runtime_owner'",
            "UPDATE meta SET value='python' WHERE key='runtime_owner'",
            "UPDATE meta SET value='15' WHERE key='schema_version'",
            "DROP INDEX ux_tasks_active; CREATE UNIQUE INDEX ux_tasks_active ON tasks(project_id) WHERE status='implementing'",
            "PRAGMA user_version=13; UPDATE meta SET value='13' WHERE key='schema_version'",
        ] {
            let root = TempDir::new("intermediate-rejection");
            let layout = demo_layout(&root.path);
            intermediate(&layout, version);
            execute(&layout.database(), mutation);
            let database = file_bytes(&layout.database());
            let marker = file_bytes(&layout.marker());
            assert!(layout.open().is_err());
            assert!(layout.initialize().is_err());
            assert_state_unchanged(&layout, &database, &marker);
        }
        let root = TempDir::new("foreign-intermediate-marker");
        let layout = demo_layout(&root.path);
        intermediate(&layout, version);
        write_marker_fields(&layout, "python", 1, "demo");
        let database = file_bytes(&layout.database());
        let marker = file_bytes(&layout.marker());
        assert!(matches!(
            layout.initialize(),
            Err(RustStateError::ForeignImplementation)
        ));
        assert_state_unchanged(&layout, &database, &marker);
    }
}

#[test]
fn intermediate_upgrade_failure_rolls_back_indexes_columns_and_versions() {
    for version in [11, 14] {
        let root = TempDir::new("intermediate-rollback");
        let layout = demo_layout(&root.path);
        intermediate(&layout, version);
        execute(
            &layout.database(),
            "CREATE TRIGGER block_upgrade BEFORE UPDATE ON meta WHEN NEW.key='schema_version' BEGIN SELECT RAISE(ABORT,'blocked'); END",
        );
        let before = inspect(layout.database()).expect("legacy schema");
        assert!(matches!(
            layout.initialize(),
            Err(RustStateError::Database(_))
        ));
        let after = inspect(layout.database()).expect("rolled back legacy schema");
        assert_eq!(before, after);
        execute(&layout.database(), "DROP TRIGGER block_upgrade");
        layout.initialize().expect("retry");
        assert_compatible_empty_v15(&layout.database());
    }
}

#[test]
fn intermediate_concurrent_upgrade_has_one_complete_target() {
    for version in [11, 14] {
        let root = TempDir::new("intermediate-concurrent");
        let layout = demo_layout(&root.path);
        intermediate(&layout, version);
        drop(connect(layout.database()).expect("WAL before contention"));
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..6)
                .map(|_| scope.spawn(|| layout.initialize()))
                .collect();
            for handle in handles {
                handle.join().expect("thread").expect("upgrade");
            }
        });
        assert_compatible_empty_v15(&layout.database());
    }
}

#[test]
fn v15_guard_rejects_extra_predicate_before_parallel_zero() {
    let root = TempDir::new("predicate-prefix");
    let layout = demo_layout(&root.path);
    layout.initialize().expect("v15");
    execute(
        &layout.database(),
        "DROP INDEX ux_active_writers_single; CREATE UNIQUE INDEX ux_active_writers_single ON active_writers(project_id) WHERE project_id='other' OR parallel=0",
    );
    let database = file_bytes(&layout.database());
    let marker = file_bytes(&layout.marker());
    assert!(layout.open().is_err());
    assert!(layout.initialize().is_err());
    assert_state_unchanged(&layout, &database, &marker);
}
