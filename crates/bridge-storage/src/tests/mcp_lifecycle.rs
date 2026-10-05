use super::*;
fn state(review: bool) -> (TempDir, RustStateLayout, TaskId, ProjectId) {
    let root = TempDir::new("mcp-lifecycle");
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    let task = query_task_id(1);
    let project = query_project("demo");
    let mut s = layout.open().unwrap();
    s.create_task(create_task_input(task, &project, "submit"))
        .unwrap();
    if review {
        let reference = RoundRef {
            task_id: task,
            project_id: project.clone(),
            round_number: 1,
        };
        s.prepare_round(reference.clone(), "outbound".into())
            .unwrap();
        s.mark_round_sent(reference.clone()).unwrap();
        s.mark_round_observing(reference.clone()).unwrap();
        s.finish_round(FinishRoundInput {
            round: reference,
            round_status: RoundStatus::Complete,
            task_status: TaskStatus::AwaitingReview,
            response_message_id: None,
            response: None,
            error_code: None,
            result_json: None,
        })
        .unwrap();
    }
    (root, layout, task, project)
}
#[test]
fn manual_accept_is_atomic_idempotent_and_releases_reservation() {
    let (_root, layout, task, project) = state(true);
    let mut s = layout.open().unwrap();
    assert_eq!(
        s.accept_manual_task(task, &project).unwrap().status,
        TaskStatus::Accepted
    );
    assert_eq!(
        s.accept_manual_task(task, &project).unwrap().status,
        TaskStatus::Accepted
    );
    assert!(s.get_active_writers(&project).unwrap().is_empty());
    let count: i64 = s
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind='accepted'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let (_root, layout, task, project) = state(true);
    let mut s = layout.open().unwrap();
    s.connection().execute_batch("CREATE TRIGGER reject_accept BEFORE INSERT ON events WHEN NEW.kind='accepted' BEGIN SELECT RAISE(ABORT,'blocked'); END;").unwrap();
    assert!(s.accept_manual_task(task, &project).is_err());
    assert_eq!(
        s.get_task(task).unwrap().unwrap().status,
        TaskStatus::AwaitingReview
    );
}
#[test]
fn accept_refuses_close_on_accept_policy_and_unfinished_round() {
    for variant in 0..3 {
        let (_root, layout, task, project) = state(variant != 2);
        let mut s = layout.open().unwrap();
        match variant {
            0 => {
                s.request_task_close(task, "close").unwrap();
            }
            1 => {
                s.connection()
                    .execute("UPDATE tasks SET delivery_mode='on_accept'", [])
                    .unwrap();
            }
            _ => {}
        }
        assert!(s.accept_manual_task(task, &project).is_err());
        assert_ne!(
            s.get_task(task).unwrap().unwrap().status,
            TaskStatus::Accepted
        );
    }
}
#[test]
fn spawn_lease_release_cannot_cancel_started_child_or_close() {
    let (_root, layout, task, project) = state(false);
    let mut s = layout.open().unwrap();
    let reference = RoundRef {
        task_id: task,
        project_id: project,
        round_number: 1,
    };
    let claim = s
        .mark_worker_started(reference.clone(), 60.0)
        .unwrap()
        .round
        .worker_started_at
        .unwrap();
    assert!(!s.release_worker_spawn(&reference, "wrong lease").unwrap());
    assert!(s.release_worker_spawn(&reference, &claim).unwrap());
    let claim = s
        .mark_worker_started(reference.clone(), 60.0)
        .unwrap()
        .round
        .worker_started_at
        .unwrap();
    s.mark_worker_started(reference.clone(), 60.0).unwrap();
    assert!(!s.release_worker_spawn(&reference, &claim).unwrap());
    let claim = s
        .mark_worker_started(reference.clone(), 60.0)
        .unwrap()
        .round
        .worker_started_at
        .unwrap();
    s.request_task_close(task, "close").unwrap();
    assert!(!s.release_worker_spawn(&reference, &claim).unwrap());
}
#[test]
fn revision_limit_parks_once_and_event_failure_rolls_back() {
    let (_root, layout, task, project) = state(true);
    let mut s = layout.open().unwrap();
    s.connection().execute_batch("CREATE TRIGGER reject_park BEFORE INSERT ON events WHEN NEW.message='revision limit reached' BEGIN SELECT RAISE(ABORT,'blocked'); END;").unwrap();
    assert!(s.park_revision_limit(task, &project).is_err());
    assert_eq!(
        s.get_task(task).unwrap().unwrap().status,
        TaskStatus::AwaitingReview
    );
    s.connection()
        .execute_batch("DROP TRIGGER reject_park")
        .unwrap();
    assert_eq!(
        s.park_revision_limit(task, &project).unwrap().status,
        TaskStatus::NeedsUser
    );
    assert!(s.park_revision_limit(task, &project).is_err());
    assert_eq!(
        s.connection()
            .query_row("SELECT COUNT(*) FROM rounds", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        s.connection()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE message='revision limit reached'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}
