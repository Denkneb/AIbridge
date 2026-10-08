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
fn endpoint_suggestions_skip_configured_ports_and_other_listeners_without_writing() {
    use std::net::TcpListener;
    let f = Fixture::new();
    let mut doc = fs::read_to_string(&f.service.config)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    // Both endpoint types reserve ports in either range, even when stopped.
    doc["projects"]["primary"]["mcp_url"] = toml_edit::value("http://127.0.0.1:4103/mcp");
    doc["projects"]["second"]["mcp_url"] = toml_edit::value("http://127.0.0.1:4201/mcp");
    fs::write(&f.service.config, doc.to_string()).unwrap();
    let original = fs::read(&f.service.config).unwrap();
    let first = f.service.suggest_endpoints().unwrap();
    let opencode_port: u16 = first
        .opencode_url
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let mcp_port: u16 = first
        .mcp_url
        .trim_end_matches("/mcp")
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(![4101, 4102, 4103, 4201].contains(&opencode_port));
    assert!(![4101, 4102, 4103, 4201].contains(&mcp_port));
    let _opencode = TcpListener::bind(("127.0.0.1", opencode_port)).unwrap();
    let _mcp = TcpListener::bind(("127.0.0.1", mcp_port)).unwrap();
    let next = f.service.suggest_endpoints().unwrap();
    assert_ne!(next.opencode_url, first.opencode_url);
    assert_ne!(next.mcp_url, first.mcp_url);
    assert!(next.mcp_url.ends_with("/mcp"));
    assert_eq!(fs::read(&f.service.config).unwrap(), original);
    assert!(!f.service.state.exists());
}

#[test]
fn endpoint_suggestions_support_an_unsaved_first_project() {
    let root = std::env::temp_dir().join(format!("bridge-endpoints-{}", uuid::Uuid::new_v4()));
    let service = ProjectService::new(root.join("projects.toml"), root.join("state")).unwrap();
    let endpoints = service.suggest_endpoints().unwrap();
    assert!(endpoints.opencode_url.starts_with("http://127.0.0.1:"));
    assert!(endpoints.mcp_url.starts_with("http://127.0.0.1:"));
    assert!(endpoints.mcp_url.ends_with("/mcp"));
    assert!(!root.exists());
}

#[test]
fn setup_after_saving_codex_environment_preserves_preferences_and_is_idempotent() {
    let f = Fixture::new();
    let text = "KEY=fixture-private-value\n";
    f.service.save_codex_env("primary", text).unwrap();
    let (_, layout) = f.service.project("primary").unwrap();
    assert!(layout.project_dir().is_dir());
    assert!(!layout.database().exists());
    assert!(!layout.marker().exists());

    for _ in 0..2 {
        assert_eq!(
            f.service.lifecycle("primary", "setup").unwrap()["status"],
            "ready"
        );
        layout.open_readonly().unwrap();
        assert_eq!(f.service.read_codex_env("primary").unwrap(), text);
    }
}

#[test]
fn setup_refuses_unowned_database_before_creating_credentials() {
    let f = Fixture::new();
    f.service.save_codex_env("primary", "KEY=value\n").unwrap();
    let (_, layout) = f.service.project("primary").unwrap();
    fs::write(layout.database(), b"foreign-state").unwrap();
    let error = f.service.lifecycle("primary", "setup").unwrap_err();
    assert!(error.contains("not owned"));
    assert_eq!(fs::read(layout.database()).unwrap(), b"foreign-state");
    assert!(!layout.marker().exists());
    assert!(!f.root.join("password").exists());
}

#[test]
fn project_models_and_private_env_roundtrip_without_affecting_other_project() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let env = f.root.join("executor.env");
    fs::write(&env, "MODEL_API_KEY=fixture-private-value\n").unwrap();
    fs::set_permissions(&env, fs::Permissions::from_mode(0o600)).unwrap();
    let mut draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    draft.opencode_model = Some("executor/model".into());
    draft.opencode_controller_model = Some("controller/model".into());
    draft.opencode_env_file = Some(env.to_string_lossy().into());
    let original = fs::read(&f.service.config).unwrap();
    let review = f.service.preview(draft.clone(), None, None).unwrap();
    assert!(!review.to_string().contains("fixture-private-value"));
    fs::set_permissions(&env, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        f.service
            .apply(review["review_id"].as_str().unwrap())
            .is_err()
    );
    assert_eq!(fs::read(&f.service.config).unwrap(), original);
    fs::set_permissions(&env, fs::Permissions::from_mode(0o600)).unwrap();
    f.service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
    let projects = f.service.projects().unwrap();
    let saved = projects.iter().find(|p| p.id == "primary").unwrap();
    assert_eq!(saved.opencode_model, draft.opencode_model);
    assert_eq!(
        saved.opencode_controller_model,
        draft.opencode_controller_model
    );
    assert_eq!(saved.opencode_env_file, draft.opencode_env_file);
    assert!(
        projects
            .iter()
            .find(|p| p.id == "second")
            .unwrap()
            .opencode_model
            .is_none()
    );
    let (p, l) = f.service.project("primary").unwrap();
    let payload = bridge_runtime::controller::build_controller_config(
        &p,
        &[],
        &l,
        &f.root.join("bridge"),
        &f.service.config,
    )
    .unwrap();
    assert_eq!(
        payload["agent"]["bridge-controller"]["model"],
        "controller/model"
    );
    assert!(payload.get("model").is_none());
    assert_eq!(
        p.profile_snapshot(None).unwrap().model.as_deref(),
        Some("executor/model")
    );
    let mut invalid = draft.clone();
    invalid.opencode_controller_model = Some("invalid".into());
    assert!(f.service.preview(invalid, None, None).is_err());
    draft.opencode_model = None;
    draft.opencode_controller_model = None;
    draft.opencode_env_file = None;
    let review = f.service.preview(draft, None, None).unwrap();
    f.service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
    assert!(
        f.service
            .projects()
            .unwrap()
            .iter()
            .find(|p| p.id == "primary")
            .unwrap()
            .opencode_controller_model
            .is_none()
    );
}

#[test]
fn opencode_editor_preserves_jsonc_and_backup_and_refuses_stale_edits() {
    let f = Fixture::new();
    let path = f.root.join("main/opencode.jsonc");
    let original = "{\n // keep comment\n \"model\": \"provider/old\",\n}\n";
    fs::write(&path, original).unwrap();
    assert_eq!(
        f.service
            .read_opencode_config("primary", "opencode.jsonc")
            .unwrap()["content"],
        original
    );
    let proposed = original.replace("provider/old", "provider/new");
    let review = f
        .service
        .preview_opencode_config("primary", "opencode.jsonc", &proposed, Some(original))
        .unwrap();
    fs::write(&path, "{}").unwrap();
    assert!(
        f.service
            .apply_opencode_config(review["review_id"].as_str().unwrap())
            .is_err()
    );
    assert!(
        f.service
            .preview_opencode_config("primary", "opencode.jsonc", &proposed, Some(original))
            .is_err()
    );
    fs::write(&path, original).unwrap();
    f.service
        .apply_opencode_config(review["review_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), proposed);
    let backup = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|v| v.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "bak"))
        .unwrap();
    assert_eq!(fs::read_to_string(backup).unwrap(), original);
    assert!(!f.root.join("second/opencode.jsonc").exists());
    assert!(!f.service.state.exists());
}

#[test]
fn opencode_editor_rejects_reserved_fields_symlinks_and_active_controller() {
    use std::os::{fd::AsRawFd, unix::fs::symlink};
    let f = Fixture::new();
    for content in [
        "[]",
        "{broken",
        "{\"default_agent\":null}",
        "{\"subagent_depth\":1}",
        "{\"mcp\":{\"agent_bridge\":{}}}",
        "{\"agent\":{\"bridge-controller\":{}}}",
    ] {
        assert!(
            f.service
                .preview_opencode_config("primary", "opencode.json", content, None)
                .is_err()
        );
    }
    assert!(
        f.service
            .read_opencode_config("primary", "../projects.toml")
            .is_err()
    );
    let path = f.root.join("main/opencode.json");
    symlink(&f.service.config, &path).unwrap();
    assert!(
        f.service
            .read_opencode_config("primary", "opencode.json")
            .is_err()
    );
    fs::remove_file(&path).unwrap();
    let review = f
        .service
        .preview_opencode_config("primary", "opencode.json", "{}", None)
        .unwrap();
    let (_, layout) = f.service.project("primary").unwrap();
    layout.initialize().unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(layout.project_dir().join("controller.lock"))
        .unwrap();
    nix::fcntl::flock(lock.as_raw_fd(), nix::fcntl::FlockArg::LockSharedNonblock).unwrap();
    assert!(
        f.service
            .apply_opencode_config(review["review_id"].as_str().unwrap())
            .is_err()
    );
    assert!(!path.exists());
    drop(lock);
    f.service
        .apply_opencode_config(review["review_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "{}");
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
fn project_settings_and_keys_can_be_saved_with_unfinished_tasks_when_stopped() {
    let f = Fixture::new();
    let id = f.task("primary", "unfinished", "2026-01-01T00:00:00.000+00:00");
    let (_, layout) = f.service.project("primary").unwrap();
    let mut draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    for (index, status) in [
        "implementing",
        "revising",
        "awaiting_review",
        "needs_user",
        "failed",
        "waiting_dependencies",
        "delivery_unknown",
    ]
    .iter()
    .enumerate()
    {
        layout
            .open()
            .unwrap()
            .connection()
            .execute(
                "UPDATE tasks SET status=?1 WHERE task_id=?2",
                rusqlite::params![status, id],
            )
            .unwrap();
        draft.max_rounds = 10 + index as u64;
        let review = f.service.preview(draft.clone(), None, None).unwrap();
        f.service
            .apply(review["review_id"].as_str().unwrap())
            .unwrap();
        let original = f
            .service
            .read_opencode_config("primary", "opencode.json")
            .unwrap();
        let review = f
            .service
            .preview_opencode_config(
                "primary",
                "opencode.json",
                "{}",
                if original["exists"] == true {
                    original["content"].as_str()
                } else {
                    None
                },
            )
            .unwrap();
        f.service
            .apply_opencode_config(review["review_id"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            layout
                .open()
                .unwrap()
                .get_task(id.parse().unwrap())
                .unwrap()
                .unwrap()
                .status
                .to_string(),
            *status
        );
    }
    f.service
        .save_opencode_key("primary", "PROVIDER_API_KEY", Some("secret"), None)
        .unwrap();
    assert_eq!(
        layout
            .open()
            .unwrap()
            .get_task(id.parse().unwrap())
            .unwrap()
            .unwrap()
            .status
            .to_string(),
        "delivery_unknown"
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
        remote_execution: None,
        id: "first".into(),
        workspace: f.root.join("main").to_string_lossy().into(),
        opencode_url: "http://127.0.0.1:4103".into(),
        mcp_url: None,
        opencode_model: None,
        opencode_controller_model: None,
        opencode_env_file: None,
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
    assert!(result["tasks"][0].get("rounds").is_none());
    assert!(result["tasks"][0].get("usage").is_none());
    let detail = f.service.task_detail("primary", &id).unwrap();
    assert_eq!(detail["rounds"].as_array().unwrap().len(), 10);
    assert_eq!(detail["rounds"][0]["number"], 101);
    assert_eq!(detail["usage"]["input"], 101);
    assert_eq!(detail["revision"], result["tasks"][0]["revision"]);
    let mut numbers: Vec<u64> = detail["rounds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["number"].as_u64().unwrap())
        .collect();
    let mut cursor = detail["next_before"].as_u64();
    while let Some(before) = cursor {
        let page = f
            .service
            .task_rounds(
                "primary",
                &id,
                before as u32,
                detail["revision"].as_str().unwrap(),
            )
            .unwrap();
        assert!(page["rounds"].as_array().unwrap().len() <= 10);
        numbers.extend(
            page["rounds"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["number"].as_u64().unwrap()),
        );
        cursor = page["next_before"].as_u64();
    }
    assert_eq!(numbers, (1..=101).rev().collect::<Vec<_>>());
    assert!(f.service.task_detail("second", &id).is_err());
    assert!(
        f.service
            .task_rounds("primary", &id, 0, detail["revision"].as_str().unwrap())
            .is_err()
    );
    storage
        .connection()
        .execute(
            "UPDATE rounds SET updated_at='later' WHERE task_id=?1 AND round_number=1",
            [&id],
        )
        .unwrap();
    assert!(
        f.service
            .task_rounds("primary", &id, 91, detail["revision"].as_str().unwrap())
            .is_err()
    );
    assert_eq!(detail["delivery_refusal"]["code"], "scope_overlap");
    assert!(!result.to_string().contains("/private/journal"));
    assert!(!detail.to_string().contains("/private/journal"));
    assert_eq!(result["waiting_count"], 0);
    assert!(result["reservations"].as_array().unwrap().is_empty());
}

#[test]
fn task_revisions_track_selected_round_updates_without_invalidating_other_cards() {
    let f = Fixture::new();
    let first = f.task("primary", "first", "now");
    let second = f.task("primary", "second", "now");
    let first_revision = f.service.task_detail("primary", &first).unwrap()["revision"].clone();
    let second_revision = f.service.task_detail("primary", &second).unwrap()["revision"].clone();
    let (_, layout) = f.service.project("primary").unwrap();
    layout.open().unwrap().connection().execute(
        r#"UPDATE rounds SET verifier_json='{"status":"passed"}',updated_at='later' WHERE task_id=?1"#,
        [&second],
    ).unwrap();
    let snapshot = f
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
    let rows = snapshot["tasks"].as_array().unwrap();
    assert_eq!(
        rows.iter().find(|t| t["task_id"] == first).unwrap()["revision"],
        first_revision
    );
    assert_ne!(
        rows.iter().find(|t| t["task_id"] == second).unwrap()["revision"],
        second_revision
    );
    assert_eq!(
        f.service.task_detail("primary", &second).unwrap()["rounds"][0]["verification"]["status"],
        "passed"
    );
}

#[test]
fn codex_environment_survives_reopen_is_private_and_project_scoped() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let text = "export HTTPS_PROXY=http://127.0.0.1:8888\nPRIVATE_KEY='fixture-secret'\n";
    assert_eq!(f.service.read_codex_env("primary").unwrap(), "");
    f.service.save_codex_env("primary", text).unwrap();
    let reopened = ProjectService::new(f.service.config.clone(), f.service.state.clone()).unwrap();
    assert_eq!(reopened.read_codex_env("primary").unwrap(), text);
    assert_eq!(reopened.read_codex_env("second").unwrap(), "");
    let file = f.root.join("state/primary/desktop-codex.env");
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        !fs::read_to_string(&f.service.config)
            .unwrap()
            .contains("fixture-secret")
    );
    assert!(
        !serde_json::to_string(&reopened.projects().unwrap())
            .unwrap()
            .contains("fixture-secret")
    );
    let error = reopened
        .save_codex_env("primary", "1INVALID=fixture-secret")
        .unwrap_err();
    assert!(!error.contains("fixture-secret"));
    assert_eq!(reopened.read_codex_env("primary").unwrap(), text);
    reopened.save_codex_env("primary", "").unwrap();
    assert_eq!(reopened.read_codex_env("primary").unwrap(), "");
}

#[test]
fn codex_environment_refuses_unknown_project_symlink_and_public_file() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let f = Fixture::new();
    assert!(f.service.save_codex_env("../primary", "KEY=value").is_err());
    f.service.save_codex_env("primary", "KEY=value").unwrap();
    let file = f.root.join("state/primary/desktop-codex.env");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(f.service.read_codex_env("primary").is_err());
    assert!(f.service.save_codex_env("primary", "KEY=changed").is_err());
    fs::remove_file(&file).unwrap();
    let outside = f.root.join("outside.env");
    fs::write(&outside, "KEY=original").unwrap();
    symlink(&outside, &file).unwrap();
    assert!(f.service.read_codex_env("primary").is_err());
    assert!(f.service.save_codex_env("primary", "KEY=changed").is_err());
    assert_eq!(fs::read_to_string(outside).unwrap(), "KEY=original");
}

#[test]
fn independent_project_can_be_edited_and_added_while_another_is_active() {
    use std::os::{fd::AsRawFd, unix::fs::PermissionsExt};
    let f = Fixture::new();
    let config = fs::read_to_string(&f.service.config)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("auto_approve_external_directories="))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&f.service.config, config).unwrap();
    let task = f.task("primary", "running", "2026-01-01T00:00:00.000+00:00");
    let (_, active) = f.service.project("primary").unwrap();
    active
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET status='implementing' WHERE task_id=?1",
            [&task],
        )
        .unwrap();
    let mut locks = Vec::new();
    for (name, kind) in [
        ("worker.lock", nix::fcntl::FlockArg::LockExclusiveNonblock),
        ("controller.lock", nix::fcntl::FlockArg::LockSharedNonblock),
    ] {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(active.project_dir().join(name))
            .unwrap();
        nix::fcntl::flock(file.as_raw_fd(), kind).unwrap();
        locks.push(file);
    }
    let record = active.project_dir().join("opencode.process.json");
    fs::write(&record, serde_json::json!({"pid":999999,"start":"1","boot_id":"fixture",
        "project_id":"primary","task_id":"00000000-0000-0000-0000-000000000000",
        "checkout":f.root.join("main"),"kind":"opencode","port":4101,"endpoint":"http://127.0.0.1:4101"}).to_string()).unwrap();
    fs::set_permissions(record, fs::Permissions::from_mode(0o600)).unwrap();
    let mut second = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "second")
        .unwrap();
    second.max_rounds = 8;
    let review = f.service.preview(second.clone(), None, None).unwrap();
    f.service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
    let review = f
        .service
        .preview_opencode_config("second", "opencode.json", "{}", None)
        .unwrap();
    f.service
        .apply_opencode_config(review["review_id"].as_str().unwrap())
        .unwrap();
    fs::create_dir(f.root.join("third")).unwrap();
    second.id = "third".into();
    second.workspace = f.root.join("third").to_string_lossy().into();
    second.opencode_url = "http://127.0.0.1:4103".into();
    let review = f.service.preview(second, None, None).unwrap();
    f.service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
    assert!(
        f.service
            .projects()
            .unwrap()
            .iter()
            .any(|p| p.id == "third")
    );
    assert_eq!(
        active
            .open_readonly()
            .unwrap()
            .get_task(task.parse().unwrap())
            .unwrap()
            .unwrap()
            .status
            .to_string(),
        "implementing"
    );
    let draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    let review = f.service.preview(draft, None, None).unwrap();
    assert!(
        f.service
            .apply(review["review_id"].as_str().unwrap())
            .is_err()
    );
}

#[test]
fn project_edit_protects_existing_and_new_linked_controller_bindings() {
    use std::os::fd::AsRawFd;
    let f = Fixture::new();
    let (_, layout) = f.service.project("primary").unwrap();
    layout.initialize().unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(layout.project_dir().join("controller.lock"))
        .unwrap();
    nix::fcntl::flock(lock.as_raw_fd(), nix::fcntl::FlockArg::LockSharedNonblock).unwrap();
    let mut draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "second")
        .unwrap();
    draft.max_rounds = 8;
    let review = f.service.preview(draft.clone(), None, None).unwrap();
    assert!(
        f.service
            .apply(review["review_id"].as_str().unwrap())
            .is_err()
    );
    // Removing an old link still requires stopping its consumer.
    fs::create_dir(f.root.join("third")).unwrap();
    draft.workspace = f.root.join("third").to_string_lossy().into();
    let review = f.service.preview(draft.clone(), None, None).unwrap();
    assert!(
        f.service
            .apply(review["review_id"].as_str().unwrap())
            .is_err()
    );
    // Registering a formerly unregistered trusted workspace introduces a link.
    let mut doc = fs::read_to_string(&f.service.config)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    doc["projects"].as_table_mut().unwrap().remove("second");
    fs::write(&f.service.config, doc.to_string()).unwrap();
    draft.workspace = f.root.join("second").to_string_lossy().into();
    let review = f.service.preview(draft, None, None).unwrap();
    assert!(
        f.service
            .apply(review["review_id"].as_str().unwrap())
            .is_err()
    );
    drop(lock);
    f.service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
}

#[test]
fn project_edit_protects_another_project_using_the_same_credentials() {
    use std::os::fd::AsRawFd;
    let f = Fixture::new();
    let mut doc = fs::read_to_string(&f.service.config)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    doc["projects"]["second"]["password_file"] =
        doc["projects"]["primary"]["password_file"].clone();
    fs::write(&f.service.config, doc.to_string()).unwrap();
    let (_, layout) = f.service.project("second").unwrap();
    layout.initialize().unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(layout.project_dir().join("controller.lock"))
        .unwrap();
    nix::fcntl::flock(lock.as_raw_fd(), nix::fcntl::FlockArg::LockSharedNonblock).unwrap();
    let draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    let review = f
        .service
        .preview(draft, Some("fixture-new-password".into()), None)
        .unwrap();
    assert!(
        f.service
            .apply(review["review_id"].as_str().unwrap())
            .is_err()
    );
    drop(lock);
    f.service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
}

#[test]
fn config_edit_reports_live_service_and_ignores_its_record_after_exit() {
    use std::{
        os::unix::fs::PermissionsExt,
        process::{Child, Command},
    };
    struct Service(Child);
    impl Drop for Service {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let f = Fixture::new();
    let (_, layout) = f.service.project("primary").unwrap();
    layout.initialize().unwrap();
    let mut child = Service(Command::new("sleep").arg("30").spawn().unwrap());
    let pid = child.0.id();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap();
    let record = layout.project_dir().join("opencode.process.json");
    fs::write(&record, serde_json::json!({"pid":pid,"start":start,
        "boot_id":fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),
        "project_id":"primary","task_id":"00000000-0000-0000-0000-000000000000",
        "checkout":f.root.join("main"),"kind":"opencode","port":4101,"endpoint":"http://127.0.0.1:4101"}).to_string()).unwrap();
    fs::set_permissions(&record, fs::Permissions::from_mode(0o600)).unwrap();
    let mut draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    draft.max_rounds = 9;
    let review = f.service.preview(draft, None, None).unwrap();
    let error = f
        .service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap_err();
    assert!(error.contains("Проект primary") && error.contains("сервис opencode"));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    f.service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
    assert!(record.exists()); // No record deletion or process signalling by config save.
}

#[test]
fn config_edit_reports_invalid_state_instead_of_claiming_services_are_running() {
    let f = Fixture::new();
    let (_, layout) = f.service.project("primary").unwrap();
    layout.initialize().unwrap();
    let draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    let review = f.service.preview(draft, None, None).unwrap();
    fs::write(layout.marker(), "foreign fixture marker").unwrap();
    let original = fs::read(&f.service.config).unwrap();
    let error = f
        .service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap_err();
    assert!(error.contains("Проект primary") && error.contains("runtime_state_unowned"));
    assert!(!error.contains("Остановите"));
    assert_eq!(fs::read(&f.service.config).unwrap(), original);
    assert_eq!(
        fs::read_to_string(layout.marker()).unwrap(),
        "foreign fixture marker"
    );
}

#[test]
fn provider_keys_are_private_persisted_and_never_returned_to_ui() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    assert_eq!(
        f.service.opencode_keys("primary").unwrap()["names"],
        serde_json::json!([])
    );
    let saved = f
        .service
        .save_opencode_key("primary", "PROVIDER_API_KEY", Some("fixture-secret"), None)
        .unwrap();
    assert!(!saved.to_string().contains("fixture-secret"));
    let file = saved["file"].as_str().unwrap();
    assert_eq!(
        fs::metadata(file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let reopened = ProjectService::new(f.service.config.clone(), f.service.state.clone()).unwrap();
    let (project, _) = reopened.project("primary").unwrap();
    assert_eq!(
        project
            .read_opencode_env()
            .unwrap()
            .unwrap()
            .get("PROVIDER_API_KEY"),
        Some("fixture-secret")
    );
    assert!(
        !fs::read_to_string(&f.service.config)
            .unwrap()
            .contains("fixture-secret")
    );
    assert!(
        !reopened
            .opencode_keys("primary")
            .unwrap()
            .to_string()
            .contains("fixture-secret")
    );
    reopened
        .save_opencode_key(
            "primary",
            "SECOND_API_KEY",
            Some("second-secret"),
            Some(file),
        )
        .unwrap();
    reopened
        .save_opencode_key(
            "primary",
            "PROVIDER_API_KEY",
            Some("replaced-secret"),
            Some(file),
        )
        .unwrap();
    reopened
        .save_opencode_key("primary", "PROVIDER_API_KEY", None, Some(file))
        .unwrap();
    let (project, _) = reopened.project("primary").unwrap();
    let env = project.read_opencode_env().unwrap().unwrap();
    assert!(!env.contains_key("PROVIDER_API_KEY"));
    assert_eq!(env.get("SECOND_API_KEY"), Some("second-secret"));
    assert!(
        reopened.opencode_keys("second").unwrap()["names"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn provider_keys_copy_external_env_without_changing_shared_file_or_other_project() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let external = f.root.join("shared.env");
    fs::write(&external, "SHARED_KEY=external-secret\n").unwrap();
    fs::set_permissions(&external, fs::Permissions::from_mode(0o600)).unwrap();
    let mut doc = fs::read_to_string(&f.service.config)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    for id in ["primary", "second"] {
        doc["projects"][id]["opencode_env_file"] = toml_edit::value(external.to_str().unwrap());
    }
    fs::write(&f.service.config, doc.to_string()).unwrap();
    f.service
        .save_opencode_key(
            "primary",
            "PROVIDER_API_KEY",
            Some("primary-secret"),
            external.to_str(),
        )
        .unwrap();
    assert_eq!(
        fs::read_to_string(&external).unwrap(),
        "SHARED_KEY=external-secret\n"
    );
    let (primary, _) = f.service.project("primary").unwrap();
    let env = primary.read_opencode_env().unwrap().unwrap();
    assert_eq!(env.get("SHARED_KEY"), Some("external-secret"));
    assert_eq!(env.get("PROVIDER_API_KEY"), Some("primary-secret"));
    assert_eq!(
        f.service.opencode_keys("second").unwrap()["names"],
        serde_json::json!(["SHARED_KEY"])
    );
    assert!(
        f.service
            .save_opencode_key("primary", "SHARED_KEY", Some("new"), external.to_str())
            .is_err()
    );
}

#[test]
fn provider_keys_reject_injection_reserved_names_unsafe_files_and_active_project() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let f = Fixture::new();
    let original = fs::read(&f.service.config).unwrap();
    for (name, value) in [
        ("OPENCODE_SERVER_PASSWORD", "secret"),
        ("A=\nB", "secret"),
        ("KEY", "secret\nOTHER=injected"),
        ("KEY", "secret\u{2028}OTHER=injected"),
        ("1KEY", "secret"),
    ] {
        let error = f
            .service
            .save_opencode_key("primary", name, Some(value), None)
            .unwrap_err();
        assert!(!error.contains("secret"));
        assert_eq!(fs::read(&f.service.config).unwrap(), original);
    }
    f.service
        .save_opencode_key("primary", "KEY", Some("secret"), None)
        .unwrap();
    let view = f.service.opencode_keys("primary").unwrap();
    let file = view["file"].as_str().unwrap();
    fs::set_permissions(file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        f.service
            .save_opencode_key("primary", "KEY", Some("new"), Some(file))
            .is_err()
    );
    fs::remove_file(file).unwrap();
    let foreign = f.root.join("foreign.env");
    fs::write(&foreign, "KEY=foreign\n").unwrap();
    symlink(&foreign, file).unwrap();
    assert!(
        f.service
            .save_opencode_key("primary", "KEY", Some("new"), Some(file))
            .is_err()
    );
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "KEY=foreign\n");
    fs::remove_file(file).unwrap();
    fs::write(file, "KEY=secret\n").unwrap();
    fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    let id = f.task("primary", "active", "now");
    let (_, layout) = f.service.project("primary").unwrap();
    layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET status='needs_user' WHERE task_id=?1",
            [&id],
        )
        .unwrap();
    let bridge_worker::WorkerLockOutcome::Acquired(worker) =
        bridge_worker::WorkerLock::try_acquire_task(&layout, id.parse().unwrap()).unwrap()
    else {
        panic!("worker fence unavailable")
    };
    assert!(
        f.service
            .save_opencode_key("primary", "KEY", Some("new"), Some(file))
            .unwrap_err()
            .contains("исполнитель")
    );
    assert_eq!(fs::read_to_string(file).unwrap(), "KEY=secret\n");
    drop(worker);
    f.service
        .save_opencode_key("primary", "KEY", Some("new"), Some(file))
        .unwrap();
}

#[test]
fn stopped_tasks_allow_edits_but_each_executor_fence_still_blocks_them() {
    use std::os::fd::AsRawFd;
    let f = Fixture::new();
    let id = f.task("primary", "unfinished", "now");
    let (_, layout) = f.service.project("primary").unwrap();
    layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET status='implementing' WHERE task_id=?1",
            [&id],
        )
        .unwrap();
    let mut draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    draft.max_rounds = 8;
    for (name, kind) in [
        ("worker.lock", nix::fcntl::FlockArg::LockExclusiveNonblock),
        (
            "admission.lock",
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        ),
        ("controller.lock", nix::fcntl::FlockArg::LockSharedNonblock),
    ] {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(layout.project_dir().join(name))
            .unwrap();
        nix::fcntl::flock(file.as_raw_fd(), kind).unwrap();
        let review = f.service.preview(draft.clone(), None, None).unwrap();
        assert!(
            f.service
                .apply(review["review_id"].as_str().unwrap())
                .is_err()
        );
        drop(file);
    }
    let config =
        bridge_config::load_config_with_state_root(&f.service.config, &f.service.state).unwrap();
    assert!(bridge_runtime::project::config_edit_guard(&config, &f.service.state).is_err());
    let guard = bridge_runtime::project::project_config_edit_guard(
        &config,
        &config,
        "primary",
        &f.service.state,
    )
    .unwrap();
    assert!(matches!(
        bridge_worker::WorkerLock::try_acquire_task(&layout, id.parse().unwrap()).unwrap(),
        bridge_worker::WorkerLockOutcome::Busy
    ));
    drop(guard);
    let review = f.service.preview(draft, None, None).unwrap();
    f.service
        .apply(review["review_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        layout
            .open()
            .unwrap()
            .get_task(id.parse().unwrap())
            .unwrap()
            .unwrap()
            .status
            .to_string(),
        "implementing"
    );
}

#[test]
fn failed_task_recovery_is_explicit_and_available_only_for_bound_assistant_errors() {
    let f = Fixture::new();
    let id = f.task("primary", "fixture", "2026-01-01T00:00:00Z");
    let (_, l) = f.service.project("primary").unwrap();
    let s = l.open().unwrap();
    let detail = || f.service.task_detail("primary", &id).unwrap();
    assert_eq!(detail()["recoverable"], false);
    s.connection()
        .execute("UPDATE tasks SET status='failed' WHERE task_id=?1", [&id])
        .unwrap();
    s.connection().execute("UPDATE rounds SET status='failed',error_code='assistant_error',attempted=1,session_id='ses_fixture',outbound_message_id='msg_fixture' WHERE task_id=?1",[&id]).unwrap();
    assert_eq!(detail()["recoverable"], true);
    assert_eq!(
        s.get_task(id.parse().unwrap()).unwrap().unwrap().status,
        bridge_domain::TaskStatus::Failed
    );
    for code in ["worker_error", "workspace_mismatch", "session_not_found"] {
        s.connection()
            .execute(
                "UPDATE rounds SET error_code=?1 WHERE task_id=?2",
                [code, &id],
            )
            .unwrap();
        assert_eq!(detail()["recoverable"], false);
        assert!(f.service.recover_failed_task("primary", &id).is_err());
        assert_eq!(
            s.get_task(id.parse().unwrap()).unwrap().unwrap().status,
            bridge_domain::TaskStatus::Failed
        );
        let saved: String = s
            .connection()
            .query_row(
                "SELECT error_code FROM rounds WHERE task_id=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(saved, code);
    }
    assert!(f.service.recover_failed_task("second", &id).is_err());
    assert!(f.service.recover_failed_task("primary", "invalid").is_err());
}

#[test]
fn manual_status_supports_all_unfinished_labels_and_preserves_evidence() {
    use bridge_domain::TaskStatus;
    let f = Fixture::new();
    let id = f.task("primary", "fixture", "2026-01-01T00:00:00Z");
    let (p, l) = f.service.project("primary").unwrap();
    let s = l.open().unwrap();
    s.connection()
        .execute("UPDATE tasks SET status='failed' WHERE task_id=?1", [&id])
        .unwrap();
    s.connection().execute("UPDATE rounds SET status='failed',error_code='assistant_error',attempted=1,session_id='ses_fixture',outbound_message_id='msg_fixture' WHERE task_id=?1",[&id]).unwrap();
    let round:String=s.connection().query_row("SELECT json_object('status',status,'error',error_code,'session',session_id,'outbound',outbound_message_id,'result',result_json,'verifier',verifier_json) FROM rounds WHERE task_id=?1",[&id],|r|r.get(0)).unwrap();
    let mut expected = TaskStatus::Failed;
    for target in TaskStatus::ALL
        .into_iter()
        .filter(|v| v.is_active() && *v != TaskStatus::Failed)
        .chain([TaskStatus::Failed])
    {
        let result = f
            .service
            .set_task_status(
                "primary",
                &id,
                expected.as_str(),
                target.as_str(),
                "Исправление статуса после ручной работы",
            )
            .unwrap();
        assert_eq!(result["status"], target.as_str());
        let saved:String=s.connection().query_row("SELECT json_object('status',status,'error',error_code,'session',session_id,'outbound',outbound_message_id,'result',result_json,'verifier',verifier_json) FROM rounds WHERE task_id=?1",[&id],|r|r.get(0)).unwrap();
        assert_eq!(saved, round);
        assert_eq!(
            s.get_task(id.parse().unwrap())
                .unwrap()
                .unwrap()
                .revision_count,
            0
        );
        expected = target;
    }
    let count: i64 = s
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE task_id=?1 AND kind='manual_status_change'",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 7);
    assert!(
        f.service
            .set_task_status("primary", &id, "failed", "accepted", "Причина")
            .is_err()
    );
    assert!(
        f.service
            .set_task_status("primary", &id, "failed", "closed", "Причина")
            .is_err()
    );
    assert!(
        f.service
            .set_task_status("primary", &id, "needs_user", "implementing", "Причина")
            .is_err()
    );
    assert!(
        f.service
            .set_task_status("primary", &id, "failed", "needs_user", "")
            .is_err()
    );
    assert!(
        f.service
            .set_task_status("second", &id, "failed", "needs_user", "Причина")
            .is_err()
    );
    let worker = match bridge_worker::WorkerLock::try_acquire_task(&l, id.parse().unwrap()).unwrap()
    {
        bridge_worker::WorkerLockOutcome::Acquired(g) => g,
        _ => panic!("busy"),
    };
    assert!(
        f.service
            .set_task_status("primary", &id, "failed", "needs_user", "Причина")
            .is_err()
    );
    drop(worker);
    // A label is not sufficient evidence for acceptance of an incomplete round.
    f.service
        .set_task_status(
            "primary",
            &id,
            "failed",
            "awaiting_review",
            "Проверка ручного статуса",
        )
        .unwrap();
    assert!(
        l.open()
            .unwrap()
            .accept_manual_task(id.parse().unwrap(), p.id())
            .is_err()
    );
    for terminal in ["accepted", "closed"] {
        s.connection()
            .execute(
                "UPDATE tasks SET status=?1 WHERE task_id=?2",
                [terminal, &id],
            )
            .unwrap();
        assert!(
            f.service
                .set_task_status("primary", &id, terminal, "needs_user", "Причина")
                .is_err()
        );
        assert_eq!(
            s.get_task(id.parse().unwrap())
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            terminal
        );
    }
}

#[test]
fn settings_preview_preserves_specific_safe_configuration_errors() {
    let f = Fixture::new();
    let projects = f.service.projects().unwrap();
    let primary = projects.iter().find(|p| p.id == "primary").unwrap().clone();
    let second = projects.iter().find(|p| p.id == "second").unwrap();
    let mut draft = primary.clone();
    draft.workspace = second.workspace.clone();
    assert_eq!(
        f.service.preview(draft, None, None).unwrap_err(),
        "project workspace is already used by another project"
    );
    let mut draft = primary.clone();
    draft.opencode_url = second.opencode_url.clone();
    assert_eq!(
        f.service.preview(draft, None, None).unwrap_err(),
        "project endpoint is already used by another project"
    );
    let mut draft = primary.clone();
    draft.allow_parallel_writers = true;
    assert_eq!(
        f.service.preview(draft, None, None).unwrap_err(),
        "project allow_parallel_writers=true requires execution_mode=worktree"
    );
    let mut draft = primary;
    draft.opencode_url = "http://user:private-password@127.0.0.1:4109".into();
    let error = f.service.preview(draft, None, None).unwrap_err();
    assert!(error.contains("opencode_url"));
    assert!(!error.contains("private-password"));
    assert!(!error.contains("127.0.0.1"));
}

#[test]
fn new_project_preview_reports_reused_workspace_and_accepts_distinct_bindings() {
    let f = Fixture::new();
    let original = fs::read(&f.service.config).unwrap();
    let mut draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    draft.id = "new_project".into();
    draft.opencode_url = "http://127.0.0.1:4103".into();
    draft.mcp_url = Some("http://127.0.0.1:4203/mcp".into());
    assert_eq!(
        f.service.preview(draft.clone(), None, None).unwrap_err(),
        "project workspace is already used by another project"
    );
    let workspace = f.root.join("new-workspace");
    fs::create_dir(&workspace).unwrap();
    draft.workspace = workspace.to_string_lossy().into();
    let preview = f.service.preview(draft, None, None).unwrap();
    assert_eq!(preview["before"], serde_json::Value::Null);
    assert_eq!(preview["after"]["id"], "new_project");
    assert_eq!(fs::read(&f.service.config).unwrap(), original);
    assert!(!f.service.state.exists());
}

#[test]
fn remove_registration_preserves_files_history_credentials_other_projects_and_backup() {
    let f = Fixture::new();
    let task = f.task(
        "primary",
        "History survives removal",
        "2026-01-01T00:00:00Z",
    );
    let (_, layout) = f.service.project("primary").unwrap();
    fs::write(f.root.join("main/keep.txt"), "workspace contents").unwrap();
    fs::write(f.root.join("password"), "private-credential").unwrap();
    let original = fs::read(&f.service.config).unwrap();
    let preview = f.service.preview_remove("primary").unwrap();
    assert_eq!(preview["before"]["id"], "primary");
    assert!(preview["after"].is_null());
    assert_eq!(fs::read(&f.service.config).unwrap(), original);
    assert_eq!(
        f.service
            .apply(preview["review_id"].as_str().unwrap())
            .unwrap()["removed"],
        true
    );
    assert_eq!(
        f.service
            .projects()
            .unwrap()
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        ["second"]
    );
    assert_eq!(
        fs::read_to_string(f.root.join("main/keep.txt")).unwrap(),
        "workspace contents"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("password")).unwrap(),
        "private-credential"
    );
    assert!(
        layout
            .open_readonly()
            .unwrap()
            .get_task(task.parse().unwrap())
            .unwrap()
            .is_some()
    );
    let backup = fs::read_dir(&f.root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "bak"))
        .unwrap();
    assert_eq!(fs::read(backup).unwrap(), original);
    assert!(
        fs::read_to_string(&f.service.config)
            .unwrap()
            .contains("# keep formatting")
    );
}

#[test]
fn remove_last_registration_cancellation_and_stale_preview_are_safe() {
    let f = Fixture::new();
    assert!(f.service.preview_remove("missing").is_err());
    let preview = f.service.preview_remove("primary").unwrap();
    let id = preview["review_id"].as_str().unwrap();
    f.service.cancel(id);
    assert!(f.service.apply(id).is_err());
    let preview = f.service.preview_remove("primary").unwrap();
    fs::write(
        &f.service.config,
        format!(
            "{}\n# external edit\n",
            fs::read_to_string(&f.service.config).unwrap()
        ),
    )
    .unwrap();
    assert!(
        f.service
            .apply(preview["review_id"].as_str().unwrap())
            .unwrap_err()
            .contains("config changed")
    );
    for project in ["primary", "second"] {
        let preview = f.service.preview_remove(project).unwrap();
        let id = preview["review_id"].as_str().unwrap();
        f.service.apply(id).unwrap();
        assert!(f.service.apply(id).is_err());
    }
    assert!(f.service.projects().unwrap().is_empty());
    assert!(!f.service.state.exists());
}

#[test]
fn removal_refuses_unfinished_tasks_and_busy_linked_controllers() {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let f = Fixture::new();
    let task = f.task("primary", "Unfinished", "2026-01-01T00:00:00Z");
    let (_, layout) = f.service.project("primary").unwrap();
    let storage = layout.open().unwrap();
    storage
        .connection()
        .execute("UPDATE tasks SET status='failed' WHERE task_id=?1", [&task])
        .unwrap();
    let original = fs::read(&f.service.config).unwrap();
    let preview = f.service.preview_remove("primary").unwrap();
    assert!(
        f.service
            .apply(preview["review_id"].as_str().unwrap())
            .unwrap_err()
            .contains("незавершённые задачи")
    );
    storage
        .connection()
        .execute("UPDATE tasks SET status='closed' WHERE task_id=?1", [&task])
        .unwrap();
    let controller = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(layout.project_dir().join("controller.lock"))
        .unwrap();
    nix::fcntl::flock(
        controller.as_raw_fd(),
        nix::fcntl::FlockArg::LockExclusiveNonblock,
    )
    .unwrap();
    let preview = f.service.preview_remove("second").unwrap();
    let id = preview["review_id"].as_str().unwrap();
    let error = f.service.apply(id).unwrap_err();
    assert!(error.contains("primary") && error.contains("контроллер"));
    assert_eq!(fs::read(&f.service.config).unwrap(), original);
    drop(controller);
    f.service.apply(id).unwrap();
}

#[test]
fn removal_refuses_paused_automation_until_stopped() {
    use std::process::Command;
    let f = Fixture::new();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Proof",
            "-c",
            "user.email=proof@example.test",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(f.root.join("main"))
                .status()
                .unwrap()
                .success()
        );
    }
    let (entry, layout) = f.service.project("primary").unwrap();
    let plan = serde_json::json!({"version":1,"goal":"Pause proof","steps":[{"id":"one","task":"Create file","allowed_paths":["file.txt"],"test_commands":["true"],"acceptance_criteria":["File exists"]}],"final_test_commands":["true"],"delivery":"manual"});
    let run = bridge_automation::run::create_run(&entry, &layout, &plan).unwrap();
    let store = bridge_storage::automation::AutomationRunStore::new(layout);
    store
        .save(
            run.document(),
            bridge_storage::automation::RunStatus::Paused,
        )
        .unwrap();
    let preview = f.service.preview_remove("primary").unwrap();
    let id = preview["review_id"].as_str().unwrap();
    assert!(
        f.service
            .apply(id)
            .unwrap_err()
            .contains("автоматический запуск")
    );
    store
        .save(
            run.document(),
            bridge_storage::automation::RunStatus::Stopped,
        )
        .unwrap();
    f.service.apply(id).unwrap();
}

#[test]
fn remote_settings_are_reviewed_persisted_and_disabled_by_default() {
    let f = Fixture::new();
    let mut draft = f
        .service
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.id == "primary")
        .unwrap();
    assert!(draft.remote_execution.is_none());
    let original = fs::read(&f.service.config).unwrap();
    let remote = bridge_config::remote::RemoteExecution {
        host: "192.168.1.22".into(),
        user: "executor".into(),
        port: 22,
        executable: "/opt/bridge/agent-bridge".into(),
        config: "/home/executor/projects.toml".into(),
        state_root: "/home/executor/state".into(),
        project: "proj".into(),
        repository: "git@gitlab.example:owner/repo.git".into(),
    };
    draft.remote_execution = Some(remote.clone());
    let preview = f.service.preview(draft.clone(), None, None).unwrap();
    assert_eq!(fs::read(&f.service.config).unwrap(), original);
    assert_eq!(preview["after"]["remote_execution"]["host"], "192.168.1.22");
    f.service
        .apply(preview["review_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        f.service
            .projects()
            .unwrap()
            .into_iter()
            .find(|p| p.id == "primary")
            .unwrap()
            .remote_execution,
        Some(remote)
    );
    draft.remote_execution = None;
    let preview = f.service.preview(draft, None, None).unwrap();
    f.service
        .apply(preview["review_id"].as_str().unwrap())
        .unwrap();
    assert!(
        !fs::read_to_string(&f.service.config)
            .unwrap()
            .contains("remote_execution")
    );
}

#[test]
fn shutdown_all_is_readonly_for_unused_projects_and_rejects_new_launches() {
    let f = Fixture::new();
    let config = fs::read(&f.service.config).unwrap();
    assert_eq!(f.service.shutdown_all().unwrap()["status"], "stopped");
    assert!(!f.service.state.exists());
    assert_eq!(fs::read(&f.service.config).unwrap(), config);
    assert_eq!(
        f.service.lifecycle("primary", "start").unwrap_err(),
        "application is shutting down"
    );
    assert_eq!(f.service.shutdown_all().unwrap()["status"], "stopped");
}
#[test]
fn shutdown_closes_every_project_and_continues_after_a_foreign_record() {
    use std::{os::unix::fs::PermissionsExt, process::Command};
    let f = Fixture::new();
    let mut children = vec![];
    for id in ["primary", "second"] {
        let (p, l) = f.service.project(id).unwrap();
        l.initialize().unwrap();
        let child = Command::new("sleep").arg("90").spawn().unwrap();
        let raw = fs::read_to_string(format!("/proc/{}/stat", child.id())).unwrap();
        let start = raw
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap();
        let record = serde_json::json!({"pid":child.id(),"start":start,"boot_id":fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),"project_id":if id=="primary" { "foreign" } else { id },"task_id":"00000000-0000-0000-0000-000000000000","checkout":p.workspace(),"kind":"opencode","port":p.opencode_endpoint().port(),"endpoint":p.opencode_endpoint().url()});
        let path = l.project_dir().join("opencode.process.json");
        fs::write(&path, record.to_string()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        children.push(child);
    }
    let report = f.service.shutdown_all().unwrap();
    assert_eq!(report["status"], "incomplete");
    assert_eq!(report["errors"].as_array().unwrap().len(), 1);
    assert!(children[0].try_wait().unwrap().is_none());
    assert!(children[1].try_wait().unwrap().is_some());
    assert!(f.root.join("main").exists());
    assert!(f.root.join("second").exists());
    children[0].kill().unwrap();
    children[0].wait().unwrap();
}

#[test]
fn shutdown_unused_project_with_saved_preferences_does_not_require_setup() {
    let f = Fixture::new();
    f.service
        .save_codex_env("primary", "EXAMPLE=value")
        .unwrap();
    let (_, layout) = f.service.project("primary").unwrap();
    assert!(layout.project_dir().exists());
    assert!(!layout.database().exists());
    assert_eq!(f.service.shutdown_all().unwrap()["status"], "stopped");
    assert!(!layout.database().exists());
    assert_eq!(
        f.service.read_codex_env("primary").unwrap(),
        "EXAMPLE=value"
    );
}
