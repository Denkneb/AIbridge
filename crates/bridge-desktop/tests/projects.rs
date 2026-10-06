use bridge_desktop::{dashboard::Query, projects::ProjectService};
use bridge_storage::{CreateTaskInput, RustStateLayout};
use std::{fs, path::PathBuf};
struct Fixture {
    root: PathBuf,
    service: ProjectService,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bridge-desktop-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("main")).unwrap();
        fs::create_dir(root.join("second")).unwrap();
        let config = root.join("projects.toml");
        fs::write(&config,format!("# keep formatting\n[projects.primary]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file={}\nmax_rounds=3\nauto_approve_external_directories=[{}]\n[projects.second]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4102\"\npassword_file={}\nmax_rounds=3\n",serde_json::json!(root.join("main")),serde_json::json!(root.join("password")),serde_json::json!(root.join("second")),serde_json::json!(root.join("second")),serde_json::json!(root.join("second-password")))).unwrap();
        let service = ProjectService::new(config, root.join("state")).unwrap();
        Self { root, service }
    }
    fn task(&self, project: &str, title: &str, time: &str) -> String {
        let (p, l) = self.service.project(project).unwrap();
        l.initialize().unwrap();
        let mut s = l.open().unwrap();
        let id = uuid::Uuid::new_v4().to_string().parse().unwrap();
        s.create_task(CreateTaskInput {
            task_id: id,
            project_id: p.id().clone(),
            workspace: p.workspace().to_string_lossy().into(),
            task: title.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            payload_hash: "hash".into(),
            base_head: None,
            allowed_paths: vec!["file".into()],
            test_commands: vec!["true".into()],
            snapshot: None,
        })
        .unwrap();
        s.connection()
            .execute(
                "UPDATE tasks SET status='accepted',updated_at=?1 WHERE task_id=?2",
                rusqlite::params![time, id.to_string()],
            )
            .unwrap();
        s.connection()
            .execute(
                "DELETE FROM active_writers WHERE task_id=?1",
                [id.to_string()],
            )
            .unwrap();
        id.to_string()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn preview_secrets_stale_config_and_save_preserve_other_project() {
    let f = Fixture::new();
    let original = fs::read(&f.service.config).unwrap();
    let mut draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    draft.max_rounds = 7;
    let review = f
        .service
        .preview(draft.clone(), Some("private-password".into()), None)
        .unwrap();
    assert!(!review.to_string().contains("private-password"));
    assert_eq!(fs::read(&f.service.config).unwrap(), original);
    f.service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        fs::read_to_string(f.root.join("password")).unwrap(),
        "private-password\n"
    );
    assert_eq!(
        f.service
            .projects()
            .unwrap()
            .iter()
            .find(|p| p.id == "primary")
            .unwrap()
            .max_rounds,
        7
    );
    assert_eq!(
        f.service
            .projects()
            .unwrap()
            .iter()
            .find(|p| p.id == "second")
            .unwrap()
            .max_rounds,
        3
    );
    assert!(
        fs::read_to_string(&f.service.config)
            .unwrap()
            .starts_with("# keep formatting")
    );
    let review = f.service.preview(draft, None, None).unwrap();
    let mut text = fs::read_to_string(&f.service.config).unwrap();
    text.push_str("\n# external edit\n");
    fs::write(&f.service.config, text).unwrap();
    assert!(
        f.service
            .apply(review["review_id"].as_str().unwrap())
            .is_err()
    );
}
#[test]
fn linked_dashboard_global_page_search_and_readonly() {
    let f = Fixture::new();
    let a = f.task("primary", "first", "2026-01-01T00:00:00.000+00:00");
    let b = f.task("second", "needle", "2026-01-02T00:00:00.000+00:00");
    let (_, layout) = f.service.project("primary").unwrap();
    let before = fs::read(layout.database()).unwrap();
    let query = |offset, search| Query {
        project: "primary".into(),
        active_only: false,
        linked: true,
        offset,
        limit: 1,
        search,
        status: None,
    };
    let page = f.service.dashboard(query(0, "".into())).unwrap();
    assert_eq!(page["tasks"][0]["task_id"], b);
    assert_eq!(page["total"], 2);
    let page = f.service.dashboard(query(1, "".into())).unwrap();
    assert_eq!(page["tasks"][0]["task_id"], a);
    let page = f.service.dashboard(query(0, "needle".into())).unwrap();
    assert_eq!(page["tasks"][0]["task_id"], b);
    assert_eq!(page["total"], 1);
    assert_eq!(fs::read(layout.database()).unwrap(), before);
}
#[test]
fn empty_dashboard_never_initializes_runtime() {
    let f = Fixture::new();
    let q = Query {
        project: "primary".into(),
        active_only: true,
        linked: true,
        offset: 0,
        limit: 100,
        search: "".into(),
        status: None,
    };
    assert!(
        f.service.dashboard(q).unwrap()["tasks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!f.service.state.exists());
    let l = RustStateLayout::new(f.service.state.clone(), "primary".parse().unwrap()).unwrap();
    assert!(!l.marker().exists());
}
#[test]
fn config_edit_locks_all_namespaces_once_and_refuses_active_tasks() {
    let f = Fixture::new();
    let id = f.task("primary", "done", "2026-01-01T00:00:00.000+00:00");
    f.task("second", "done", "2026-01-01T00:00:00.000+00:00");
    let mut draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    draft.max_rounds = 9;
    let review = f.service.preview(draft.clone(), None, None).unwrap();
    f.service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
    let (_, l) = f.service.project("primary").unwrap();
    l.open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET status='implementing' WHERE task_id=?1",
            [id],
        )
        .unwrap();
    draft.max_rounds = 10;
    let review = f.service.preview(draft, None, None).unwrap();
    assert!(
        f.service
            .apply(review["review_id"].as_str().unwrap())
            .is_err()
    );
    assert_eq!(
        f.service
            .projects()
            .unwrap()
            .iter()
            .find(|p| p.id == "primary")
            .unwrap()
            .max_rounds,
        9
    );
}
#[test]
fn first_project_can_be_created_without_initial_config_write() {
    let f = Fixture::new();
    fs::remove_file(&f.service.config).unwrap();
    let service = ProjectService::new(f.service.config.clone(), f.service.state.clone()).unwrap();
    assert!(service.projects().unwrap().is_empty());
    assert!(!service.config.exists());
    let existing = bridge_desktop::projects::ProjectDraft {
        id: "first".into(),
        workspace: f.root.join("main").to_string_lossy().into(),
        opencode_url: "http://127.0.0.1:4103".into(),
        mcp_url: None,
        max_rounds: 3,
        execution_mode: "direct".into(),
        delivery_mode: "manual".into(),
        max_active_tasks: 1,
        allow_parallel_writers: false,
        auto_approve_state_directory: false,
        auto_approve_permissions: vec![],
        auto_approve_external_directories: vec![],
    };
    let review = service.preview(existing, None, None).unwrap();
    assert!(!service.config.exists());
    service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(service.projects().unwrap()[0].id, "first");
    assert!(!service.state.exists());
}
#[test]
fn wal_revision_and_usage_include_rounds_outside_visible_limit() {
    let f = Fixture::new();
    let initial = f.service.dashboard_revision("primary", false).unwrap();
    assert!(!f.service.state.exists());
    let id = f.task("primary", "usage", "2026-01-01T00:00:00.000+00:00");
    let revision = f.service.dashboard_revision("primary", false).unwrap();
    assert_ne!(revision, initial);
    let (_, layout) = f.service.project("primary").unwrap();
    let mut storage = layout.open().unwrap();
    let tx = storage.connection_mut().transaction().unwrap();
    tx.execute("DELETE FROM rounds WHERE task_id=?1", [&id])
        .unwrap();
    for n in 1..=101 {
        tx.execute("INSERT INTO rounds(task_id,project_id,round_number,request_id,payload_hash,kind,status,result_json,created_at,updated_at) VALUES (?1,'primary',?2,?3,'hash','initial','completed',?4,'now','now')", rusqlite::params![id,n,format!("round-{n}"),r#"{"usage":{"input":1}}"#]).unwrap();
    }
    tx.execute("INSERT INTO events(task_id,kind,message,created_at) VALUES (?1,'delivery_refused','scope_overlap: /private/journal','now')", [&id]).unwrap();
    tx.commit().unwrap();
    assert_ne!(
        revision,
        f.service.dashboard_revision("primary", false).unwrap()
    );
    let result = f
        .service
        .dashboard(Query {
            project: "primary".into(),
            active_only: false,
            linked: false,
            offset: 0,
            limit: 100,
            search: String::new(),
            status: None,
        })
        .unwrap();
    assert_eq!(result["tasks"][0]["rounds"].as_array().unwrap().len(), 100);
    assert_eq!(result["tasks"][0]["usage"]["input"], 101);
    assert_eq!(
        result["tasks"][0]["delivery_refusal"]["code"],
        "scope_overlap"
    );
    assert!(!result.to_string().contains("/private/journal"));
    assert_eq!(result["waiting_count"], 0);
    assert!(result["reservations"].as_array().unwrap().is_empty());
}
