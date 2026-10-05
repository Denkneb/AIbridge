use super::*;
use crate::recovery::RecoveryClaim;
use std::sync::Barrier;

fn state(
    tag: &str,
    kind: RoundKind,
    status: RoundStatus,
) -> (TempDir, RustStateLayout, TaskId, ProjectId) {
    let root = TempDir::new(tag);
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    let task = query_task_id(1);
    let project = query_project("demo");
    layout
        .open()
        .unwrap()
        .create_task(create_task_input(task, &project, "request"))
        .unwrap();
    layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET status='needs_user',session_id='bound-session'",
            [],
        )
        .unwrap();
    layout.open().unwrap().connection().execute("UPDATE rounds SET kind=?1,status=?2,session_id='bound-session',outbound_message_id='bound-message',attempted=?3,error_code='needs_user'",
        rusqlite::params![kind.as_str(),status.as_str(),status!=RoundStatus::Pending]).unwrap();
    (root, layout, task, project)
}
fn all_rows(storage: &StorageConnection) -> Vec<Vec<Vec<SqlValue>>> {
    ["tasks", "rounds", "events", "active_writers"]
        .iter()
        .map(|t| dump_rows(storage, t))
        .collect()
}
fn claim(storage: &mut StorageConnection, task: TaskId, project: &ProjectId) -> RecoveryClaim {
    storage
        .claim_needs_user_recovery(task, project)
        .unwrap()
        .unwrap()
}
#[test]
fn recovery_claim_resumes_same_round_without_changing_message_session_or_revision() {
    for kind in [RoundKind::Implement, RoundKind::Revise] {
        for status in [
            RoundStatus::NeedsUser,
            RoundStatus::Pending,
            RoundStatus::Sent,
            RoundStatus::Observing,
            RoundStatus::DeliveryUnknown,
        ] {
            let (_root, layout, task, project) = state("recovery-matrix", kind, status);
            let mut storage = layout.open().unwrap();
            let c = claim(&mut storage, task, &project);
            assert_eq!(c.round().round_number, 1);
            assert_eq!(
                c.round().status,
                if status == RoundStatus::NeedsUser {
                    RoundStatus::Observing
                } else {
                    status
                }
            );
            assert_eq!(c.round().attempted, status != RoundStatus::Pending);
            assert_eq!(
                c.round().outbound_message_id.as_deref(),
                Some("bound-message")
            );
            assert_eq!(c.round().session_id.as_deref(), Some("bound-session"));
            let saved = storage.get_task(task).unwrap().unwrap();
            assert_eq!(saved.revision_count, 0);
            assert_eq!(
                saved.status,
                if kind == RoundKind::Implement {
                    TaskStatus::Implementing
                } else {
                    TaskStatus::Revising
                }
            );
            let before = all_rows(&storage);
            assert!(
                storage
                    .claim_needs_user_recovery(task, &project)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(all_rows(&storage), before);
            assert!(storage.release_needs_user_recovery(&c).unwrap());
            let restored = storage
                .connection()
                .query_row("SELECT * FROM rounds", [], |r| {
                    Ok(crate::RoundRow::from_row(r))
                })
                .unwrap()
                .unwrap();
            assert_eq!(restored.status, status);
            assert_eq!(restored.worker_started_at, None);
            assert_eq!(
                storage.get_task(task).unwrap().unwrap().status,
                TaskStatus::NeedsUser
            );
            let before = all_rows(&storage);
            assert!(!storage.release_needs_user_recovery(&c).unwrap());
            assert_eq!(all_rows(&storage), before);
        }
    }
}
#[test]
fn concurrent_claims_have_exactly_one_spawn_winner() {
    let (_root, layout, task, project) = state(
        "recovery-race",
        RoundKind::Implement,
        RoundStatus::NeedsUser,
    );
    let barrier = Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let run = || {
            let mut storage = layout.open().unwrap();
            barrier.wait();
            storage.claim_needs_user_recovery(task, &project).unwrap()
        };
        let a = scope.spawn(run);
        let b = scope.spawn(run);
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(outcomes.iter().filter(|c| c.is_some()).count(), 1);
    assert_eq!(
        layout
            .open()
            .unwrap()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind='needs_user_recovery'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}
#[test]
fn stale_claim_cannot_cancel_worker_start_or_later_claim_even_with_same_timestamp() {
    let (_root, layout, task, project) = state(
        "recovery-stale",
        RoundKind::Implement,
        RoundStatus::NeedsUser,
    );
    let mut storage = layout.open().unwrap();
    let old = claim(&mut storage, task, &project);
    assert!(storage.release_needs_user_recovery(&old).unwrap());
    let new = claim(&mut storage, task, &project);
    // Force identical timestamps: the event fence still distinguishes claimants.
    storage
        .connection()
        .execute("UPDATE rounds SET worker_started_at=?1", [old.lease()])
        .unwrap();
    assert!(!storage.release_needs_user_recovery(&old).unwrap());
    storage
        .connection()
        .execute("UPDATE rounds SET worker_started_at=?1", [new.lease()])
        .unwrap();
    storage
        .mark_worker_started(new.reference().clone(), 30.0)
        .unwrap();
    let before = all_rows(&storage);
    assert!(!storage.release_needs_user_recovery(&new).unwrap());
    assert_eq!(all_rows(&storage), before);
}
#[test]
fn close_terminal_foreign_project_and_completed_round_are_noop_claims() {
    let (_root, layout, task, project) = state(
        "recovery-guards",
        RoundKind::Implement,
        RoundStatus::NeedsUser,
    );
    let mut storage = layout.open().unwrap();
    assert!(
        storage
            .claim_needs_user_recovery(task, &query_project("other"))
            .unwrap()
            .is_none()
    );
    for sql in [
        "UPDATE tasks SET close_requested_at='requested'",
        "UPDATE tasks SET close_requested_at=NULL,status='accepted'",
        "UPDATE tasks SET status='needs_user'; UPDATE rounds SET status='complete'",
    ] {
        storage.connection().execute_batch(sql).unwrap();
        let before = all_rows(&storage);
        assert!(
            storage
                .claim_needs_user_recovery(task, &project)
                .unwrap()
                .is_none()
        );
        assert_eq!(all_rows(&storage), before);
    }
}
#[test]
fn release_does_not_overwrite_terminal_task_close_or_new_round() {
    for sql in [
        "UPDATE tasks SET status='accepted'",
        "UPDATE tasks SET close_requested_at='requested'",
        "INSERT INTO rounds SELECT task_id,project_id,2,'request-2',payload_hash,kind,'pending',NULL,0,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,created_at,updated_at,structured_findings,checkpoint_json FROM rounds WHERE round_number=1",
    ] {
        let (_root, layout, task, project) = state(
            "recovery-release-guards",
            RoundKind::Implement,
            RoundStatus::NeedsUser,
        );
        let mut storage = layout.open().unwrap();
        let c = claim(&mut storage, task, &project);
        storage.connection().execute_batch(sql).unwrap();
        let before = all_rows(&storage);
        assert!(!storage.release_needs_user_recovery(&c).unwrap());
        assert_eq!(all_rows(&storage), before);
    }
}
#[test]
fn claim_and_release_event_failure_rolls_back_every_row_and_redacts_sql() {
    let (_root, layout, task, project) = state(
        "recovery-rollback",
        RoundKind::Implement,
        RoundStatus::NeedsUser,
    );
    let mut storage = layout.open().unwrap();
    let before = all_rows(&storage);
    storage.connection().execute_batch("CREATE TRIGGER reject_claim BEFORE INSERT ON events WHEN NEW.kind='needs_user_recovery' BEGIN SELECT RAISE(ABORT,'secret-trigger'); END;").unwrap();
    let error = storage
        .claim_needs_user_recovery(task, &project)
        .unwrap_err();
    assert!(!format!("{error} {error:?}").contains("secret-trigger"));
    assert_eq!(all_rows(&storage), before);
    storage
        .connection()
        .execute_batch("DROP TRIGGER reject_claim")
        .unwrap();
    let c = claim(&mut storage, task, &project);
    storage.connection().execute_batch("CREATE TRIGGER reject_release BEFORE INSERT ON events WHEN NEW.kind='needs_user_recovery_failed' BEGIN SELECT RAISE(ABORT,'secret-trigger'); END;").unwrap();
    let before = all_rows(&storage);
    assert!(storage.release_needs_user_recovery(&c).is_err());
    assert_eq!(all_rows(&storage), before);
}

#[test]
fn delivery_claim_requires_attempted_identity_and_exact_release_preserves_delivery_error() {
    let (_root, layout, id, project) = state(
        "delivery-recovery",
        RoundKind::Implement,
        RoundStatus::DeliveryUnknown,
    );
    let mut storage = layout.open().unwrap();
    storage
        .connection()
        .execute("UPDATE tasks SET status='delivery_unknown'", [])
        .unwrap();
    storage
        .connection()
        .execute("UPDATE rounds SET error_code='delivery_unknown'", [])
        .unwrap();
    let c = storage
        .claim_delivery_recovery(id, &project)
        .unwrap()
        .unwrap();
    assert_eq!(c.round().status, RoundStatus::Observing);
    assert_eq!(
        c.round().outbound_message_id.as_deref(),
        Some("bound-message")
    );
    assert!(
        storage
            .claim_delivery_recovery(id, &project)
            .unwrap()
            .is_none()
    );
    assert!(storage.release_observation_recovery(&c).unwrap());
    assert!(!storage.release_observation_recovery(&c).unwrap());
    let row = storage
        .connection()
        .query_row("SELECT * FROM rounds", [], |r| Ok(RoundRow::from_row(r)))
        .unwrap()
        .unwrap();
    assert_eq!(row.status, RoundStatus::DeliveryUnknown);
    assert_eq!(row.error_code.as_deref(), Some("delivery_unknown"));
    for sql in [
        "UPDATE rounds SET attempted=0",
        "UPDATE rounds SET attempted=1,session_id=NULL",
        "UPDATE rounds SET session_id='bound-session',outbound_message_id=NULL",
    ] {
        storage.connection().execute(sql, []).unwrap();
        let before = all_rows(&storage);
        assert!(
            storage
                .claim_delivery_recovery(id, &project)
                .unwrap()
                .is_none()
        );
        assert_eq!(all_rows(&storage), before);
    }
}

#[test]
fn failed_claim_only_reopens_assistant_error_and_restores_it_on_failed_spawn() {
    for kind in [RoundKind::Implement, RoundKind::Revise] {
        let (_root, layout, id, project) = state("failed-recovery", kind, RoundStatus::Failed);
        let mut s = layout.open().unwrap();
        s.connection()
            .execute("UPDATE tasks SET status='failed'", [])
            .unwrap();
        for code in [
            "workspace_mismatch",
            "session_not_found",
            "session_directory_mismatch",
            "worker_error",
        ] {
            s.connection()
                .execute("UPDATE rounds SET error_code=?1", [code])
                .unwrap();
            let before = all_rows(&s);
            assert!(s.claim_failed_recovery(id, &project).unwrap().is_none());
            assert_eq!(all_rows(&s), before);
        }
        s.connection()
            .execute("UPDATE rounds SET error_code='assistant_error'", [])
            .unwrap();
        let c = s.claim_failed_recovery(id, &project).unwrap().unwrap();
        assert_eq!(c.round().status, RoundStatus::Observing);
        assert_eq!(c.round().error_code, None);
        assert!(s.claim_failed_recovery(id, &project).unwrap().is_none());
        assert!(s.release_observation_recovery(&c).unwrap());
        assert_eq!(s.get_task(id).unwrap().unwrap().status, TaskStatus::Failed);
        let row = s
            .connection()
            .query_row("SELECT * FROM rounds", [], |r| Ok(RoundRow::from_row(r)))
            .unwrap()
            .unwrap();
        assert_eq!(row.status, RoundStatus::Failed);
        assert_eq!(row.error_code.as_deref(), Some("assistant_error"));
        assert!(row.attempted);
        let c = s.claim_failed_recovery(id, &project).unwrap().unwrap();
        s.mark_worker_started(c.reference().clone(), 30.0).unwrap();
        assert!(!s.release_observation_recovery(&c).unwrap());
    }
}
#[test]
fn recovery_kind_change_cannot_release_a_new_claim_with_the_same_timestamp() {
    let (_root, layout, id, project) = state(
        "cross-recovery",
        RoundKind::Implement,
        RoundStatus::DeliveryUnknown,
    );
    let mut s = layout.open().unwrap();
    s.connection()
        .execute("UPDATE tasks SET status='delivery_unknown'", [])
        .unwrap();
    let old = s.claim_delivery_recovery(id, &project).unwrap().unwrap();
    s.connection().execute_batch("UPDATE tasks SET status='failed'; UPDATE rounds SET status='failed',error_code='assistant_error'").unwrap();
    let current = s.claim_failed_recovery(id, &project).unwrap().unwrap();
    s.connection()
        .execute("UPDATE rounds SET worker_started_at=?1", [old.lease()])
        .unwrap();
    assert!(!s.release_observation_recovery(&old).unwrap());
    s.connection()
        .execute("UPDATE rounds SET worker_started_at=?1", [current.lease()])
        .unwrap();
    assert!(s.release_observation_recovery(&current).unwrap());
}
