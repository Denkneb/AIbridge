use super::*;
#[test]
fn progress_is_current_running_only_safe_and_cannot_overwrite_done() {
    let root = TempDir::new("progress");
    let layout = demo_layout(&root.path);
    layout.initialize().unwrap();
    let id = query_task_id(1);
    let project = query_project("demo");
    let mut s = layout.open().unwrap();
    s.create_task(create_task_input(id, &project, "progress"))
        .unwrap();
    let round = RoundRef {
        task_id: id,
        project_id: project,
        round_number: 1,
    };
    assert!(s.verifier_progress(&round, 2).unwrap().is_none());
    assert!(s.save_verifier_progress(&round, 1, 2).is_err());
    s.begin_verifier(round.clone()).unwrap();
    s.save_verifier_progress(&round, 1, 2).unwrap();
    assert_eq!(
        s.verifier_progress(&round, 9).unwrap(),
        Some(serde_json::json!({"state":"running","command_index":1,"command_count":2}))
    );
    assert!(s.save_verifier_progress(&round, 3, 2).is_err());
    s.connection().execute("UPDATE rounds SET verifier_json=?1",[serde_json::json!({"state":"secret","command_index":-1,"command_count":"secret","command":"secret"}).to_string()]).unwrap();
    assert_eq!(
        s.verifier_progress(&round, 4).unwrap(),
        Some(serde_json::json!({"state":"running","command_index":0,"command_count":4}))
    );
    s.complete_verifier(CompleteVerifierInput {
        round: round.clone(),
        verification: serde_json::from_str(&valid_verifier_json()).unwrap(),
    })
    .unwrap();
    assert!(s.verifier_progress(&round, 4).unwrap().is_none());
    assert!(s.save_verifier_progress(&round, 1, 2).is_err());
}
