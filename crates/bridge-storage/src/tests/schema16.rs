use super::*;

fn owned_v15(root: &TempDir) -> RustStateLayout {
    let layout = demo_layout(&root.path);
    ensure_project_dir(&layout);
    std::fs::copy(
        fixture_dir().join("delta/owned-v15.sqlite"),
        layout.database(),
    )
    .unwrap();
    let connection = Connection::open(layout.database()).unwrap();
    connection
        .execute("UPDATE meta SET value='rust' WHERE key='runtime_owner'", [])
        .unwrap();
    drop(connection);
    write_marker_fields(&layout, "rust", 1, "demo");
    layout
}

#[test]
fn schema16_contract_defaults_match_independent_delta_manifest() {
    let root = TempDir::new("schema16-contract");
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    let storage = layout.open().unwrap();
    let expected: Value = serde_json::from_str(include_str!(
        "../../../../docs/fixtures/sqlite/delta/expected.json"
    ))
    .unwrap();
    let expected = &expected["databases"]["owned-v16.sqlite"]["schema"];
    let observed = inspect(layout.database()).unwrap();
    let mut names: Vec<_> = observed
        .tables()
        .iter()
        .map(|table| table.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        expected
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    for (table, definition) in expected.as_object().unwrap() {
        let mut statement = storage.connection().prepare("SELECT name,type,\"notnull\",pk,dflt_value FROM pragma_table_info(?1) ORDER BY name").unwrap();
        let actual: Vec<Value> = statement
            .query_map([table], |row| {
                Ok(serde_json::json!({
                    "name": row.get::<_,String>(0)?, "type": row.get::<_,String>(1)?,
                    "notnull": row.get::<_,i64>(2)?, "pk": row.get::<_,i64>(3)?,
                    "dflt_value": row.get::<_,Option<String>>(4)?,
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
}

#[test]
fn readonly_v15_is_unchanged_then_upgrade_preserves_every_row() {
    let root = TempDir::new("schema16-preserve");
    let layout = owned_v15(&root);
    let before = file_bytes(&layout.database());
    let marker = file_bytes(&layout.marker());
    super::super::validate_rust_database(&layout.database()).unwrap();
    let storage = super::super::open_read_only_current(&layout.database()).unwrap();
    assert_eq!(query_user_version(&storage).unwrap(), 15);
    drop(storage);
    assert_eq!(file_bytes(&layout.database()), before);
    let snapshot = |connection: &Connection, table: &str, columns: &str| {
        let mut statement = connection
            .prepare(&format!("SELECT {columns} FROM {table} ORDER BY rowid"))
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
    let old = Connection::open(layout.database()).unwrap();
    let original: Vec<_> = super::super::read_tables(&old)
        .unwrap()
        .into_iter()
        .filter(|table| table.name != "meta")
        .map(|table| {
            let columns = table
                .columns
                .iter()
                .map(|col| col.name.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let rows = snapshot(&old, &table.name, &columns);
            (table.name, columns, rows)
        })
        .collect();
    drop(old);
    layout.initialize().unwrap();
    let storage = layout.open().unwrap();
    assert_eq!(query_user_version(storage.connection()).unwrap(), 16);
    for (table, columns, rows) in original {
        assert_eq!(snapshot(storage.connection(), &table, &columns), rows);
    }
    let count: i64 = storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE delivery_mode!='manual'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    drop(storage);
    let upgraded = file_bytes(&layout.database());
    layout.initialize().unwrap();
    assert_state_unchanged(&layout, &upgraded, &marker);
}

#[test]
fn schema16_metadata_failure_rolls_back_column_and_both_markers() {
    let root = TempDir::new("schema16-rollback");
    let layout = owned_v15(&root);
    execute(
        &layout.database(),
        "CREATE TRIGGER block16 BEFORE UPDATE ON meta WHEN NEW.key='schema_version' AND NEW.value='16' BEGIN SELECT RAISE(ABORT,'blocked'); END",
    );
    let before = logical_state(&layout.database());
    let marker = file_bytes(&layout.marker());
    assert!(matches!(
        layout.initialize(),
        Err(RustStateError::Database(_))
    ));
    assert_eq!(logical_state(&layout.database()), before);
    assert_eq!(file_bytes(&layout.marker()), marker);
    assert_eq!(inspect(layout.database()).unwrap().user_version(), 15);
    execute(&layout.database(), "DROP TRIGGER block16");
    layout.initialize().unwrap();
}

#[test]
fn schema16_rejects_foreign_owner_and_wrong_default_without_writes() {
    let root = TempDir::new("schema16-foreign");
    let layout = owned_v15(&root);
    execute(
        &layout.database(),
        "UPDATE meta SET value='python' WHERE key='runtime_owner'",
    );
    let before = file_bytes(&layout.database());
    assert!(matches!(
        layout.initialize(),
        Err(RustStateError::ForeignRuntimeOwner)
    ));
    assert_eq!(file_bytes(&layout.database()), before);
    execute(
        &layout.database(),
        "UPDATE meta SET value='rust' WHERE key='runtime_owner'; ALTER TABLE tasks ADD COLUMN delivery_mode TEXT NOT NULL DEFAULT 'on_accept'; PRAGMA user_version=16; UPDATE meta SET value='16' WHERE key='schema_version'",
    );
    let before = file_bytes(&layout.database());
    assert!(layout.initialize().is_err());
    assert!(super::super::validate_rust_database(&layout.database()).is_err());
    assert_eq!(file_bytes(&layout.database()), before);
}
