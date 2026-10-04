use super::*;
use crate::{AdmissionSettings, DependencyUpdateError, WriterError};
use bridge_domain::ExecutionMode;
use std::{os::unix::fs::symlink, sync::Barrier};

fn state(tag: &str) -> (TempDir, RustStateLayout, PathBuf, ProjectId) {
    let root = TempDir::new(tag);
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("src")).expect("workspace");
    std::fs::create_dir_all(workspace.join("src-other")).expect("sibling directory");
    symlink("src", workspace.join("alias")).expect("alias");
    let layout = demo_layout(&root.path);
    layout.initialize().expect("v15");
    (root, layout, workspace, query_project("demo"))
}
fn settings(max: u64, parallel: bool) -> AdmissionSettings {
    AdmissionSettings::new(
        max,
        parallel,
        if parallel {
            ExecutionMode::Worktree
        } else {
            ExecutionMode::Direct
        },
    )
    .expect("settings")
}
fn input(id: u32, project: &ProjectId, workspace: &Path, scopes: &[&str]) -> CreateTaskInput {
    let mut input = create_task_input(query_task_id(id), project, &format!("req-{id}"));
    input.workspace = workspace.to_str().expect("workspace text").into();
    input.allowed_paths = scopes.iter().map(|scope| (*scope).into()).collect();
    input
}
fn rows(storage: &StorageConnection) -> Vec<Vec<Vec<SqlValue>>> {
    ["tasks", "rounds", "events", "active_writers"]
        .iter()
        .map(|table| dump_rows(storage, table))
        .collect()
}

#[test]
fn admission_settings_preserve_defaults_and_worktree_gate() {
    let default = AdmissionSettings::default();
    assert_eq!(default.max_active_tasks(), 1);
    assert!(!default.allow_parallel_writers());
    assert_eq!(default.execution_mode(), ExecutionMode::Direct);
    assert!(matches!(
        AdmissionSettings::new(0, false, ExecutionMode::Direct),
        Err(WriterError::InvalidSettings)
    ));
    assert!(matches!(
        AdmissionSettings::new(3, true, ExecutionMode::Direct),
        Err(WriterError::InvalidSettings)
    ));
    assert_eq!(settings(u64::MAX, true).max_active_tasks(), u64::MAX);
    assert!(AdmissionSettings::new(1, true, ExecutionMode::Worktree).is_ok());
}

#[test]
fn queue_bound_counts_waiters_and_replay_bypasses_full_admission() {
    let (_root, layout, workspace, project) = state("writer-queue");
    let mut storage = layout.open().expect("open");
    let bound = settings(3, false);
    storage
        .create_task_with_admission(
            input(1, &project, &workspace, &["src/a.rs"]),
            &bound,
            TaskStatus::Implementing,
        )
        .expect("writer");
    for id in [2, 3] {
        storage
            .create_task_with_admission(
                input(id, &project, &workspace, &["src/a.rs"]),
                &bound,
                TaskStatus::WaitingDependencies,
            )
            .expect("queued task");
    }
    assert_eq!(
        storage.get_active_writers(&project).expect("writers").len(),
        1
    );
    let before = rows(&storage);
    assert!(matches!(
        storage.create_task_with_admission(
            input(4, &project, &workspace, &["src/b.rs"]),
            &bound,
            TaskStatus::WaitingDependencies
        ),
        Err(CreateTaskError::ProjectBusy)
    ));
    assert_eq!(rows(&storage), before);
    assert!(
        storage
            .create_task_with_admission(
                input(1, &project, &workspace, &["other.rs"]),
                &AdmissionSettings::default(),
                TaskStatus::Implementing
            )
            .expect("replay at full bound")
            .is_replayed()
    );
    assert_eq!(rows(&storage), before);
    assert!(
        !storage
            .activate_waiting_dependencies(query_task_id(2), &project)
            .expect("blocked")
    );
    storage
        .update_task_status(query_task_id(1), &project, TaskStatus::Closed, None)
        .expect("close releases slot");
    assert!(
        storage
            .activate_waiting_dependencies(query_task_id(2), &project)
            .expect("activate queued")
    );
    assert!(
        !storage
            .activate_waiting_dependencies(query_task_id(3), &project)
            .expect("still single writer")
    );
    assert!(matches!(
        storage.update_task_status(query_task_id(3), &project, TaskStatus::Implementing, None),
        Err(WriterError::InvalidTransition)
    ));
}

#[test]
fn all_unfinished_statuses_count_and_terminal_rows_do_not() {
    for status in TaskStatus::ALL {
        let (_root, layout, workspace, project) = state("writer-count-status");
        seed_task_raw(
            &layout.database(),
            &query_task_id(9).to_string(),
            "demo",
            status.as_str(),
        );
        let mut storage = layout.open().expect("open");
        let result = storage.create_task(input(1, &project, &workspace, &["src/a.rs"]));
        if status.is_active() {
            assert!(
                matches!(result, Err(CreateTaskError::ProjectBusy)),
                "{status}"
            );
        } else {
            assert!(result.is_ok(), "{status}");
        }
    }
}

#[test]
fn parallel_scope_matrix_uses_directory_boundaries_and_symlink_identity() {
    for (left, right, allowed) in [
        ("src/a.rs", "src/b.rs", true),
        ("src/", "src/a.rs", false),
        ("src/a.rs", "src/", false),
        ("src/", "src-other/new.rs", true),
        ("alias/new.rs", "src/new.rs", false),
        ("alias/", "src/missing/leaf.rs", false),
    ] {
        let (_root, layout, workspace, project) = state("writer-scope-matrix");
        let mut storage = layout.open().expect("open");
        let config = settings(10, true);
        storage
            .create_task_with_admission(
                input(1, &project, &workspace, &[left]),
                &config,
                TaskStatus::Implementing,
            )
            .expect("first writer");
        let before = rows(&storage);
        let result = storage.create_task_with_admission(
            input(2, &project, &workspace, &[right]),
            &config,
            TaskStatus::Implementing,
        );
        if allowed {
            assert!(result.is_ok(), "{left}/{right}");
        } else {
            assert!(
                matches!(result, Err(CreateTaskError::ScopeOverlap)),
                "{left}/{right}"
            );
            assert_eq!(rows(&storage), before);
        }
        let writers = storage.get_active_writers(&project).expect("writers");
        assert!(writers.iter().all(|writer| writer.parallel));
        let mode: String = storage
            .connection()
            .query_row("SELECT execution_mode FROM tasks LIMIT 1", [], |row| {
                row.get(0)
            })
            .expect("mode");
        assert_eq!(mode, "worktree");
    }
    let (_root, layout, workspace, project) = state("absolute-scope");
    let mut storage = layout.open().expect("open");
    storage
        .create_task_with_admission(
            input(1, &project, &workspace, &["src/new.rs"]),
            &settings(3, true),
            TaskStatus::Implementing,
        )
        .expect("relative");
    let absolute = workspace.join("src/new.rs");
    assert!(matches!(
        storage.create_task_with_admission(
            input(2, &project, &workspace, &[absolute.to_str().expect("path")]),
            &settings(3, true),
            TaskStatus::Implementing
        ),
        Err(CreateTaskError::ScopeOverlap)
    ));
}

#[test]
fn lost_ledger_and_config_downgrade_never_hide_writer_activity() {
    let (_root, layout, workspace, project) = state("writer-lost-lease");
    let mut storage = layout.open().expect("open");
    storage
        .create_task_with_admission(
            input(1, &project, &workspace, &["src/a.rs"]),
            &settings(10, true),
            TaskStatus::Implementing,
        )
        .expect("writer");
    storage
        .connection()
        .execute_batch("DELETE FROM active_writers")
        .expect("simulate lost lease");
    assert!(
        storage
            .get_active_writers(&project)
            .expect("empty ledger")
            .is_empty()
    );
    assert!(
        storage
            .writer_activity_present(&project)
            .expect("real task fence")
    );
    assert!(matches!(
        storage.create_task_with_admission(
            input(2, &project, &workspace, &["src/a.rs"]),
            &settings(10, true),
            TaskStatus::Implementing
        ),
        Err(CreateTaskError::ScopeOverlap)
    ));
    assert!(matches!(
        storage.create_task_with_admission(
            input(2, &project, &workspace, &["src/b.rs"]),
            &settings(10, false),
            TaskStatus::Implementing
        ),
        Err(CreateTaskError::ProjectBusy)
    ));
    assert!(
        !storage
            .writer_activity_present(&query_project("other"))
            .expect("project isolation")
    );
}

#[test]
fn corrupt_ledger_and_real_scopes_fail_closed_with_no_partial_rows() {
    for raw in [
        "broken-json",
        "null",
        "{}",
        "[42]",
        "[\"\"]",
        "[\"src/../a.rs\"]",
        "[\"src//a.rs\"]",
    ] {
        for location in ["ledger", "task"] {
            let (_root, layout, workspace, project) = state("writer-corrupt");
            let mut storage = layout.open().expect("open");
            storage
                .create_task_with_admission(
                    input(1, &project, &workspace, &["src/a.rs"]),
                    &settings(10, true),
                    TaskStatus::Implementing,
                )
                .expect("writer");
            storage
                .connection()
                .execute(
                    if location == "ledger" {
                        "UPDATE active_writers SET scopes_json=?1"
                    } else {
                        "UPDATE tasks SET allowed_paths=?1"
                    },
                    [raw],
                )
                .expect("corrupt data");
            let before = rows(&storage);
            assert!(matches!(
                storage.create_task_with_admission(
                    input(2, &project, &workspace, &["src/b.rs"]),
                    &settings(10, true),
                    TaskStatus::Implementing
                ),
                Err(CreateTaskError::ScopeDataError)
            ));
            assert_eq!(rows(&storage), before);
            assert!(
                storage
                    .writer_activity_present(&project)
                    .expect("corrupt data still means activity")
            );
            if location == "ledger" {
                assert!(matches!(
                    storage.get_active_writers(&project),
                    Err(WriterError::ScopeDataError)
                ));
            }
        }
    }
}

#[test]
fn scope_normalization_and_unresolvable_aliases_refuse_admission() {
    for raw in [
        "[\"./a\"]",
        "[\"a/\"]",
        "[\"/\"]",
        "[\"a\\\\b\"]",
        "[\"a//b\"]",
        "[\"a/../b\"]",
        "[\"a//\"]",
    ] {
        let expected = raw == "[\"a/\"]";
        assert_eq!(crate::writers::parse_scopes(raw).is_ok(), expected, "{raw}");
    }
    for alias in ["loop", "broken"] {
        let (_root, layout, workspace, project) = state("writer-alias-failure");
        symlink(
            if alias == "loop" {
                "loop"
            } else {
                "does-not-exist"
            },
            workspace.join(alias),
        )
        .expect("bad alias");
        let mut storage = layout.open().expect("open");
        let scope = format!("{alias}/new.rs");
        let before = rows(&storage);
        assert!(matches!(
            storage.create_task_with_admission(
                input(1, &project, &workspace, &[&scope]),
                &settings(3, true),
                TaskStatus::Implementing
            ),
            Err(CreateTaskError::ScopeDataError)
        ));
        assert_eq!(rows(&storage), before);
    }
}

#[test]
fn parallel_activation_checks_scopes_and_persists_reservation_atomically() {
    let (_root, layout, workspace, project) = state("writer-parallel-activation");
    let mut storage = layout.open().expect("open");
    let config = settings(5, true);
    storage
        .create_task_with_admission(
            input(1, &project, &workspace, &["src/a.rs"]),
            &config,
            TaskStatus::Implementing,
        )
        .expect("writer");
    storage
        .create_task_with_admission(
            input(2, &project, &workspace, &["src/b.rs"]),
            &config,
            TaskStatus::WaitingDependencies,
        )
        .expect("queued disjoint");
    assert!(
        storage
            .activate_waiting_dependencies_with_admission(query_task_id(2), &project, &config)
            .expect("parallel activate")
    );
    let writers = storage.get_active_writers(&project).expect("writers");
    assert_eq!(writers.len(), 2);
    assert!(writers.iter().all(|writer| writer.parallel));
    let before = rows(&storage);
    assert!(
        !storage
            .activate_waiting_dependencies_with_admission(query_task_id(2), &project, &config)
            .expect("repeat")
    );
    assert_eq!(rows(&storage), before);
    // B1 may queue an overlapping task, but B2 activation must refuse it.
    let single_worktree =
        AdmissionSettings::new(5, false, ExecutionMode::Worktree).expect("settings");
    storage
        .create_task_with_admission(
            input(3, &project, &workspace, &["alias/a.rs"]),
            &single_worktree,
            TaskStatus::WaitingDependencies,
        )
        .expect("queued overlapping");
    let before = rows(&storage);
    assert!(
        !storage
            .activate_waiting_dependencies_with_admission(query_task_id(3), &project, &config)
            .expect("overlap refused")
    );
    assert_eq!(rows(&storage), before);
    storage
        .create_task_with_admission(
            input(4, &project, &workspace, &["src/c.rs"]),
            &settings(5, false),
            TaskStatus::WaitingDependencies,
        )
        .expect("direct waiter");
    assert!(matches!(
        storage.activate_waiting_dependencies_with_admission(query_task_id(4), &project, &config),
        Err(DependencyUpdateError::Writer(WriterError::InvalidSettings))
    ));
}

#[test]
fn reconcile_repairs_orphans_and_lost_reservations_and_is_idempotent() {
    let (_root, layout, workspace, project) = state("writer-repair");
    let mut storage = layout.open().expect("open");
    storage
        .create_task_with_admission(
            input(1, &project, &workspace, &["src/a.rs"]),
            &settings(5, true),
            TaskStatus::Implementing,
        )
        .expect("writer");
    storage.connection().execute_batch("DELETE FROM active_writers; INSERT INTO active_writers VALUES ('orphan','demo','[]','old',1)").expect("orphan");
    assert_eq!(
        storage
            .reconcile_active_writers(&project, &settings(5, true))
            .expect("repair"),
        2
    );
    let before = rows(&storage);
    assert_eq!(
        storage
            .reconcile_active_writers(&project, &settings(5, false))
            .expect("downgrade preserves existing parallel flag"),
        0
    );
    assert_eq!(rows(&storage), before);
    assert!(storage.get_active_writers(&project).expect("writers")[0].parallel);
    storage
        .connection()
        .execute_batch("UPDATE tasks SET status='accepted'")
        .expect("terminal crash window");
    assert!(
        storage
            .writer_activity_present(&project)
            .expect("ledger fence")
    );
    assert_eq!(
        storage
            .reconcile_active_writers(&project, &settings(5, true))
            .expect("terminal repair"),
        1
    );
    assert!(!storage.writer_activity_present(&project).expect("idle"));
}

#[test]
fn reconcile_corruption_rolls_back_other_repairs() {
    let (_root, layout, workspace, project) = state("writer-repair-corruption");
    let mut storage = layout.open().expect("open");
    storage
        .create_task_with_admission(
            input(1, &project, &workspace, &["src/a.rs"]),
            &settings(5, true),
            TaskStatus::Implementing,
        )
        .expect("writer");
    storage.connection().execute_batch("DELETE FROM active_writers; INSERT INTO active_writers VALUES ('orphan','demo','[]','old',1); UPDATE tasks SET allowed_paths='[42]'").expect("corrupt crash window");
    let before = rows(&storage);
    assert!(matches!(
        storage.reconcile_active_writers(&project, &settings(5, true)),
        Err(WriterError::ScopeDataError)
    ));
    assert_eq!(rows(&storage), before);
}

#[test]
fn lifecycle_releases_only_terminal_reservations_in_same_transaction() {
    for finish in [false, true] {
        let (_root, layout, workspace, project) = state("writer-lifecycle");
        let mut storage = layout.open().expect("open");
        let task = query_task_id(1);
        storage
            .create_task(input(1, &project, &workspace, &["src/a.rs"]))
            .expect("writer");
        storage
            .update_task_status(task, &project, TaskStatus::AwaitingReview, Some(2))
            .expect("review keeps slot");
        assert_eq!(
            storage.get_active_writers(&project).expect("writers").len(),
            1
        );
        if finish {
            let round = round_ref(task, &project, 1);
            storage
                .mark_round_observing(round.clone())
                .expect("observing");
            storage
                .finish_round(FinishRoundInput {
                    round,
                    round_status: RoundStatus::Complete,
                    task_status: TaskStatus::Accepted,
                    response_message_id: None,
                    response: None,
                    error_code: None,
                    result_json: None,
                })
                .expect("accepted finish");
        } else {
            storage
                .update_task_status(task, &project, TaskStatus::Accepted, None)
                .expect("accepted status");
        }
        assert!(
            storage
                .get_active_writers(&project)
                .expect("released")
                .is_empty()
        );
        assert!(!storage.writer_activity_present(&project).expect("idle"));
    }
    let (_root, layout, workspace, project) = state("writer-cooperative-close");
    let mut storage = layout.open().expect("open");
    storage
        .create_task(input(1, &project, &workspace, &["src/a.rs"]))
        .expect("writer");
    storage
        .request_task_close(query_task_id(1), "close")
        .expect("request");
    assert_eq!(storage.get_active_writers(&project).expect("held").len(), 1);
    storage
        .complete_requested_close(query_task_id(1))
        .expect("complete close");
    assert!(
        storage
            .get_active_writers(&project)
            .expect("released")
            .is_empty()
    );
}

#[test]
fn reservation_and_release_failure_roll_back_every_row_and_redact_errors() {
    let (_root, layout, workspace, project) = state("writer-rollback");
    let mut storage = layout.open().expect("open");
    storage.connection().execute_batch("CREATE TRIGGER block_reserve BEFORE INSERT ON active_writers BEGIN SELECT RAISE(ABORT,'secret-reservation-text'); END").expect("trigger");
    let before = rows(&storage);
    let error = storage
        .create_task(input(1, &project, &workspace, &["src/a.rs"]))
        .expect_err("reservation failure");
    assert_eq!(rows(&storage), before);
    assert!(!format!("{error} {error:?}").contains("secret-reservation-text"));
    storage
        .connection()
        .execute_batch("DROP TRIGGER block_reserve")
        .expect("remove");
    storage
        .create_task(input(1, &project, &workspace, &["src/a.rs"]))
        .expect("retry");
    storage.connection().execute_batch("CREATE TRIGGER block_release BEFORE DELETE ON active_writers BEGIN SELECT RAISE(ABORT,'secret-release-text'); END").expect("release trigger");
    let before = rows(&storage);
    let error = storage
        .update_task_status(query_task_id(1), &project, TaskStatus::Closed, None)
        .expect_err("release failure");
    assert_eq!(rows(&storage), before);
    assert!(!format!("{error} {error:?}").contains("secret-release-text"));
}

#[test]
fn concurrent_parallel_submissions_admit_disjoint_and_reject_overlap() {
    for overlap in [true, false] {
        let (_root, layout, workspace, project) = state("writer-submit-race");
        let barrier = Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = [1, 2]
                .into_iter()
                .map(|id| {
                    let layout = &layout;
                    let workspace = &workspace;
                    let project = &project;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let mut storage = layout.open().expect("open");
                        let path = if overlap || id == 1 {
                            "src/a.rs"
                        } else {
                            "src/b.rs"
                        };
                        barrier.wait();
                        storage.create_task_with_admission(
                            input(id, project, workspace, &[path]),
                            &settings(3, true),
                            TaskStatus::Implementing,
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            if overlap { 1 } else { 2 }
        );
        if overlap {
            assert!(
                results
                    .iter()
                    .any(|result| matches!(result, Err(CreateTaskError::ScopeOverlap)))
            );
        }
        let storage = layout.open().expect("result");
        let expected = if overlap { 1 } else { 2 };
        assert_eq!(
            storage.get_active_writers(&project).expect("writers").len(),
            expected
        );
        for table in ["tasks", "rounds", "events"] {
            assert_eq!(count_rows(&storage, table), expected as i64);
        }
    }
}

#[test]
fn concurrent_submissions_cannot_exceed_bound_including_waiters() {
    let (_root, layout, workspace, project) = state("writer-bound-race");
    let config = settings(2, true);
    let mut storage = layout.open().expect("open");
    storage
        .create_task_with_admission(
            input(1, &project, &workspace, &["src/queued.rs"]),
            &config,
            TaskStatus::WaitingDependencies,
        )
        .expect("waiting task counts");
    drop(storage);
    let barrier = Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = [2, 3]
            .into_iter()
            .map(|id| {
                let layout = &layout;
                let workspace = &workspace;
                let project = &project;
                let barrier = &barrier;
                let config = &config;
                scope.spawn(move || {
                    let mut storage = layout.open().expect("open racer");
                    barrier.wait();
                    storage.create_task_with_admission(
                        input(id, project, workspace, &[&format!("src/{id}.rs")]),
                        config,
                        TaskStatus::Implementing,
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CreateTaskError::ProjectBusy)))
            .count(),
        1
    );
    let storage = layout.open().expect("result");
    assert_eq!(count_rows(&storage, "tasks"), 2);
    assert_eq!(count_rows(&storage, "rounds"), 2);
    assert_eq!(count_rows(&storage, "events"), 2);
    assert_eq!(
        storage.get_active_writers(&project).expect("ledger").len(),
        1
    );
}

#[test]
fn concurrent_parallel_activation_admits_only_disjoint_scopes() {
    for overlap in [true, false] {
        let (_root, layout, workspace, project) = state("writer-activation-race");
        let queue =
            AdmissionSettings::new(3, false, ExecutionMode::Worktree).expect("queue settings");
        let config = settings(3, true);
        let mut storage = layout.open().expect("open");
        storage
            .create_task_with_admission(
                input(1, &project, &workspace, &["src/a.rs"]),
                &queue,
                TaskStatus::WaitingDependencies,
            )
            .expect("first waiter");
        storage
            .create_task_with_admission(
                input(
                    2,
                    &project,
                    &workspace,
                    &[if overlap { "alias/a.rs" } else { "src/b.rs" }],
                ),
                &queue,
                TaskStatus::WaitingDependencies,
            )
            .expect("second waiter");
        let rounds = dump_rows(&storage, "rounds");
        drop(storage);
        let barrier = Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = [1, 2]
                .into_iter()
                .map(|id| {
                    let layout = &layout;
                    let project = &project;
                    let barrier = &barrier;
                    let config = &config;
                    scope.spawn(move || {
                        let mut storage = layout.open().expect("open racer");
                        barrier.wait();
                        storage
                            .activate_waiting_dependencies_with_admission(
                                query_task_id(id),
                                project,
                                config,
                            )
                            .expect("activate")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread"))
                .collect::<Vec<_>>()
        });
        let winners = if overlap { 1 } else { 2 };
        assert_eq!(results.iter().filter(|won| **won).count(), winners);
        let storage = layout.open().expect("result");
        assert_eq!(
            storage.get_active_writers(&project).expect("writers").len(),
            winners
        );
        assert_eq!(count_rows(&storage, "events"), 2 + winners as i64);
        assert_eq!(dump_rows(&storage, "rounds"), rounds);
    }
}

#[test]
fn corrupt_own_crash_window_reservation_refuses_activation_and_preserves_rows() {
    for (raw, flag) in [("[\"different.rs\"]", 0), ("[]", 2)] {
        let (_root, layout, workspace, project) = state("writer-stale-own");
        let mut storage = layout.open().expect("open");
        storage
            .create_task_with_admission(
                input(1, &project, &workspace, &["src/a.rs"]),
                &settings(3, false),
                TaskStatus::WaitingDependencies,
            )
            .expect("waiter");
        storage
            .connection()
            .execute(
                "INSERT INTO active_writers VALUES (?1,'demo',?2,'stale',?3)",
                rusqlite::params![query_task_id(1).to_string(), raw, flag],
            )
            .expect("corrupt own reservation");
        let before = rows(&storage);
        assert!(
            !storage
                .activate_waiting_dependencies(query_task_id(1), &project)
                .expect("refuse")
        );
        assert_eq!(rows(&storage), before);
    }
}

#[test]
fn pending_close_finish_releases_and_release_failure_rolls_back_round_and_event() {
    let (_root, layout, workspace, project) = state("writer-finish-close");
    let mut storage = layout.open().expect("open");
    let task = query_task_id(1);
    storage
        .create_task(input(1, &project, &workspace, &["src/a.rs"]))
        .expect("writer");
    let round = round_ref(task, &project, 1);
    storage
        .mark_round_observing(round.clone())
        .expect("observing");
    storage
        .request_task_close(task, "close")
        .expect("pending close");
    let finish = FinishRoundInput {
        round,
        round_status: RoundStatus::Failed,
        task_status: TaskStatus::Failed,
        response_message_id: None,
        response: None,
        error_code: None,
        result_json: None,
    };
    storage.connection().execute_batch("CREATE TRIGGER block_release BEFORE DELETE ON active_writers BEGIN SELECT RAISE(ABORT,'secret-terminal-release'); END").expect("trigger");
    let before = rows(&storage);
    let error = storage
        .finish_round(finish.clone())
        .expect_err("release failure");
    assert!(!format!("{error} {error:?}").contains("secret-terminal-release"));
    assert!(error.source().is_some());
    assert_eq!(rows(&storage), before);
    storage
        .connection()
        .execute_batch("DROP TRIGGER block_release")
        .expect("remove");
    storage.finish_round(finish).expect("retry");
    assert_eq!(
        storage.get_task(task).expect("query").expect("task").status,
        TaskStatus::Closed
    );
    assert!(!storage.writer_activity_present(&project).expect("released"));
}

#[test]
fn saved_writer_admission_freezes_flags_repairs_missing_rows_and_rolls_back_corruption() {
    let (_root, layout, workspace, project) = state("saved-writer-admission");
    let mut storage = layout.open().unwrap();
    storage
        .create_task_with_admission(
            input(1, &project, &workspace, &["src/a.rs"]),
            &settings(10, true),
            TaskStatus::Implementing,
        )
        .unwrap();
    assert!(
        storage
            .admit_saved_writer(query_task_id(1), &project, &settings(10, false))
            .unwrap()
    );
    storage
        .connection()
        .execute("DELETE FROM active_writers", [])
        .unwrap();
    assert!(
        !storage
            .admit_saved_writer(query_task_id(1), &project, &settings(10, false))
            .unwrap()
    );
    assert!(
        !storage
            .admit_saved_writer(query_task_id(1), &project, &settings(10, true))
            .unwrap()
    );
    storage
        .connection()
        .execute("UPDATE active_writers SET scopes_json='[\"src/b.rs\"]'", [])
        .unwrap();
    let before = rows(&storage);
    assert!(matches!(
        storage.admit_saved_writer(query_task_id(1), &project, &settings(10, true)),
        Err(WriterError::ScopeDataError)
    ));
    assert_eq!(rows(&storage), before);
}
