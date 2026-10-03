use super::*;
use crate::{TaskBudget, normalize_persisted_budget, read_task_budget_readonly, validate_budget};
use serde_json::json;

fn budget() -> TaskBudget {
    TaskBudget::from_json(&json!({"limits":{"input":100000,"cost":5.0}})).unwrap()
}
fn state(tag: &str) -> (TempDir, RustStateLayout, TaskId, ProjectId) {
    let root = TempDir::new(tag);
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    (root, layout, query_task_id(1), query_project("demo"))
}
#[test]
fn validates_all_limit_fields_preserves_numbers_and_defaults_threshold() {
    let limits = json!({"input":u64::MAX,"output":1,"reasoning":2.5,"cache_read":3,"cache_write":4,"cost":0.01});
    let b = TaskBudget::from_json(&json!({"limits":limits})).unwrap();
    assert_eq!(b.as_json()["limits"], limits);
    assert_eq!(b.warning_threshold(), 0.8);
    assert_eq!(b.as_json()["limits"]["input"].as_u64(), Some(u64::MAX));
    for t in [json!(1), json!(0.0001), json!(0.8)] {
        let b =
            TaskBudget::from_json(&json!({"limits":{"input":1},"warning_threshold":t})).unwrap();
        assert_eq!(b.as_json()["warning_threshold"], t);
    }
    assert_eq!(validate_budget(&json!(null)).unwrap(), None);
    assert_eq!(normalize_persisted_budget(None).unwrap(), None);
    assert_eq!(
        normalize_persisted_budget(Some("{\"limits\":{\"input\":100000,\"cost\":5.0}}")).unwrap(),
        Some(budget())
    );
}
#[test]
fn malformed_shapes_unknown_keys_and_invalid_numbers_are_rejected_safely() {
    let cases = [
        json!({}),
        json!([]),
        json!("secret-input"),
        json!(true),
        json!(1),
        json!({"limits":{}}),
        json!({"limits":[]}),
        json!({"limits":{"secret-limit":1}}),
        json!({"limits":{"input":0}}),
        json!({"limits":{"input":-1}}),
        json!({"limits":{"input":true}}),
        json!({"limits":{"input":"1"}}),
        json!({"limits":{"input":null}}),
        json!({"limits":{"input":1},"secret-field":2}),
        json!({"limits":{"input":1},"warning_threshold":0}),
        json!({"limits":{"input":1},"warning_threshold":-0.2}),
        json!({"limits":{"input":1},"warning_threshold":1.001}),
        json!({"limits":{"input":1},"warning_threshold":true}),
        json!({"limits":{"input":1},"warning_threshold":null}),
    ];
    for case in cases {
        let e = validate_budget(&case).unwrap_err();
        assert!(!format!("{e:?}: {e}").contains("secret"));
        assert!(normalize_persisted_budget(Some(&case.to_string())).is_err());
    }
    for raw in [
        "null",
        "",
        "not-json-secret",
        "NaN",
        "Infinity",
        "{\"limits\":{\"cost\":1e400}}",
        "{\"limits\":{\"cost\":1},\"warning_threshold\":NaN}",
    ] {
        assert!(normalize_persisted_budget(Some(raw)).is_err());
    }
}
#[test]
fn creation_budget_round_reservation_and_event_commit_together_and_reopen() {
    let (_root, layout, t, p) = state("budget-create");
    let b = budget();
    let mut s = layout.open().unwrap();
    let outcome = s
        .create_task_with_budget(
            create_task_input(t, &p, "budget-request"),
            &crate::AdmissionSettings::default(),
            TaskStatus::Implementing,
            Some(&b),
        )
        .unwrap();
    assert_eq!(outcome.task().budget, Some(b.clone()));
    let raw: String = s
        .connection()
        .query_row("SELECT budget_json FROM tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
        *b.as_json()
    );
    assert_eq!(count_rows(&s, "rounds"), 1);
    assert_eq!(count_rows(&s, "events"), 1);
    assert_eq!(count_rows(&s, "active_writers"), 1);
    drop(s);
    let s = layout.open().unwrap();
    assert_eq!(s.get_task(t).unwrap().unwrap().budget, Some(b.clone()));
    assert_eq!(
        s.get_active_task(&p).unwrap().unwrap().budget,
        Some(b.clone())
    );
    assert_eq!(s.list_tasks(&p, false, 10, 0).unwrap()[0].budget, Some(b));
}
#[test]
fn budget_creation_failure_rolls_back_every_table_and_redacts_trigger_text() {
    let (_root, layout, t, p) = state("budget-rollback");
    let mut s = layout.open().unwrap();
    s.connection().execute_batch("CREATE TRIGGER reject_budget BEFORE UPDATE OF budget_json ON tasks WHEN NEW.budget_json IS NOT NULL BEGIN SELECT RAISE(ABORT,'secret-budget'); END;").unwrap();
    let e = s
        .create_task_with_budget(
            create_task_input(t, &p, "request"),
            &crate::AdmissionSettings::default(),
            TaskStatus::Implementing,
            Some(&budget()),
        )
        .unwrap_err();
    assert!(!format!("{e:?}: {e}").contains("secret-budget"));
    for table in ["tasks", "rounds", "events", "active_writers"] {
        assert_eq!(count_rows(&s, table), 0);
    }
    s.connection()
        .execute("DROP TRIGGER reject_budget", [])
        .unwrap();
    s.connection().execute_batch("CREATE TRIGGER reject_created_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'secret-event'); END;").unwrap();
    assert!(
        s.create_task_with_budget(
            create_task_input(t, &p, "request"),
            &crate::AdmissionSettings::default(),
            TaskStatus::Implementing,
            Some(&budget())
        )
        .is_err()
    );
    for table in ["tasks", "rounds", "events", "active_writers"] {
        assert_eq!(count_rows(&s, table), 0);
    }
}
#[test]
fn default_creation_is_null_and_replay_preserves_original_budget() {
    let (_root, layout, t, p) = state("budget-replay");
    let mut s = layout.open().unwrap();
    let original = budget();
    s.create_task_with_budget(
        create_task_input(t, &p, "request"),
        &crate::AdmissionSettings::default(),
        TaskStatus::Implementing,
        Some(&original),
    )
    .unwrap();
    let before = ["tasks", "rounds", "events", "active_writers"].map(|name| dump_rows(&s, name));
    let replay = s
        .create_task_with_budget(
            create_task_input(query_task_id(2), &p, "request"),
            &crate::AdmissionSettings::default(),
            TaskStatus::Implementing,
            None,
        )
        .unwrap();
    assert!(matches!(replay, CreateTaskOutcome::Replayed(_)));
    assert_eq!(replay.task().budget, Some(original));
    assert_eq!(
        ["tasks", "rounds", "events", "active_writers"].map(|name| dump_rows(&s, name)),
        before
    );
    let mut conflict = create_task_input(query_task_id(2), &p, "request");
    conflict.payload_hash = "different-budget-payload".into();
    assert!(matches!(
        s.create_task_with_budget(
            conflict,
            &crate::AdmissionSettings::default(),
            TaskStatus::Implementing,
            Some(&budget())
        ),
        Err(CreateTaskError::RequestConflict)
    ));
    let (_root, layout, t, p) = state("budget-none");
    let mut s = layout.open().unwrap();
    assert_eq!(
        s.create_task(create_task_input(t, &p, "request"))
            .unwrap()
            .task()
            .budget,
        None
    );
    let raw: Option<String> = s
        .connection()
        .query_row("SELECT budget_json FROM tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(raw, None);
}
#[test]
fn corrupt_stored_budget_fails_all_task_reads_replay_and_status_updates() {
    let (_root, layout, t, p) = state("budget-corrupt");
    let mut s = layout.open().unwrap();
    s.create_task(create_task_input(t, &p, "request")).unwrap();
    for raw in [
        "null",
        "{}",
        "not-json-secret",
        "{\"limits\":{\"input\":0}}",
        "{\"limits\":{\"secret-key\":1}}",
    ] {
        s.connection()
            .execute("UPDATE tasks SET budget_json=?1", [raw])
            .unwrap();
        assert!(matches!(
            s.get_task(t),
            Err(QueryError::TaskRow(TaskRowError::InvalidBudget))
        ));
        assert!(s.get_active_task(&p).is_err());
        assert!(s.list_tasks(&p, false, 10, 0).is_err());
        let before = dump_rows(&s, "tasks");
        assert!(
            s.update_task_status(t, &p, TaskStatus::Closed, None)
                .is_err()
        );
        assert_eq!(dump_rows(&s, "tasks"), before);
        let e = s
            .create_task(create_task_input(t, &p, "request"))
            .unwrap_err();
        assert!(!format!("{e:?}: {e}").contains("secret"));
    }
    s.connection()
        .execute("UPDATE tasks SET budget_json=x'ff'", [])
        .unwrap();
    assert!(matches!(
        s.get_task(t),
        Err(QueryError::TaskRow(TaskRowError::ColumnType {
            column: "budget_json"
        }))
    ));
    s.connection()
        .execute("UPDATE tasks SET budget_json=NULL", [])
        .unwrap();
    assert_eq!(s.get_task(t).unwrap().unwrap().budget, None);
}
#[test]
fn legacy_v6_remains_without_budget_and_nonempty_submission_is_rejected() {
    let root = TempDir::new("budget-legacy");
    let path = root.join("state.sqlite");
    initialize(&path).unwrap();
    let mut s = connect(&path).unwrap();
    let t = query_task_id(1);
    let p = query_project("demo");
    assert!(matches!(
        s.create_task_with_budget(
            create_task_input(t, &p, "request"),
            &crate::AdmissionSettings::default(),
            TaskStatus::Implementing,
            Some(&budget())
        ),
        Err(CreateTaskError::InvalidInput)
    ));
    assert_eq!(count_rows(&s, "tasks"), 0);
    assert_eq!(
        s.create_task(create_task_input(t, &p, "request"))
            .unwrap()
            .task()
            .budget,
        None
    );
    assert_eq!(s.get_task(t).unwrap().unwrap().budget, None);
    let before = file_bytes(&path);
    assert_eq!(read_task_budget_readonly(&path, t, &p).unwrap(), Some(None));
    assert_eq!(file_bytes(&path), before);
}
#[test]
fn readonly_budget_distinguishes_absence_null_and_corruption_and_sees_live_wal() {
    let (root, layout, t, p) = state("budget-readonly");
    let missing = root.join("missing/state.sqlite");
    assert_eq!(read_task_budget_readonly(&missing, t, &p).unwrap(), None);
    assert!(!root.join("missing").exists());
    let mut s = layout.open().unwrap();
    s.create_task_with_budget(
        create_task_input(t, &p, "request"),
        &crate::AdmissionSettings::default(),
        TaskStatus::Implementing,
        Some(&budget()),
    )
    .unwrap();
    let path: String = s
        .connection()
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let path = Path::new(&path);
    let before = dump_rows(&s, "tasks");
    let journal: String = s
        .connection()
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(
        read_task_budget_readonly(path, t, &p).unwrap(),
        Some(Some(budget()))
    );
    assert_eq!(
        read_task_budget_readonly(path, t, &query_project("foreign")).unwrap(),
        None
    );
    assert_eq!(
        read_task_budget_readonly(path, query_task_id(2), &p).unwrap(),
        None
    );
    assert_eq!(dump_rows(&s, "tasks"), before);
    assert_eq!(
        s.connection()
            .pragma_query_value::<String, _>(None, "journal_mode", |r| r.get(0))
            .unwrap(),
        journal
    );
    s.connection()
        .execute("UPDATE tasks SET budget_json='null'", [])
        .unwrap();
    let e = read_task_budget_readonly(path, t, &p).unwrap_err();
    assert!(!format!("{e:?}: {e}").contains("null"));
    s.connection()
        .execute("UPDATE tasks SET budget_json=NULL", [])
        .unwrap();
    assert_eq!(read_task_budget_readonly(path, t, &p).unwrap(), Some(None));
    s.connection()
        .pragma_update(None, "user_version", 999)
        .unwrap();
    assert!(read_task_budget_readonly(path, t, &p).is_err());
}
