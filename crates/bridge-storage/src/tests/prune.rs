use super::*;
use crate::prune;
#[test]
fn history_prune_rechecks_status_and_worktrees_and_rolls_back() {
    let root = TempDir::new("history-prune");
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    let project = query_project("demo");
    let mut s = layout.open().unwrap();
    for n in 1..=5 {
        let task = query_task_id(n);
        s.create_task(create_task_input(task, &project, &format!("prune-{n}")))
            .unwrap();
        s.connection().execute("UPDATE tasks SET status=?1, updated_at='2000-01-01T00:00:00.000+00:00' WHERE task_id=?2",rusqlite::params!["closed",task.to_string()]).unwrap();
        s.connection()
            .execute(
                "DELETE FROM active_writers WHERE task_id=?1",
                [task.to_string()],
            )
            .unwrap();
    }
    for n in 1..=5 {
        s.connection()
            .execute(
                "UPDATE tasks SET status=?1 WHERE task_id=?2",
                rusqlite::params![
                    match n {
                        1 | 2 => "accepted",
                        3 => "failed",
                        4 => "closed",
                        _ => "implementing",
                    },
                    query_task_id(n).to_string()
                ],
            )
            .unwrap();
    }
    s.connection()
        .execute(
            "UPDATE tasks SET execution_mode='worktree' WHERE task_id=?1",
            [query_task_id(2).to_string()],
        )
        .unwrap();
    s.register_worktree(query_task_id(2), &project, "retained", &Default::default())
        .unwrap();
    let cutoff = "2020-01-01T00:00:00.000+00:00";
    assert_eq!(
        s.prune_candidates(&project, cutoff, false).unwrap().len(),
        2
    );
    // Status changed after preview: apply must re-select.
    s.connection()
        .execute(
            "UPDATE tasks SET status='implementing' WHERE task_id=?1",
            [query_task_id(4).to_string()],
        )
        .unwrap();
    s.connection().execute_batch("CREATE TRIGGER reject_prune BEFORE DELETE ON tasks BEGIN SELECT RAISE(ABORT,'blocked'); END;").unwrap();
    assert!(s.prune_history(&project, cutoff, true).is_err());
    assert_eq!(count_rows(&s, "tasks"), 5);
    assert_eq!(count_rows(&s, "rounds"), 5);
    s.connection()
        .execute_batch("DROP TRIGGER reject_prune;")
        .unwrap();
    let rows = s.prune_history(&project, cutoff, true).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(count_rows(&s, "tasks"), 3);
    assert_eq!(count_rows(&s, "rounds"), 3);
    assert_eq!(count_rows(&s, "events"), 3);
    assert!(s.get_task(query_task_id(2)).unwrap().is_some());
    assert!(s.get_task(query_task_id(4)).unwrap().is_some());
    assert!(s.get_task(query_task_id(5)).unwrap().is_some());
}
#[test]
fn history_missing_state_dry_run_creates_nothing() {
    let root = TempDir::new("prune-missing");
    let layout = demo_layout(&root.path);
    assert!(
        prune::run(&layout, 86400, false, false, false)
            .unwrap()
            .is_empty()
    );
    assert!(!layout.project_dir().exists());
}
