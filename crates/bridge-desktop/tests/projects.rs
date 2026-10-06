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
fn project_config_edit_still_refuses_its_active_tasks() {
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
