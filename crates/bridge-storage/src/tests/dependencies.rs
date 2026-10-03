use super::*;
use crate::DependencyUpdateError;
use std::sync::Barrier;

fn waiting_state(tag: &str) -> (TempDir, RustStateLayout, TaskId, ProjectId) {
    let root = TempDir::new(tag);
    let layout = demo_layout(&root.path);
    layout.initialize().expect("v15 state");
    let task = query_task_id(1);
    let project = query_project("demo");
    let mut storage = layout.open().expect("open");
    storage
        .create_task_with_admission(
            create_task_input(task, &project, "waiting-request"),
            &crate::AdmissionSettings::default(),
            TaskStatus::WaitingDependencies,
        )
        .expect("initial round");
    storage.connection().execute(
        "UPDATE tasks SET status='waiting_dependencies',workflow_id='workflow',depends_on='[[\"other\",\"dependency\"]]',updated_at='before' WHERE task_id=?1",
        [task.to_string()],
    ).expect("waiting fixture");
    drop(storage);
    (root, layout, task, project)
}

fn all_rows(storage: &StorageConnection) -> Vec<Vec<Vec<SqlValue>>> {
    ["tasks", "rounds", "events", "active_writers"]
        .iter()
        .map(|table| dump_rows(storage, table))
        .collect()
}

#[test]
fn activation_is_explicit_pins_baseline_and_writes_one_matching_event() {
    let (_root, layout, task, project) = waiting_state("dependency-explicit");
    layout
        .initialize()
        .expect("reinitialize without activation");
    let mut storage = layout.open().expect("reopen without activation");
    assert_eq!(
        storage.get_task(task).expect("query").expect("task").status,
        TaskStatus::WaitingDependencies
    );
    let rounds = dump_rows(&storage, "rounds");
    let events = count_rows(&storage, "events");
    let baseline = serde_json::json!({"head":"accepted-head","files":{"файл.rs":"hash"}});
    assert!(
        storage
            .refresh_task_baseline(task, &project, &baseline, Some("accepted-head"))
            .expect("refresh")
    );
    assert_eq!(count_rows(&storage, "events"), events);
    assert!(
        storage
            .activate_waiting_dependencies(task, &project)
            .expect("activate")
    );
    let activated = storage.get_task(task).expect("query").expect("task");
    assert_eq!(activated.status, TaskStatus::Implementing);
    assert_eq!(activated.snapshot, Some(baseline));
    assert_eq!(activated.base_head.as_deref(), Some("accepted-head"));
    let event: (i64, String, String, String) = storage.connection().query_row(
        "SELECT round_number,kind,message,created_at FROM events WHERE kind='dependencies_satisfied'", [],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    ).expect("activation event");
    assert_eq!(
        event,
        (
            1,
            "dependencies_satisfied".into(),
            "accepted dependencies unlocked the task".into(),
            activated.updated_at
        )
    );
    assert_eq!(dump_rows(&storage, "rounds"), rounds);
    assert_eq!(
        count_rows(&storage, "active_writers"),
        1,
        "activation reserves its writer slot"
    );
    let before = all_rows(&storage);
    assert!(
        !storage
            .activate_waiting_dependencies(task, &project)
            .expect("repeat")
    );
    assert!(
        !storage
            .refresh_task_baseline(task, &project, &serde_json::json!({"stale":true}), None)
            .expect("fenced refresh")
    );
    assert_eq!(all_rows(&storage), before);
}

#[test]
fn missing_other_project_and_nonwaiting_statuses_are_noops() {
    let (_root, layout, task, project) = waiting_state("dependency-noop");
    let mut storage = layout.open().expect("open");
    for (id, owner) in [
        (query_task_id(99), project.clone()),
        (task, query_project("other")),
    ] {
        let before = all_rows(&storage);
        assert!(
            !storage
                .activate_waiting_dependencies(id, &owner)
                .expect("noop")
        );
        assert!(
            !storage
                .refresh_task_baseline(id, &owner, &serde_json::json!({}), None)
                .expect("noop refresh")
        );
        assert_eq!(all_rows(&storage), before);
    }
    for status in TaskStatus::ALL
        .into_iter()
        .filter(|status| *status != TaskStatus::WaitingDependencies)
    {
        storage
            .connection()
            .execute(
                "UPDATE tasks SET status=?1 WHERE task_id=?2",
                rusqlite::params![status.as_str(), task.to_string()],
            )
            .expect("fixture status");
        let before = all_rows(&storage);
        assert!(
            !storage
                .activate_waiting_dependencies(task, &project)
                .expect("status noop")
        );
        assert!(
            !storage
                .refresh_task_baseline(task, &project, &serde_json::json!({}), None)
                .expect("status refresh noop")
        );
        assert_eq!(all_rows(&storage), before);
    }
}

#[test]
fn other_writer_statuses_and_stale_ledger_block_activation() {
    for status in [
        "implementing",
        "awaiting_review",
        "revising",
        "needs_user",
        "failed",
        "delivery_unknown",
    ] {
        let (_root, layout, task, project) = waiting_state("dependency-busy-task");
        seed_task_raw(
            &layout.database(),
            &query_task_id(2).to_string(),
            "demo",
            status,
        );
        let mut storage = layout.open().expect("open");
        let before = all_rows(&storage);
        assert!(
            !storage
                .activate_waiting_dependencies(task, &project)
                .expect("busy")
        );
        assert_eq!(all_rows(&storage), before);
    }
    let (_root, layout, task, project) = waiting_state("dependency-busy-ledger");
    let mut storage = layout.open().expect("open");
    storage
        .connection()
        .execute_batch(
            "INSERT INTO active_writers VALUES ('stale','demo','[\"module.py\"]','before',0)",
        )
        .expect("stale writer");
    let before = all_rows(&storage);
    assert!(
        !storage
            .activate_waiting_dependencies(task, &project)
            .expect("ledger busy")
    );
    assert_eq!(all_rows(&storage), before);
}

#[test]
fn own_reservation_and_other_project_do_not_block_activation() {
    let (_root, layout, task, project) = waiting_state("dependency-own-ledger");
    seed_task_raw(
        &layout.database(),
        &query_task_id(2).to_string(),
        "other",
        "implementing",
    );
    let mut storage = layout.open().expect("open");
    storage
        .connection()
        .execute(
            "INSERT INTO active_writers VALUES (?1,'demo','[\"module.py\"]','original-time',0)",
            [task.to_string()],
        )
        .expect("own crash-window reservation");
    storage
        .connection()
        .execute_batch(
            "INSERT INTO active_writers VALUES ('other','other','[\"module.py\"]','before',0)",
        )
        .expect("other project ledger");
    let ledger = dump_rows(&storage, "active_writers");
    assert!(
        storage
            .activate_waiting_dependencies(task, &project)
            .expect("activate")
    );
    assert_eq!(dump_rows(&storage, "active_writers"), ledger);
}

#[test]
fn concurrent_activation_has_one_winner_and_one_event() {
    let (_root, layout, task, project) = waiting_state("dependency-race");
    let barrier = Barrier::new(8);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    let mut storage = layout.open().expect("open racer");
                    barrier.wait();
                    storage
                        .activate_waiting_dependencies(task, &project)
                        .expect("race activation")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|won| **won).count(), 1);
    let storage = layout.open().expect("open result");
    assert_eq!(count_rows(&storage, "events"), 2);
    assert_eq!(count_rows(&storage, "rounds"), 1);
}

#[test]
fn concurrent_ready_tasks_keep_single_writer_bound() {
    let (_root, layout, first, project) = waiting_state("dependency-competing");
    let second = query_task_id(2);
    seed_task_raw(
        &layout.database(),
        &second.to_string(),
        "demo",
        "waiting_dependencies",
    );
    let barrier = Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|id| {
                let barrier = &barrier;
                let layout = &layout;
                let project = &project;
                scope.spawn(move || {
                    let mut storage = layout.open().expect("open");
                    barrier.wait();
                    storage
                        .activate_waiting_dependencies(id, project)
                        .expect("activate")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|won| **won).count(), 1);
    let storage = layout.open().expect("open");
    let statuses =
        [first, second].map(|id| storage.get_task(id).expect("query").expect("task").status);
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == TaskStatus::Implementing)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == TaskStatus::WaitingDependencies)
            .count(),
        1
    );
}

#[test]
fn baseline_refresh_racing_activation_cannot_overwrite_after_transition() {
    let (_root, layout, task, project) = waiting_state("dependency-baseline-race");
    let before = layout
        .open()
        .expect("open")
        .get_task(task)
        .expect("query")
        .expect("task");
    let fresh = serde_json::json!({"new":"baseline"});
    let barrier = Barrier::new(2);
    let refreshed = std::thread::scope(|scope| {
        let refresh = scope.spawn(|| {
            let mut storage = layout.open().expect("open refresh");
            barrier.wait();
            storage
                .refresh_task_baseline(task, &project, &fresh, None)
                .expect("refresh")
        });
        let activate = scope.spawn(|| {
            let mut storage = layout.open().expect("open activate");
            barrier.wait();
            storage
                .activate_waiting_dependencies(task, &project)
                .expect("activate")
        });
        assert!(activate.join().expect("activation thread"));
        refresh.join().expect("refresh thread")
    });
    let storage = layout.open().expect("open result");
    let result = storage.get_task(task).expect("query").expect("task");
    assert_eq!(result.status, TaskStatus::Implementing);
    assert_eq!(
        result.snapshot,
        if refreshed {
            Some(fresh)
        } else {
            before.snapshot
        }
    );
    assert_eq!(
        result.base_head,
        if refreshed { None } else { before.base_head }
    );
}

#[test]
fn event_and_baseline_write_failures_roll_back_without_disclosing_trigger_text() {
    for operation in ["event", "baseline"] {
        let (_root, layout, task, project) = waiting_state("dependency-rollback");
        let mut storage = layout.open().expect("open");
        let trigger = if operation == "event" {
            "CREATE TRIGGER block_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'secret-trigger-content'); END"
        } else {
            "CREATE TRIGGER block_baseline BEFORE UPDATE OF snapshot ON tasks BEGIN SELECT RAISE(ABORT,'secret-trigger-content'); END"
        };
        storage
            .connection()
            .execute_batch(trigger)
            .expect("trigger");
        let before = all_rows(&storage);
        let error = if operation == "event" {
            storage
                .activate_waiting_dependencies(task, &project)
                .expect_err("event failure")
        } else {
            storage
                .refresh_task_baseline(
                    task,
                    &project,
                    &serde_json::json!({"private":"secret-input"}),
                    Some("secret-head"),
                )
                .expect_err("baseline failure")
        };
        assert!(matches!(error, DependencyUpdateError::Database(_)));
        assert!(error.source().is_some());
        for secret in [
            "secret-trigger-content",
            "secret-input",
            "secret-head",
            &task.to_string(),
        ] {
            assert!(!format!("{error} {error:?}").contains(secret));
        }
        assert_eq!(all_rows(&storage), before);
        storage
            .connection()
            .execute_batch(if operation == "event" {
                "DROP TRIGGER block_event"
            } else {
                "DROP TRIGGER block_baseline"
            })
            .expect("remove trigger");
        assert!(
            storage
                .activate_waiting_dependencies(task, &project)
                .expect("retry")
        );
    }
}

#[test]
fn invalid_snapshots_and_corrupt_waiting_rows_fail_without_writes() {
    let (_root, layout, task, project) = waiting_state("dependency-invalid");
    let mut storage = layout.open().expect("open");
    let before = all_rows(&storage);
    for snapshot in [
        serde_json::json!(null),
        serde_json::json!([]),
        serde_json::json!("secret"),
        serde_json::json!(42),
        serde_json::json!(true),
    ] {
        let error = storage
            .refresh_task_baseline(task, &project, &snapshot, None)
            .expect_err("invalid snapshot");
        assert!(matches!(error, DependencyUpdateError::InvalidSnapshot));
        assert_eq!(all_rows(&storage), before);
    }
    storage
        .connection()
        .execute_batch("UPDATE tasks SET allowed_paths='[42]'")
        .expect("corrupt scope shape");
    let before = all_rows(&storage);
    assert!(matches!(
        storage.activate_waiting_dependencies(task, &project),
        Err(DependencyUpdateError::TaskRow(_))
    ));
    assert!(
        !storage
            .activate_waiting_dependencies(task, &query_project("other"))
            .expect("foreign noop")
    );
    assert_eq!(all_rows(&storage), before);
}
