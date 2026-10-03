use super::*;
use crate::*;
use bridge_domain::{ExecutionMode, WorkflowId};
use std::num::NonZeroU16;

fn state(tag: &str) -> (TempDir, RustStateLayout, TaskId, ProjectId) {
    let root = TempDir::new(tag);
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    let task = query_task_id(1);
    let project = query_project("demo");
    let mut storage = layout.open().unwrap();
    storage
        .create_task_with_admission(
            create_task_input(task, &project, "worktree-request"),
            &AdmissionSettings::new(1, false, ExecutionMode::Worktree).unwrap(),
            TaskStatus::Implementing,
        )
        .unwrap();
    (root, layout, task, project)
}
fn id(s: &str) -> WorkflowId {
    WorkflowId::try_from(s.to_owned()).unwrap()
}
#[test]
fn transition_maps_are_exhaustive_and_unknown_values_fail_closed() {
    for &a in WorktreeStatus::ALL {
        for &b in WorktreeStatus::ALL {
            let pair = (a.as_str(), b.as_str());
            assert_eq!(
                a.allows(b),
                a == b
                    || matches!(
                        pair,
                        ("pending", "creating" | "error" | "removed")
                            | ("creating", "created" | "error" | "removing")
                            | ("created", "removing")
                            | ("removing", "removed")
                    )
            );
        }
    }
    for &a in WorktreeDeliveryState::ALL {
        for &b in WorktreeDeliveryState::ALL {
            assert_eq!(
                a.allows(b),
                a == b
                    || matches!(
                        (a.as_str(), b.as_str()),
                        ("none", "applying") | ("applying", "delivered")
                    )
            );
        }
    }
    for &a in WorktreeQuarantineStatus::ALL {
        for &b in WorktreeQuarantineStatus::ALL {
            assert_eq!(
                a.allows(b),
                a == b
                    || matches!(
                        (a.as_str(), b.as_str()),
                        ("quarantined", "moved" | "removed") | ("moved", "removed")
                    )
            );
        }
    }
    assert!("unknown".parse::<WorktreeStatus>().is_err());
    assert!("".parse::<WorktreeDeliveryState>().is_err());
    assert!("unknown".parse::<WorktreeQuarantineStatus>().is_err());
}
#[test]
fn registration_scopes_owner_and_preserves_all_metadata_without_filesystem_work() {
    let (root, layout, task, project) = state("worktree-register");
    let mut s = layout.open().unwrap();
    let path = root.join("never-created");
    let input = WorktreeRegistration {
        runtime_dir: Some("runtime".into()),
        base_head: Some("base".into()),
        baseline_json: Some("opaque\n baseline".into()),
        server_endpoint: Some("endpoint".into()),
        server_port: NonZeroU16::new(65535),
        server_process_record: Some("process".into()),
        cleanup_reason: Some("reason".into()),
        ..Default::default()
    };
    let events = count_rows(&s, "events");
    let r = s
        .register_worktree(task, &project, path.to_str().unwrap(), &input)
        .unwrap();
    assert_eq!(r.status, WorktreeStatus::Pending);
    assert_eq!(r.delivery_state, Some(WorktreeDeliveryState::None));
    assert_eq!(r.server_port, input.server_port);
    assert_eq!(r.created_at, r.updated_at);
    assert_eq!(r.baseline_json, input.baseline_json);
    assert_eq!(r.runtime_dir, input.runtime_dir);
    assert_eq!(r.base_head, input.base_head);
    assert_eq!(r.server_endpoint, input.server_endpoint);
    assert_eq!(r.server_process_record, input.server_process_record);
    assert_eq!(r.cleanup_reason, input.cleanup_reason);
    assert_eq!(r.removed_at, None);
    assert_eq!(r.delivery_journal_path, None);
    assert_eq!(r.delivered_at, None);
    assert!(!path.exists());
    assert_eq!(count_rows(&s, "events"), events);
    assert!(
        s.register_worktree(task, &project, "duplicate", &input)
            .is_err()
    );
    assert!(s.get_worktree(task, &query_project("foreign")).is_err());
    s.connection()
        .execute("UPDATE tasks SET execution_mode='direct'", [])
        .unwrap();
    assert!(
        s.update_worktree_status(task, &project, WorktreeStatus::Creating, None)
            .is_err()
    );
    assert!(
        s.register_worktree(task, &project, "direct", &input)
            .is_err()
    );
    assert!(
        s.register_worktree(query_task_id(2), &project, "missing", &input)
            .is_err()
    );
}
#[test]
fn lifecycle_is_atomic_and_repeats_leave_metadata_untouched() {
    let (_root, layout, t, p) = state("worktree-life");
    let mut s = layout.open().unwrap();
    s.register_worktree(t, &p, "opaque", &Default::default())
        .unwrap();
    let a = s
        .update_worktree_status(t, &p, WorktreeStatus::Creating, Some("start"))
        .unwrap();
    assert_eq!(
        s.update_worktree_status(t, &p, WorktreeStatus::Creating, Some("ignored"))
            .unwrap(),
        a
    );
    let before = dump_rows(&s, "events");
    assert!(
        s.update_worktree_status(t, &p, WorktreeStatus::Removed, None)
            .is_err()
    );
    assert_eq!(dump_rows(&s, "events"), before);
    s.update_worktree_status(t, &p, WorktreeStatus::Created, None)
        .unwrap();
    s.update_worktree_status(t, &p, WorktreeStatus::Removing, None)
        .unwrap();
    let r = s
        .update_worktree_status(t, &p, WorktreeStatus::Removed, Some(""))
        .unwrap();
    assert_eq!(r.removed_at, r.updated_at);
    assert_eq!(r.cleanup_reason.as_deref(), Some(""));
    let event: (Option<i64>, String, String) = s
        .connection()
        .query_row(
            "SELECT round_number,message,created_at FROM events WHERE kind='worktree_removed'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        event,
        (
            None,
            "worktree status: removing -> removed".into(),
            r.updated_at.unwrap()
        )
    );
}
#[test]
fn delivery_fallback_metadata_and_single_completion_event() {
    let (_root, layout, t, p) = state("worktree-delivery");
    let mut s = layout.open().unwrap();
    s.register_worktree(t, &p, "opaque", &Default::default())
        .unwrap();
    for legacy in [None, Some("")] {
        s.connection()
            .execute("UPDATE worktrees SET delivery_state=?1", [legacy])
            .unwrap();
        assert_eq!(s.get_worktree(t, &p).unwrap().unwrap().delivery_state, None);
    }
    assert!(
        s.set_worktree_delivery(t, &p, WorktreeDeliveryState::Delivered, None, None)
            .is_err()
    );
    s.set_worktree_delivery(
        t,
        &p,
        WorktreeDeliveryState::Applying,
        Some("journal"),
        Some("provided"),
    )
    .unwrap();
    let delivered = s
        .set_worktree_delivery(t, &p, WorktreeDeliveryState::Delivered, None, None)
        .unwrap();
    assert_eq!(delivered.delivery_journal_path.as_deref(), Some("journal"));
    assert_eq!(delivered.delivered_at, delivered.updated_at);
    assert_eq!(
        s.set_worktree_delivery(
            t,
            &p,
            WorktreeDeliveryState::Delivered,
            Some("ignored"),
            Some("ignored")
        )
        .unwrap(),
        delivered
    );
    let n: i64 = s
        .connection()
        .query_row(
            "SELECT count(*) FROM events WHERE kind='delivered'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}
#[test]
fn baseline_is_opaque_and_server_none_clears_process_record() {
    let (_root, layout, t, p) = state("worktree-metadata");
    let mut s = layout.open().unwrap();
    s.register_worktree(t, &p, "opaque", &Default::default())
        .unwrap();
    let a = s
        .update_worktree_baseline(t, &p, "not JSON\n", Some("head"))
        .unwrap();
    assert_eq!(a.baseline_json.as_deref(), Some("not JSON\n"));
    let b = s.update_worktree_baseline(t, &p, "new", None).unwrap();
    assert_eq!(b.base_head, a.base_head);
    let port = NonZeroU16::new(1).unwrap();
    s.update_worktree_server(t, &p, "server", port, Some("process"))
        .unwrap();
    let r = s
        .update_worktree_server(t, &p, "server", port, None)
        .unwrap();
    assert_eq!(r.server_process_record, None);
    assert!(s.update_worktree_server(t, &p, "", port, None).is_err());
    assert!(s.update_worktree_baseline(t, &p, "", None).is_err());
    let n: i64 = s
        .connection()
        .query_row(
            "SELECT count(*) FROM events WHERE kind='worktree_server_started'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2);
}
#[test]
fn corrupt_rows_cannot_be_mutated_even_with_identical_target() {
    let (_root, layout, t, p) = state("worktree-corrupt");
    let mut s = layout.open().unwrap();
    s.register_worktree(t, &p, "opaque", &Default::default())
        .unwrap();
    for sql in [
        "UPDATE worktrees SET status='secret-invalid'",
        "UPDATE worktrees SET status='pending',delivery_state='secret-invalid'",
        "UPDATE worktrees SET delivery_state='none',server_port=0",
        "UPDATE worktrees SET server_port=65536",
    ] {
        s.connection().execute(sql, []).unwrap();
        let before = dump_rows(&s, "worktrees");
        let error = s
            .update_worktree_status(t, &p, WorktreeStatus::Pending, None)
            .unwrap_err();
        assert!(!format!("{error:?}").contains("secret"));
        assert_eq!(dump_rows(&s, "worktrees"), before);
    }
}
#[test]
fn event_failure_rolls_back_metadata_and_redacts_database_error() {
    let (_root, layout, t, p) = state("worktree-rollback");
    let mut s = layout.open().unwrap();
    s.register_worktree(t, &p, "opaque", &Default::default())
        .unwrap();
    let before = dump_rows(&s, "worktrees");
    s.connection().execute_batch("CREATE TRIGGER reject_registry_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'secret-token'); END;").unwrap();
    for error in [
        s.update_worktree_status(t, &p, WorktreeStatus::Creating, None)
            .unwrap_err(),
        s.set_worktree_delivery(t, &p, WorktreeDeliveryState::Applying, None, None)
            .and_then(|_| {
                s.set_worktree_delivery(t, &p, WorktreeDeliveryState::Delivered, None, None)
            })
            .unwrap_err(),
    ] {
        assert!(!format!("{error:?}: {error}").contains("secret-token"));
    }
    // Applying intentionally has no event, and survives the later failed delivered transaction.
    let r = s.get_worktree(t, &p).unwrap().unwrap();
    assert_eq!(r.status, WorktreeStatus::Pending);
    assert_eq!(r.delivery_state, Some(WorktreeDeliveryState::Applying));
    assert_ne!(dump_rows(&s, "worktrees"), before);
}
#[test]
fn quarantine_drift_and_transition_rules_preserve_registry_on_failure() {
    let (_root, layout, _, _) = state("quarantine-life");
    let mut s = layout.open().unwrap();
    let key = id("entry");
    let initial = s
        .register_worktree_quarantine(&key, "original", &Default::default())
        .unwrap();
    assert!(initial.found_at.is_some());
    assert!(
        s.register_worktree_quarantine(&key, "duplicate", &Default::default())
            .is_err()
    );
    assert!(matches!(
        s.transition_worktree_quarantine(
            &key,
            WorktreeQuarantineStatus::Moved,
            Some("moved"),
            Some(WorktreeQuarantineStatus::Moved),
            None
        ),
        Err(WorktreeStorageError::Drift)
    ));
    assert!(matches!(
        s.transition_worktree_quarantine(
            &key,
            WorktreeQuarantineStatus::Moved,
            None,
            None,
            Some("changed")
        ),
        Err(WorktreeStorageError::Drift)
    ));
    assert_eq!(s.get_worktree_quarantine(&key).unwrap(), Some(initial));
    s.transition_worktree_quarantine(
        &key,
        WorktreeQuarantineStatus::Moved,
        Some("moved"),
        Some(WorktreeQuarantineStatus::Quarantined),
        Some("original"),
    )
    .unwrap();
    let repeat = s
        .update_worktree_quarantine_status(&key, WorktreeQuarantineStatus::Moved, Some(""))
        .unwrap();
    assert_eq!(repeat.quarantined_path.as_deref(), Some(""));
    s.update_worktree_quarantine_status(&key, WorktreeQuarantineStatus::Removed, None)
        .unwrap();
    assert!(
        s.update_worktree_quarantine_status(&key, WorktreeQuarantineStatus::Moved, None)
            .is_err()
    );
}
#[test]
fn readonly_missing_and_legacy_state_never_initialize_or_change_journal() {
    let root = TempDir::new("quarantine-readonly");
    let missing = root.join("missing/state.sqlite");
    assert!(
        list_worktree_quarantine_readonly(&missing)
            .unwrap()
            .is_empty()
    );
    assert!(!has_worktree_quarantine_table(&missing).unwrap());
    assert!(!root.join("missing").exists());
    let path = root.join("legacy.sqlite");
    initialize(&path).unwrap();
    let before = file_bytes(&path);
    assert!(!has_worktree_quarantine_table(&path).unwrap());
    assert!(list_worktree_quarantine_readonly(&path).unwrap().is_empty());
    assert_eq!(file_bytes(&path), before);
    assert!(
        read_worktree_readonly_strict(&path, query_task_id(1), &query_project("demo")).is_err()
    );
    let c = Connection::open(&path).unwrap();
    c.pragma_update(None, "user_version", 999).unwrap();
    drop(c);
    assert!(has_worktree_quarantine_table(&path).is_err());
}
#[test]
fn readonly_live_wal_listing_is_ordered_and_missing_table_is_empty() {
    let (_root, layout, _, _) = state("quarantine-wal");
    let mut s = layout.open().unwrap();
    for (key, found) in [("z", "2"), ("b", "1"), ("a", "1")] {
        s.register_worktree_quarantine(
            &id(key),
            "opaque",
            &WorktreeQuarantineRegistration {
                found_at: Some(found.into()),
                ..Default::default()
            },
        )
        .unwrap();
    }
    // Retrieve the actual database path through SQLite, leaving the writer open.
    let path: String = s
        .connection()
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let path = Path::new(&path);
    let before = dump_rows(&s, "worktree_quarantine");
    let journal: String = s
        .connection()
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    let listed = list_worktree_quarantine_readonly(path).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|r| r.entry_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "z"]
    );
    assert!(has_worktree_quarantine_table(path).unwrap());
    assert_eq!(dump_rows(&s, "worktree_quarantine"), before);
    assert_eq!(
        s.connection()
            .pragma_query_value::<String, _>(None, "journal_mode", |r| r.get(0))
            .unwrap(),
        journal
    );
    s.connection()
        .execute("DROP TABLE worktree_quarantine", [])
        .unwrap();
    assert!(!has_worktree_quarantine_table(path).unwrap());
    assert!(list_worktree_quarantine_readonly(path).unwrap().is_empty());
}

#[test]
fn persisted_transition_matrices_match_allowed_maps() {
    let (_root, layout, t, p) = state("registry-matrices");
    let mut s = layout.open().unwrap();
    s.register_worktree(t, &p, "opaque", &Default::default())
        .unwrap();
    let key = id("matrix");
    s.register_worktree_quarantine(&key, "original", &Default::default())
        .unwrap();
    for &a in WorktreeStatus::ALL {
        for &b in WorktreeStatus::ALL {
            s.connection()
                .execute("UPDATE worktrees SET status=?1", [a.as_str()])
                .unwrap();
            let before = dump_rows(&s, "worktrees");
            let result = s.update_worktree_status(t, &p, b, None);
            assert_eq!(result.is_ok(), a.allows(b));
            if !a.allows(b) || a == b {
                assert_eq!(dump_rows(&s, "worktrees"), before);
            }
        }
    }
    for &a in WorktreeDeliveryState::ALL {
        for &b in WorktreeDeliveryState::ALL {
            s.connection()
                .execute("UPDATE worktrees SET delivery_state=?1", [a.as_str()])
                .unwrap();
            let before = dump_rows(&s, "worktrees");
            assert_eq!(
                s.set_worktree_delivery(t, &p, b, None, None).is_ok(),
                a.allows(b)
            );
            if !a.allows(b) || a == b {
                assert_eq!(dump_rows(&s, "worktrees"), before);
            }
        }
    }
    for &a in WorktreeQuarantineStatus::ALL {
        for &b in WorktreeQuarantineStatus::ALL {
            s.connection()
                .execute("UPDATE worktree_quarantine SET status=?1", [a.as_str()])
                .unwrap();
            let before = dump_rows(&s, "worktree_quarantine");
            assert_eq!(
                s.update_worktree_quarantine_status(&key, b, None).is_ok(),
                a.allows(b)
            );
            if !a.allows(b) || a == b {
                assert_eq!(dump_rows(&s, "worktree_quarantine"), before);
            }
        }
    }
}
#[test]
fn quarantine_failure_rolls_back_and_corruption_is_not_absence() {
    let (_root, layout, _, _) = state("quarantine-failure");
    let mut s = layout.open().unwrap();
    let key = id("entry");
    s.register_worktree_quarantine(&key, "original", &Default::default())
        .unwrap();
    let before = dump_rows(&s, "worktree_quarantine");
    s.connection().execute_batch("CREATE TRIGGER reject_quarantine BEFORE UPDATE ON worktree_quarantine BEGIN SELECT RAISE(ABORT,'secret-path'); END;").unwrap();
    let error = s
        .update_worktree_quarantine_status(&key, WorktreeQuarantineStatus::Moved, Some("new"))
        .unwrap_err();
    assert!(!format!("{error:?}").contains("secret-path"));
    assert_eq!(dump_rows(&s, "worktree_quarantine"), before);
    s.connection()
        .execute_batch(
            "DROP TRIGGER reject_quarantine; UPDATE worktree_quarantine SET status='corrupt';",
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
    assert!(list_worktree_quarantine_readonly(Path::new(&path)).is_err());
    assert!(
        s.update_worktree_quarantine_status(&key, WorktreeQuarantineStatus::Quarantined, None)
            .is_err()
    );
    s.connection()
        .execute("UPDATE meta SET value='14' WHERE key='schema_version'", [])
        .unwrap();
    assert!(has_worktree_quarantine_table(Path::new(&path)).is_err());
}
#[test]
fn competing_quarantine_transitions_detect_drift_under_write_lock() {
    let (_root, layout, _, _) = state("quarantine-race");
    let mut s = layout.open().unwrap();
    let key = id("entry");
    s.register_worktree_quarantine(&key, "original", &Default::default())
        .unwrap();
    drop(s);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads = [
        WorktreeQuarantineStatus::Moved,
        WorktreeQuarantineStatus::Removed,
    ]
    .map(|target| {
        let layout = layout.clone();
        let barrier = barrier.clone();
        let key = key.clone();
        std::thread::spawn(move || {
            let mut s = layout.open().unwrap();
            barrier.wait();
            s.transition_worktree_quarantine(
                &key,
                target,
                None,
                Some(WorktreeQuarantineStatus::Quarantined),
                Some("original"),
            )
        })
    });
    let results = threads.map(|t| t.join().unwrap());
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(WorktreeStorageError::Drift)))
            .count(),
        1
    );
}
