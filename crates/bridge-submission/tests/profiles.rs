use bridge_config::{ProjectEntry, load_config};
use bridge_domain::{ProfileOrigin, ProjectId, TaskId, TaskStatus, request_payload_hash};
use bridge_storage::{
    CreateTaskInput, RustStateLayout, StorageConnection, profiles::ProfileReadError,
};
use bridge_submission::{ProfileSubmissionInput, SubmissionError, submit_task_with_profile};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture {
    root: PathBuf,
    layout: RustStateLayout,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-submit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let layout =
            RustStateLayout::new(root.join("state"), ProjectId::from_str("proj").unwrap()).unwrap();
        layout.initialize().unwrap();
        Self { root, layout }
    }
    fn project(&self, extra: &str) -> ProjectEntry {
        let path = self.root.join("projects.toml");
        std::fs::write(&path,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4999\"\npassword_file=\"unused.password\"\nmax_rounds=3\n{extra}",json!(self.root.join("workspace").to_str().unwrap()))).unwrap();
        load_config(&path).unwrap().project("proj").unwrap().clone()
    }
    fn input(&self, profile: Option<Value>) -> ProfileSubmissionInput {
        ProfileSubmissionInput {
            task: CreateTaskInput {
                task_id: TaskId::from_str("11111111-1111-4111-8111-111111111111").unwrap(),
                project_id: self.layout.project_id().clone(),
                workspace: self.root.join("workspace").to_str().unwrap().into(),
                task: "Почини тест".into(),
                request_id: "request-1".into(),
                payload_hash: "caller-hash-is-replaced".into(),
                base_head: None,
                allowed_paths: vec!["src".into()],
                test_commands: vec!["cargo test".into()],
                snapshot: Some(json!({"dirty_paths":[]})),
            },
            profile,
            allow_dirty: false,
            allow_commit: false,
            budget: None,
            initial_status: TaskStatus::Implementing,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn count(storage: &StorageConnection, table: &str) -> i64 {
    storage
        .connection()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn historical_hash_and_replay_keep_the_original_model_and_origin() {
    let f = Fixture::new();
    let mut storage = f.layout.open().unwrap();
    let project = f.project("opencode_model=\"old/model\"\n");
    let outcome = submit_task_with_profile(&mut storage, &project, f.input(None)).unwrap();
    assert!(outcome.is_created());
    let task = outcome.task().task_id;
    let saved = storage
        .get_task_profile(task, project.id())
        .unwrap()
        .unwrap();
    assert_eq!(saved.origin, ProfileOrigin::BuiltinDefault);
    assert_eq!(saved.model.as_deref(), Some("old/model"));
    let expected = request_payload_hash(
        &json!({"kind":"implement","task":"Почини тест","allowed_paths":["src"],"test_commands":["cargo test"],"allow_dirty":false,"allow_commit":false}),
    );
    let actual: String = storage
        .connection()
        .query_row("SELECT payload_hash FROM rounds", [], |r| r.get(0))
        .unwrap();
    assert_eq!(actual, expected);
    let changed = f.project("opencode_model=\"new/model\"\n");
    let replay =
        submit_task_with_profile(&mut storage, &changed, f.input(Some(json!("implementer"))))
            .unwrap();
    assert!(!replay.is_created());
    assert_eq!(
        storage.get_task_profile(task, project.id()).unwrap(),
        Some(saved)
    );
    assert_eq!(count(&storage, "tasks"), 1);
    assert_eq!(count(&storage, "rounds"), 1);
}

#[test]
fn default_and_argument_origins_and_custom_implementer_are_pinned() {
    for (extra, arg, id, origin) in [
        (
            "default_profile=\"test-writer\"\n",
            None,
            "test-writer",
            ProfileOrigin::ProjectDefault,
        ),
        (
            "default_profile=\"test-writer\"\n",
            Some(json!("review-investigator")),
            "review-investigator",
            ProfileOrigin::Argument,
        ),
        (
            "[projects.proj.profiles.implementer]\npurpose=\"Custom\"\ninstructions=\"Check custom rules\"\nmodel=\"custom/model\"\n",
            Some(json!("implementer")),
            "implementer",
            ProfileOrigin::Argument,
        ),
    ] {
        let f = Fixture::new();
        let mut storage = f.layout.open().unwrap();
        let project = f.project(extra);
        let task = submit_task_with_profile(&mut storage, &project, f.input(arg))
            .unwrap()
            .task()
            .task_id;
        let saved = storage
            .get_task_profile(task, project.id())
            .unwrap()
            .unwrap();
        assert_eq!(saved.id, id);
        assert_eq!(saved.origin, origin);
        let raw: String = storage
            .connection()
            .query_row("SELECT profile_json FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, saved.canonical_json().unwrap());
        let hash: String = storage
            .connection()
            .query_row("SELECT payload_hash FROM rounds", [], |r| r.get(0))
            .unwrap();
        let base = json!({"kind":"implement","task":"Почини тест","allowed_paths":["src"],"test_commands":["cargo test"],"allow_dirty":false,"allow_commit":false,"profile":{"id":saved.id,"definition_hash":saved.definition_hash,"model":saved.model}});
        assert_eq!(hash, request_payload_hash(&base));
    }
}

#[test]
fn invalid_and_unknown_profiles_leave_no_records() {
    let f = Fixture::new();
    let mut storage = f.layout.open().unwrap();
    let project = f.project("");
    for arg in [
        json!(""),
        json!(" "),
        json!(" implementer"),
        json!(12),
        json!({}),
        json!([]),
        json!(false),
    ] {
        assert!(matches!(
            submit_task_with_profile(&mut storage, &project, f.input(Some(arg))),
            Err(SubmissionError::InvalidProfile)
        ));
    }
    assert!(matches!(
        submit_task_with_profile(&mut storage, &project, f.input(Some(json!("unknown")))),
        Err(SubmissionError::UnknownProfile)
    ));
    for table in ["tasks", "rounds", "events", "active_writers"] {
        assert_eq!(count(&storage, table), 0);
    }
}

#[test]
fn profile_update_failure_rolls_back_entire_creation() {
    let f = Fixture::new();
    let mut storage = f.layout.open().unwrap();
    let project = f.project("");
    storage.connection().execute_batch("CREATE TRIGGER fail_profile BEFORE UPDATE OF profile_json ON tasks BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert!(submit_task_with_profile(&mut storage, &project, f.input(None)).is_err());
    for table in ["tasks", "rounds", "events", "active_writers"] {
        assert_eq!(count(&storage, table), 0);
    }
    storage
        .connection()
        .execute_batch("DROP TRIGGER fail_profile")
        .unwrap();
    assert!(
        submit_task_with_profile(&mut storage, &project, f.input(None))
            .unwrap()
            .is_created()
    );
}

#[test]
fn changed_definition_conflicts_and_keeps_original_snapshot() {
    let f = Fixture::new();
    let mut storage = f.layout.open().unwrap();
    let project=f.project("default_profile=\"custom\"\n[projects.proj.profiles.custom]\npurpose=\"Custom\"\ninstructions=\"Version one\"\n");
    let task = submit_task_with_profile(&mut storage, &project, f.input(None))
        .unwrap()
        .task()
        .task_id;
    let saved = storage.get_task_profile(task, project.id()).unwrap();
    let changed=f.project("default_profile=\"custom\"\n[projects.proj.profiles.custom]\npurpose=\"Custom\"\ninstructions=\"Version two\"\n");
    assert!(matches!(
        submit_task_with_profile(&mut storage, &changed, f.input(None)),
        Err(SubmissionError::Storage(
            bridge_storage::CreateTaskError::RequestConflict
        ))
    ));
    assert_eq!(storage.get_task_profile(task, project.id()).unwrap(), saved);
}

#[test]
fn strict_reader_rejects_missing_corrupt_and_cross_project_snapshots() {
    let f = Fixture::new();
    let mut storage = f.layout.open().unwrap();
    let project = f.project("");
    let task = submit_task_with_profile(&mut storage, &project, f.input(None))
        .unwrap()
        .task()
        .task_id;
    assert_eq!(
        storage.get_task_profile(task, &ProjectId::from_str("other").unwrap()),
        Err(ProfileReadError::MissingTask)
    );
    let raw: String = storage
        .connection()
        .query_row("SELECT profile_json FROM tasks", [], |r| r.get(0))
        .unwrap();
    for sql in [
        "UPDATE tasks SET profile_hash='bad'",
        "UPDATE tasks SET profile_source='argument'",
        "UPDATE tasks SET profile='test-writer'",
        "UPDATE tasks SET profile_json='{}'",
    ] {
        storage.connection().execute_batch(sql).unwrap();
        assert_eq!(
            storage.get_task_profile(task, project.id()),
            Err(ProfileReadError::CorruptSnapshot)
        );
        let original = project.profile_snapshot(None).unwrap();
        storage
            .connection()
            .execute(
                "UPDATE tasks SET profile=?1,profile_json=?2,profile_hash=?3,profile_source=?4",
                rusqlite::params![
                    original.id,
                    raw,
                    original.canonical_hash().unwrap(),
                    original.origin.as_str()
                ],
            )
            .unwrap();
    }
    storage
        .connection()
        .execute_batch("UPDATE tasks SET profile_json=NULL")
        .unwrap();
    assert_eq!(
        storage.get_task_profile(task, project.id()),
        Err(ProfileReadError::MissingSnapshot)
    );
    storage
        .connection()
        .execute_batch("UPDATE tasks SET profile=NULL")
        .unwrap();
    assert_eq!(storage.get_task_profile(task, project.id()), Ok(None));
}

#[test]
fn frozen_python_payload_hashes_match() {
    let cases: Value = serde_json::from_str(include_str!(
        "../../../docs/fixtures/profile-submit-goldens.json"
    ))
    .unwrap();
    for case in cases["cases"].as_array().unwrap() {
        assert_eq!(
            request_payload_hash(&case["payload"]),
            case["hash"].as_str().unwrap()
        );
    }
}

#[test]
fn identical_concurrent_requests_create_one_pinned_task() {
    let f = Fixture::new();
    let project = f.project("");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let mut storage = f.layout.open().unwrap();
        let project = project.clone();
        let input = f.input(None);
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            submit_task_with_profile(&mut storage, &project, input)
                .unwrap()
                .is_created()
        }));
    }
    assert_eq!(
        handles
            .into_iter()
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>(),
        1
    );
    let storage = f.layout.open().unwrap();
    assert_eq!(count(&storage, "tasks"), 1);
    assert_eq!(count(&storage, "rounds"), 1);
    assert_eq!(count(&storage, "active_writers"), 1);
}

#[test]
fn budget_and_permissions_share_the_creation_transaction() {
    let f = Fixture::new();
    let mut storage = f.layout.open().unwrap();
    let project = f.project("");
    let mut input = f.input(Some(Value::Null));
    input.allow_commit = true;
    input.budget = bridge_storage::validate_budget(&json!({"limits":{"input":123}})).unwrap();
    let task = submit_task_with_profile(&mut storage, &project, input)
        .unwrap()
        .task()
        .task_id;
    let saved = storage.get_task(task).unwrap().unwrap();
    assert_eq!(saved.snapshot.unwrap()["allow_commit"], true);
    let raw: String = storage
        .connection()
        .query_row("SELECT budget_json FROM tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&raw).unwrap()["limits"]["input"],
        123
    );
}
