use super::*;
use crate::AdmissionSettings;
use bridge_domain::{DeliveryMode, ExecutionMode};

#[test]
fn frozen_delivery_replay_and_readonly_mapping_ignore_live_settings() {
    let root = TempDir::new("delivery-policy");
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    let mut storage = layout.open().unwrap();
    let input = create_task_input(query_task_id(1), &query_project("demo"), "request");
    let automatic = AdmissionSettings::new(1, false, ExecutionMode::Worktree)
        .unwrap()
        .with_delivery_mode(DeliveryMode::OnAccept)
        .unwrap();
    let task = storage
        .create_task_with_admission(input.clone(), &automatic, TaskStatus::Implementing)
        .unwrap()
        .into_task();
    assert_eq!(task.delivery_mode, DeliveryMode::OnAccept);
    let replay = storage
        .create_task_with_admission(
            input,
            &AdmissionSettings::default(),
            TaskStatus::Implementing,
        )
        .unwrap();
    assert!(replay.is_replayed());
    assert_eq!(replay.task().delivery_mode, DeliveryMode::OnAccept);
    assert_eq!(
        storage
            .get_task(task.task_id)
            .unwrap()
            .unwrap()
            .delivery_mode,
        DeliveryMode::OnAccept
    );
    assert!(
        AdmissionSettings::default()
            .with_delivery_mode(DeliveryMode::OnAccept)
            .is_err()
    );
    let readonly = super::super::open_read_only_current(&layout.database()).unwrap();
    let saved = readonly
        .query_row("SELECT * FROM tasks", [], |row| Ok(Task::from_row(row)))
        .unwrap()
        .unwrap();
    assert_eq!(saved.delivery_mode, DeliveryMode::OnAccept);
}

#[test]
fn delivery_mapping_defaults_for_legacy_corrupt_and_wrong_sql_types() {
    let (_root, mut storage) = open_query_storage("delivery-legacy");
    let id = query_task_id(1);
    let input = create_task_input(id, &query_project("demo"), "request");
    let task = storage.create_task(input).unwrap().into_task();
    assert_eq!(task.delivery_mode, DeliveryMode::Manual);
    storage
        .connection()
        .execute_batch("ALTER TABLE tasks ADD COLUMN delivery_mode")
        .unwrap();
    for value in [
        SqlValue::Null,
        SqlValue::Integer(1),
        SqlValue::Real(1.0),
        SqlValue::Blob(b"on_accept".to_vec()),
        SqlValue::Text("unknown".into()),
        SqlValue::Text(" on_accept".into()),
        SqlValue::Text("ON_ACCEPT".into()),
        SqlValue::Text("manual".into()),
    ] {
        storage
            .connection()
            .execute("UPDATE tasks SET delivery_mode=?1", [value])
            .unwrap();
        assert_eq!(
            storage.get_task(id).unwrap().unwrap().delivery_mode,
            DeliveryMode::Manual
        );
    }
    storage
        .connection()
        .execute("UPDATE tasks SET delivery_mode='on_accept'", [])
        .unwrap();
    assert_eq!(
        storage.get_task(id).unwrap().unwrap().delivery_mode,
        DeliveryMode::OnAccept
    );
}

#[test]
fn delivery_insert_is_atomic_and_legacy_automatic_policy_is_rejected() {
    let root = TempDir::new("delivery-rollback");
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    let mut storage = layout.open().unwrap();
    let id = query_task_id(1);
    let input = create_task_input(id, &query_project("demo"), "request");
    let automatic = AdmissionSettings::new(1, false, ExecutionMode::Worktree)
        .unwrap()
        .with_delivery_mode(DeliveryMode::OnAccept)
        .unwrap();
    storage.connection().execute_batch("CREATE TRIGGER block_delivery BEFORE UPDATE OF delivery_mode ON tasks BEGIN SELECT RAISE(ABORT,'secret'); END").unwrap();
    let error = storage
        .create_task_with_admission(input.clone(), &automatic, TaskStatus::Implementing)
        .unwrap_err();
    assert!(!format!("{error} {error:?}").contains("secret"));
    for table in ["tasks", "rounds", "events", "active_writers"] {
        assert_eq!(count_rows(&storage, table), 0);
    }
    storage
        .connection()
        .execute_batch("DROP TRIGGER block_delivery")
        .unwrap();
    assert_eq!(
        storage
            .create_task_with_admission(input, &automatic, TaskStatus::Implementing)
            .unwrap()
            .task()
            .delivery_mode,
        DeliveryMode::OnAccept
    );
    let (_root, mut legacy) = open_query_storage("delivery-legacy-refusal");
    let error = legacy
        .create_task_with_admission(
            create_task_input(id, &query_project("demo"), "request"),
            &automatic,
            TaskStatus::Implementing,
        )
        .unwrap_err();
    assert!(matches!(error, CreateTaskError::InvalidInput));
    assert_eq!(count_rows(&legacy, "tasks"), 0);
}
