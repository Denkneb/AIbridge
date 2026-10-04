use super::*;
use rusqlite::params;

fn owned_v16(root: &TempDir) -> RustStateLayout {
    let layout = demo_layout(&root.path);
    ensure_project_dir(&layout);
    std::fs::copy(
        fixture_dir().join("delta/owned-v16.sqlite"),
        layout.database(),
    )
    .unwrap();
    execute(
        &layout.database(),
        "UPDATE meta SET value='rust' WHERE key='runtime_owner'",
    );
    write_marker_fields(&layout, "rust", 1, "demo");
    layout
}

#[test]
fn fresh17_matches_frozen_schema_and_defaults_and_reads_fixture_without_writes() {
    let root = TempDir::new("schema17-contract");
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    let storage = layout.open().unwrap();
    let manifest: Value = serde_json::from_str(include_str!(
        "../../../../docs/fixtures/sqlite/delta/expected.json"
    ))
    .unwrap();
    let expected = &manifest["databases"]["fresh-v17.sqlite"]["schema"];
    let observed = inspect(layout.database()).unwrap();
    assert_eq!(observed.tables().len(), expected.as_object().unwrap().len());
    for (table, definition) in expected.as_object().unwrap() {
        let mut statement = storage.connection().prepare("SELECT name,type,\"notnull\",pk,dflt_value FROM pragma_table_info(?1) ORDER BY name").unwrap();
        let actual: Vec<Value> = statement
            .query_map([table], |row| {
                Ok(serde_json::json!({
                    "name":row.get::<_,String>(0)?, "type":row.get::<_,String>(1)?,
                    "notnull":row.get::<_,i64>(2)?, "pk":row.get::<_,i64>(3)?,
                    "dflt_value":row.get::<_,Option<String>>(4)?,
                }))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            actual,
            *definition["columns"].as_array().unwrap(),
            "{table}"
        );
    }
    let fixture = fixture_dir().join("delta/fresh-v17.sqlite");
    let before = file_bytes(&fixture);
    let frozen = inspect(&fixture).unwrap();
    let schema = |inspection: &crate::Inspection| {
        normalize(Contract {
            tables: inspection.tables().to_vec(),
            indexes: inspection.indexes().to_vec(),
            foreign_keys: inspection.foreign_keys().to_vec(),
        })
    };
    assert_eq!(schema(&observed), schema(&frozen));
    assert_eq!(file_bytes(&fixture), before);
}

#[test]
fn owned16_upgrade_preserves_all_rows_and_delivery_policy_and_rolls_back_failure() {
    let root = TempDir::new("schema17-upgrade");
    let layout = owned_v16(&root);
    execute(
        &layout.database(),
        "CREATE TRIGGER block17 BEFORE UPDATE ON meta WHEN NEW.key='schema_version' AND NEW.value='17' BEGIN SELECT RAISE(ABORT,'blocked'); END",
    );
    let before = logical_state(&layout.database());
    let marker = file_bytes(&layout.marker());
    assert!(matches!(
        layout.initialize(),
        Err(RustStateError::Database(_))
    ));
    assert_eq!(logical_state(&layout.database()), before);
    assert_eq!(file_bytes(&layout.marker()), marker);
    assert_eq!(inspect(layout.database()).unwrap().user_version(), 16);
    execute(&layout.database(), "DROP TRIGGER block17");
    let old = Connection::open(layout.database()).unwrap();
    let snapshot = |connection: &Connection, table: &str| {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let width = statement.column_count();
        statement
            .query_map([], |row| {
                (0..width)
                    .map(|index| row.get::<_, SqlValue>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let original: Vec<_> = super::super::read_tables(&old)
        .unwrap()
        .into_iter()
        .filter(|table| table.name != "meta")
        .map(|table| {
            let rows = snapshot(&old, &table.name);
            (table.name, rows)
        })
        .collect();
    drop(old);
    layout.initialize().unwrap();
    let storage = layout.open().unwrap();
    for (table, rows) in original {
        assert_eq!(snapshot(storage.connection(), &table), rows);
    }
    assert_eq!(count_rows(&storage, "automation_runs"), 0);
    assert_eq!(query_user_version(storage.connection()).unwrap(), 17);
    drop(storage);
    let upgraded = file_bytes(&layout.database());
    layout.initialize().unwrap();
    assert_state_unchanged(&layout, &upgraded, &marker);
}

#[test]
fn unfinished_slot_includes_paused_blocked_and_every_unknown_status() {
    let root = TempDir::new("schema17-index");
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    let storage = layout.open().unwrap();
    let connection = storage.connection();
    for status in [
        "running",
        "paused",
        "blocked",
        "unknown",
        "completed",
        "ready",
        "stopped",
    ] {
        connection
            .execute("DELETE FROM automation_runs", [])
            .unwrap();
        let insert = |id: &str| {
            connection.execute("INSERT INTO automation_runs(run_id,status,document,created_at,updated_at) VALUES (?1,?2,'{}','now','now')", params![id,status])
        };
        insert("first").unwrap();
        assert_eq!(
            insert("second").is_ok(),
            matches!(status, "completed" | "ready" | "stopped")
        );
        let control: String = connection
            .query_row(
                "SELECT control FROM automation_runs WHERE run_id='first'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(control, "run");
    }
}

#[test]
fn automation_guard_rejects_different_expression_predicate_and_default() {
    for replacement in [
        "DROP INDEX ux_automation_unfinished; CREATE UNIQUE INDEX ux_automation_unfinished ON automation_runs((2)) WHERE status NOT IN ('completed','ready','stopped')",
        "DROP INDEX ux_automation_unfinished; CREATE UNIQUE INDEX ux_automation_unfinished ON automation_runs((1)) WHERE status NOT IN ('completed','ready','stopped','paused')",
        "DROP INDEX ux_automation_unfinished; DROP TABLE automation_runs; CREATE TABLE automation_runs(run_id TEXT PRIMARY KEY,status TEXT NOT NULL,control TEXT NOT NULL DEFAULT 'pause',document TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL); CREATE UNIQUE INDEX ux_automation_unfinished ON automation_runs((1)) WHERE status NOT IN ('completed','ready','stopped')",
    ] {
        let root = TempDir::new("schema17-invalid");
        let layout = demo_layout(&root.path);
        layout.initialize().unwrap();
        execute(&layout.database(), replacement);
        let before = file_bytes(&layout.database());
        assert!(inspect(layout.database()).is_err());
        assert!(layout.initialize().is_err());
        assert!(layout.open().is_err());
        assert_eq!(file_bytes(&layout.database()), before);
    }
}
