use super::*;
use crate::automation::{AutomationRunStore, AutomationStoreError, RunControl, RunId, RunStatus};
use serde_json::json;

const FIRST: &str = "11111111-1111-4111-8111-111111111111";
const SECOND: &str = "22222222-2222-4222-8222-222222222222";
fn document(id: &str) -> Value {
    json!({"run_id":id,"plan":{"secret":"secret-document"}})
}
fn fixture(tag: &str) -> (TempDir, RustStateLayout, AutomationRunStore) {
    let root = TempDir::new(tag);
    let layout = demo_layout(&root.path);
    let store = AutomationRunStore::new(layout.clone());
    (root, layout, store)
}

#[test]
fn readonly_missing_and_invalid_create_do_not_initialize_state() {
    let (_root, layout, store) = fixture("automation-missing");
    assert_eq!(
        store.load(None).unwrap_err(),
        AutomationStoreError::NotFound
    );
    for doc in [
        json!([]),
        json!({}),
        document("not-a-uuid-secret"),
        document("11111111111141118111111111111111"),
        document("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA"),
    ] {
        let error = store.create(&doc).unwrap_err();
        assert_eq!(error, AutomationStoreError::InvalidIdentity);
        assert!(!format!("{error} {error:?}").contains("secret"));
    }
    assert!(!layout.database().exists());
    assert!(!layout.marker().exists());
}

#[test]
fn create_load_save_control_and_latest_use_durable_columns_and_fence_terminal_rows() {
    let (_root, layout, store) = fixture("automation-crud");
    let doc = document(FIRST);
    let first = store.create(&doc).unwrap();
    let id: RunId = FIRST.parse().unwrap();
    assert_eq!(first.id(), id);
    assert_eq!(first.status(), RunStatus::Running);
    assert_eq!(first.control(), RunControl::Run);
    assert_eq!(store.load(None).unwrap(), first);
    assert_eq!(
        store.create(&doc).unwrap_err(),
        AutomationStoreError::AlreadyExists
    );
    assert_eq!(
        store.create(&document(SECOND)).unwrap_err(),
        AutomationStoreError::UnfinishedRun
    );
    assert_eq!(
        store.set_control(id, RunControl::Pause).unwrap().control(),
        RunControl::Pause
    );
    let mut stale = first.document().clone();
    stale["control"] = json!("run");
    stale["status"] = json!("completed");
    stale["checkpoint"] = json!("secret-checkpoint");
    let paused = store.save(&stale, RunStatus::Paused).unwrap();
    assert_eq!(paused.control(), RunControl::Pause);
    assert_eq!(paused.document()["status"], "paused");
    assert_eq!(paused.document()["control"], "pause");
    assert_eq!(paused.created_at(), first.created_at());
    assert_eq!(
        store.create(&document(SECOND)).unwrap_err(),
        AutomationStoreError::UnfinishedRun
    );
    let blocked = store.save(&stale, RunStatus::Blocked).unwrap();
    assert_eq!(
        store.create(&document(SECOND)).unwrap_err(),
        AutomationStoreError::UnfinishedRun
    );
    assert_eq!(
        store.set_control(id, RunControl::Stop).unwrap().control(),
        RunControl::Stop
    );
    let ready = store.save(blocked.document(), RunStatus::Ready).unwrap();
    assert_eq!(ready.control(), RunControl::Stop);
    assert_eq!(
        store.set_control(id, RunControl::Run).unwrap_err(),
        AutomationStoreError::TerminalRun
    );
    assert_eq!(
        store
            .save(ready.document(), RunStatus::Running)
            .unwrap_err(),
        AutomationStoreError::TerminalRun
    );
    let second = store.create(&document(SECOND)).unwrap();
    // Tie timestamps deliberately: latest uses rowid as the reference does.
    execute(
        &layout.database(),
        "UPDATE automation_runs SET created_at='same'",
    );
    assert_eq!(store.load(None).unwrap().id(), second.id());
    assert_eq!(store.load(Some(id)).unwrap().status(), RunStatus::Ready);
    let before = file_bytes(&layout.database());
    assert!(!format!("{ready:?} {id:?}").contains("secret"));
    assert!(!format!("{ready:?} {id:?}").contains(FIRST));
    store.load(None).unwrap();
    assert_eq!(file_bytes(&layout.database()), before);
}

#[test]
fn corrupt_document_identity_status_and_control_fail_closed_without_writes() {
    for sql in [
        "UPDATE automation_runs SET document='broken-secret'",
        "UPDATE automation_runs SET document='[]'",
        "UPDATE automation_runs SET document='{\"run_id\":\"22222222-2222-4222-8222-222222222222\"}'",
        "UPDATE automation_runs SET status='unknown-secret'",
        "UPDATE automation_runs SET control='unknown-secret'",
        "UPDATE automation_runs SET run_id='11111111111141118111111111111111'",
    ] {
        let (_root, layout, store) = fixture("automation-corrupt");
        store.create(&document(FIRST)).unwrap();
        execute(&layout.database(), sql);
        let before = file_bytes(&layout.database());
        let error = store.load(None).unwrap_err();
        assert!(!format!("{error} {error:?}").contains("secret"));
        assert!(store.save(&document(FIRST), RunStatus::Running).is_err());
        assert_eq!(file_bytes(&layout.database()), before);
    }
}

#[test]
fn update_failure_rolls_back_document_status_and_control() {
    let (_root, layout, store) = fixture("automation-rollback");
    let first = store.create(&document(FIRST)).unwrap();
    execute(
        &layout.database(),
        "CREATE TRIGGER reject_auto BEFORE UPDATE ON automation_runs BEGIN SELECT RAISE(ABORT,'secret-trigger'); END",
    );
    for error in [
        store
            .save(&document(FIRST), RunStatus::Stopped)
            .unwrap_err(),
        store.set_control(first.id(), RunControl::Stop).unwrap_err(),
    ] {
        assert_eq!(error, AutomationStoreError::Database);
        assert!(!format!("{error} {error:?}").contains("secret"));
    }
    assert_eq!(store.load(None).unwrap(), first);
    execute(&layout.database(), "DROP TRIGGER reject_auto");
    store.save(&document(FIRST), RunStatus::Stopped).unwrap();
    store.create(&document(SECOND)).unwrap();
}

#[test]
fn concurrent_create_has_one_winner_and_control_survives_concurrent_save() {
    let (_root, _layout, store) = fixture("automation-concurrent");
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let barrier = &barrier;
        let store = &store;
        let spawn = |id| {
            scope.spawn(move || {
                barrier.wait();
                store.create(&document(id))
            })
        };
        let first = spawn(FIRST);
        let second = spawn(SECOND);
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .all(|error| *error == AutomationStoreError::UnfinishedRun),
        "{results:?}"
    );
    let winner = store.load(None).unwrap();
    std::thread::scope(|scope| {
        let control = scope.spawn(|| store.set_control(winner.id(), RunControl::Stop));
        let save = scope.spawn(|| store.save(winner.document(), RunStatus::Blocked));
        control.join().unwrap().unwrap();
        save.join().unwrap().unwrap();
    });
    let current = store.load(None).unwrap();
    assert_eq!(current.status(), RunStatus::Blocked);
    assert_eq!(current.control(), RunControl::Stop);
}

#[test]
fn readonly_load_rejects_old_schema_foreign_owner_and_copied_marker_namespace() {
    let (_root, layout, store) = fixture("automation-old-schema");
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
    let before = file_bytes(&layout.database());
    assert_eq!(
        store.load(None).unwrap_err(),
        AutomationStoreError::UnsupportedSchema
    );
    assert_eq!(file_bytes(&layout.database()), before);
    layout.initialize().unwrap();
    store.create(&document(FIRST)).unwrap();
    execute(
        &layout.database(),
        "UPDATE meta SET value='python' WHERE key='runtime_owner'",
    );
    let before = file_bytes(&layout.database());
    assert_eq!(
        store.load(None).unwrap_err(),
        AutomationStoreError::StateOwnership
    );
    assert!(
        store
            .set_control(FIRST.parse().unwrap(), RunControl::Stop)
            .is_err()
    );
    assert_eq!(file_bytes(&layout.database()), before);
    write_marker_fields(&layout, "rust", 1, "other");
    assert_eq!(
        store.load(None).unwrap_err(),
        AutomationStoreError::StateOwnership
    );
}

#[test]
fn initial_journal_conversion_waits_for_contending_reader_then_succeeds() {
    let root = TempDir::new("automation-journal-contention");
    let path = root.join("state.sqlite");
    let reader = Connection::open(&path).unwrap();
    reader.execute_batch("CREATE TABLE fixture(value); INSERT INTO fixture VALUES (1); BEGIN; SELECT * FROM fixture;").unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let connection = scope.spawn(|| {
            started_tx.send(()).unwrap();
            let storage = connect(&path);
            finished_tx.send(storage.is_ok()).unwrap();
            storage
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            finished_rx.recv_timeout(std::time::Duration::from_millis(30)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        reader.execute_batch("COMMIT").unwrap();
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        connection.join().unwrap().unwrap();
    });
}
