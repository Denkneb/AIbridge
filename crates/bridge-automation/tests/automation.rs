use bridge_automation::lifecycle::{directory, launch_command, supervisor_running};
use bridge_automation::{
    AutomationError as Error,
    plan::validate_plan,
    run::{AutomationLock, check_binding, create_run},
};
use bridge_config::{ProjectEntry, load_config_with_state_root};
use bridge_storage::automation::{RunControl, RunStatus};
use bridge_storage::{CreateTaskInput, RustStateLayout, automation::AutomationRunStore};
use bridge_worker::{WorkerLock, WorkerLockOutcome};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
};

struct Fixture {
    root: PathBuf,
    project: ProjectEntry,
    layout: RustStateLayout,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-auto-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("workspace")).unwrap();
        let path = root.join("projects.toml");
        fs::write(&path, format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4999\"\npassword_file=\"unused\"\nmax_rounds=3\n", json!(root.join("workspace")))).unwrap();
        let config = load_config_with_state_root(&path, &root.join("state")).unwrap();
        let project = config.project("proj").unwrap().clone();
        let layout = RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
        Self {
            root,
            project,
            layout,
        }
    }
    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.project.workspace())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "git fixture failed: {:?}", args);
    }
    fn repo(&self) {
        self.git(&["init", "-q"]);
        fs::write(self.project.workspace().join("module.py"), "print(1)\n").unwrap();
        self.git(&["add", "module.py"]);
        self.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "initial",
        ]);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn corpus() -> Value {
    serde_json::from_str(include_str!(
        "../../../docs/fixtures/runtime-automation-v17.json"
    ))
    .unwrap()
}
fn plan() -> Value {
    corpus()["base_plan"].clone()
}
fn replace(value: &mut Value, path: &str, replacement: Value) {
    let mut target = value;
    let parts = path.split('.').collect::<Vec<_>>();
    for part in &parts[..parts.len() - 1] {
        target = if target.is_array() {
            &mut target[part.parse::<usize>().unwrap()]
        } else {
            &mut target[*part]
        };
    }
    target[parts[parts.len() - 1]] = replacement;
}

#[test]
fn pinned_plan_corpus_and_defaults_without_state_writes() {
    let fixture = Fixture::new();
    let corpus = corpus();
    let mut count = 0;
    for case in corpus["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["operation"] == "plan")
    {
        let mut input = plan();
        for (path, value) in case["changes"].as_object().unwrap() {
            replace(&mut input, path, value.clone());
        }
        let result = validate_plan(&fixture.project, &input);
        if case["expect"].get("error_contains").is_some() {
            assert!(result.is_err(), "{}", case["id"]);
            assert!(
                create_run(&fixture.project, &fixture.layout, &input).is_err(),
                "{}",
                case["id"]
            );
        } else {
            let approved = result.unwrap_or_else(|e| panic!("{}: {e}", case["id"]));
            let mut output = serde_json::to_value(&approved).unwrap();
            output["step_order"] =
                json!(approved.steps().iter().map(|s| &s.id).collect::<Vec<_>>());
            for (field, value) in case["expect"]["values"].as_object().unwrap() {
                assert_eq!(&output[field], value, "{}: {field}", case["id"]);
            }
        }
        assert!(!fixture.layout.state_root().exists());
        count += 1;
    }
    assert_eq!(count, 30);
}

#[test]
fn strict_shapes_limits_and_symlink_scope_escape() {
    let fixture = Fixture::new();
    let mut cases = vec![json!([]), json!(null)];
    for (field, value) in [
        ("max_seconds", json!(null)),
        ("delivery", json!(null)),
        ("version", json!(1.0)),
        ("max_seconds", json!(-1)),
        ("max_revisions", json!(1.2)),
        ("codex_model", json!("x".repeat(201))),
    ] {
        let mut raw = plan();
        raw[field] = value;
        cases.push(raw);
    }
    for (field, value) in [
        ("depends_on", json!(["consumer", "consumer"])),
        ("depends_on", json!(null)),
        ("allowed_paths", json!([true])),
        ("test_commands", json!(["cargo test; git push"])),
        ("id", json!("__final__")),
        ("id", json!("x".repeat(65))),
        ("task", json!("x".repeat(60001))),
        ("acceptance_criteria", json!(vec!["x"; 201])),
    ] {
        let mut raw = plan();
        raw["steps"][0][field] = value;
        cases.push(raw);
    }
    let mut raw = plan();
    raw["steps"] = json!(vec![raw["steps"][0].clone(); 101]);
    cases.push(raw);
    symlink(&fixture.root, fixture.project.workspace().join("escape")).unwrap();
    let mut raw = plan();
    raw["steps"][0]["allowed_paths"] = json!(["escape/outside"]);
    cases.push(raw);
    for raw in cases {
        assert!(validate_plan(&fixture.project, &raw).is_err());
    }
    assert!(!fixture.layout.state_root().exists());
}

#[test]
fn creation_persists_final_step_and_rejects_second_run() {
    let fixture = Fixture::new();
    fixture.repo();
    let run = create_run(&fixture.project, &fixture.layout, &plan()).unwrap();
    check_binding(&fixture.project, &fixture.layout, &run).unwrap();
    let saved = AutomationRunStore::new(fixture.layout.clone())
        .load(Some(run.id()))
        .unwrap();
    assert_eq!(saved.document(), run.document());
    assert_eq!(saved.document()["steps"][2]["step"]["id"], "__final__");
    assert_eq!(
        saved.document()["steps"][2]["step"]["allowed_paths"],
        json!(["consumer.py", "module.py"])
    );
    assert_eq!(
        saved.document()["steps"][2]["step"]["test_commands"],
        plan()["final_test_commands"]
    );
    assert_eq!(
        create_run(&fixture.project, &fixture.layout, &plan()).err(),
        Some(Error::Busy)
    );
    assert_eq!(
        fs::metadata(fixture.layout.project_dir().join("automation.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn config_content_head_and_index_drift_are_refused() {
    let fixture = Fixture::new();
    fixture.repo();
    let run = create_run(&fixture.project, &fixture.layout, &plan()).unwrap();
    let config_bytes = fs::read(fixture.project.source_path()).unwrap();
    fs::write(
        fixture.project.source_path(),
        [config_bytes.clone(), b"\n# drift".to_vec()].concat(),
    )
    .unwrap();
    assert_eq!(
        check_binding(&fixture.project, &fixture.layout, &run),
        Err(Error::Binding)
    );
    fs::write(fixture.project.source_path(), config_bytes).unwrap();
    fs::write(fixture.project.workspace().join("module.py"), "changed\n").unwrap();
    assert_eq!(
        check_binding(&fixture.project, &fixture.layout, &run),
        Err(Error::Binding)
    );
    fixture.git(&["add", "module.py"]);
    assert_eq!(
        check_binding(&fixture.project, &fixture.layout, &run),
        Err(Error::Binding)
    );
    fixture.git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "changed",
    ]);
    assert_eq!(
        check_binding(&fixture.project, &fixture.layout, &run),
        Err(Error::Binding)
    );
}

#[test]
fn dirty_unsupported_and_unfinished_projects_create_no_run() {
    for kind in ["dirty", "unsupported", "unfinished"] {
        let fixture = Fixture::new();
        fixture.repo();
        fixture.layout.initialize().unwrap();
        let expected = if kind == "unfinished" {
            fixture
                .layout
                .open()
                .unwrap()
                .create_task(CreateTaskInput {
                    task_id: "11111111-1111-4111-8111-111111111111".parse().unwrap(),
                    project_id: fixture.project.id().clone(),
                    workspace: fixture.project.workspace().to_str().unwrap().into(),
                    task: "unfinished".into(),
                    request_id: "req".into(),
                    payload_hash: "hash".into(),
                    base_head: None,
                    allowed_paths: vec!["module.py".into()],
                    test_commands: vec!["cargo test".into()],
                    snapshot: None,
                })
                .unwrap();
            Error::UnfinishedTasks
        } else {
            fs::write(
                fixture.project.workspace().join(if kind == "dirty" {
                    "new.py"
                } else {
                    ".gitmodules"
                }),
                "fixture",
            )
            .unwrap();
            if kind == "unsupported" {
                fixture.git(&["add", ".gitmodules"]);
                fixture.git(&[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-qm",
                    "submodule",
                ]);
            }
            Error::Repository
        };
        assert_eq!(
            create_run(&fixture.project, &fixture.layout, &plan()).err(),
            Some(expected)
        );
        assert!(
            AutomationRunStore::new(fixture.layout.clone())
                .load(None)
                .is_err()
        );
    }
}

#[test]
fn locks_block_creation_and_symlink_lock_does_not_touch_target() {
    let fixture = Fixture::new();
    fixture.repo();
    fixture.layout.initialize().unwrap();
    let automatic = AutomationLock::acquire(&fixture.layout).unwrap();
    assert_eq!(
        create_run(&fixture.project, &fixture.layout, &plan()).err(),
        Some(Error::Busy)
    );
    drop(automatic);
    let WorkerLockOutcome::Acquired(admission) =
        WorkerLock::try_acquire_admission(&fixture.layout).unwrap()
    else {
        panic!("busy")
    };
    assert_eq!(
        create_run(&fixture.project, &fixture.layout, &plan()).err(),
        Some(Error::Busy)
    );
    drop(admission);
    let target = fixture.root.join("sentinel");
    fs::write(&target, "unchanged").unwrap();
    fs::remove_file(fixture.layout.project_dir().join("automation.lock")).unwrap();
    symlink(
        &target,
        fixture.layout.project_dir().join("automation.lock"),
    )
    .unwrap();
    assert_eq!(
        create_run(&fixture.project, &fixture.layout, &plan()).err(),
        Some(Error::State)
    );
    assert_eq!(fs::read_to_string(target).unwrap(), "unchanged");
}

#[test]
fn concurrent_creation_allows_one_unfinished_run() {
    let fixture = Fixture::new();
    fixture.repo();
    fixture.layout.initialize().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let barrier = barrier.clone();
            let project = fixture.project.clone();
            let layout = fixture.layout.clone();
            std::thread::spawn(move || {
                barrier.wait();
                create_run(&project, &layout, &plan())
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| r.as_ref().err() == Some(&Error::Busy))
            .count(),
        1
    );
}

#[test]
fn stale_config_wrong_project_and_unowned_state_are_refused() {
    let fixture = Fixture::new();
    let content = fs::read_to_string(fixture.project.source_path()).unwrap();
    fs::write(
        fixture.project.source_path(),
        content.replace("max_rounds=3", "max_rounds=4"),
    )
    .unwrap();
    assert_eq!(
        create_run(&fixture.project, &fixture.layout, &plan()).err(),
        Some(Error::Binding)
    );
    assert!(!fixture.layout.state_root().exists());
    fs::write(fixture.project.source_path(), content).unwrap();
    let other =
        RustStateLayout::new(fixture.root.join("other-state"), "other".parse().unwrap()).unwrap();
    assert_eq!(
        create_run(&fixture.project, &other, &plan()).err(),
        Some(Error::Binding)
    );
    assert!(!other.state_root().exists());
    assert!(AutomationLock::acquire(&fixture.layout).is_err());
    assert!(!fixture.layout.state_root().exists());
    fs::create_dir_all(fixture.layout.project_dir()).unwrap();
    fs::write(fixture.layout.database(), "foreign-state-sentinel").unwrap();
    assert_eq!(
        create_run(&fixture.project, &fixture.layout, &plan()).err(),
        Some(Error::State)
    );
    assert_eq!(
        fs::read_to_string(fixture.layout.database()).unwrap(),
        "foreign-state-sentinel"
    );
    assert!(
        !fixture
            .layout
            .project_dir()
            .join("automation.lock")
            .exists()
    );
}

#[test]
fn linked_main_and_nested_repository_scopes_are_refused() {
    let fixture = Fixture::new();
    fixture.repo();
    let linked = fixture.root.join("linked");
    fixture.git(&["worktree", "add", "--detach", linked.to_str().unwrap()]);
    let path = fixture.root.join("linked.toml");
    fs::write(&path, format!("[projects.linked]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4998\"\npassword_file=\"unused\"\nmax_rounds=3\n", json!(linked))).unwrap();
    let config = load_config_with_state_root(&path, fixture.layout.state_root()).unwrap();
    let project = config.project("linked").unwrap();
    let layout = RustStateLayout::new(fixture.layout.state_root(), project.id().clone()).unwrap();
    assert_eq!(
        create_run(project, &layout, &plan()).err(),
        Some(Error::Repository)
    );
    let nested = fixture.project.workspace().join("nested");
    fs::create_dir(&nested).unwrap();
    fixture.git(&["init", "-q", "nested"]);
    fs::write(fixture.project.workspace().join(".gitignore"), "nested/\n").unwrap();
    fixture.git(&["add", ".gitignore"]);
    fixture.git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "ignore nested fixture",
    ]);
    let mut raw = plan();
    raw["steps"][0]["allowed_paths"] = json!(["nested/new.py"]);
    assert_eq!(
        create_run(&fixture.project, &fixture.layout, &raw).err(),
        Some(Error::Scope)
    );
}

#[test]
fn binding_inspection_preserves_run_and_automation_lock() {
    let fixture = Fixture::new();
    fixture.repo();
    let run = create_run(&fixture.project, &fixture.layout, &plan()).unwrap();
    let file = fixture.layout.project_dir().join("automation.lock");
    fs::write(&file, "lock-content-sentinel").unwrap();
    let store = AutomationRunStore::new(fixture.layout.clone());
    let before = store.load(Some(run.id())).unwrap();
    for _ in 0..3 {
        check_binding(&fixture.project, &fixture.layout, &run).unwrap();
    }
    let after = store.load(Some(run.id())).unwrap();
    assert_eq!(before.document(), after.document());
    assert_eq!(before.updated_at(), after.updated_at());
    assert_eq!(fs::read_to_string(file).unwrap(), "lock-content-sentinel");
}

fn stop_fixture(pid: i32) {
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
    if let Ok(fd) = pidfd_open(Pid::from_raw(pid).unwrap(), PidfdFlags::empty()) {
        let _ = pidfd_send_signal(&fd, Signal::KILL);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::path::Path::new(&format!("/proc/{pid}")).exists()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
#[test]
fn detached_launch_lease_duplicate_refusal_and_stale_resume() {
    let fixture = Fixture::new();
    fixture.repo();
    let run = create_run(&fixture.project, &fixture.layout, &plan()).unwrap();
    let store = AutomationRunStore::new(fixture.layout.clone());
    let script = "import time;time.sleep(30)";
    let launch = |resume| {
        launch_command(
            &fixture.layout,
            &fixture.project,
            run.id(),
            std::path::Path::new("/usr/bin/python3"),
            vec!["-c".into(), script.into()],
            resume,
        )
    };
    let result = (0..100)
        .find_map(|_| match launch(false) {
            Ok(v) => Some(v),
            Err(Error::Busy) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                None
            }
            Err(e) => panic!("{e}"),
        })
        .expect("startup lock becomes free");
    let pid = result["pid"].as_i64().unwrap() as i32;
    assert!(supervisor_running(&fixture.layout, &fixture.project, run.id()).unwrap());
    store.set_control(run.id(), RunControl::Pause).unwrap();
    let before = store.load(Some(run.id())).unwrap();
    assert_eq!(launch(true).err(), Some(Error::Busy));
    let after = store.load(Some(run.id())).unwrap();
    assert_eq!(before.control(), after.control());
    assert_eq!(before.updated_at(), after.updated_at());
    stop_fixture(pid);
    assert!(!supervisor_running(&fixture.layout, &fixture.project, run.id()).unwrap());
    store.save(run.document(), RunStatus::Blocked).unwrap();
    assert_eq!(launch(false).err(), Some(Error::State));
    let resumed = launch(true).unwrap();
    stop_fixture(resumed["pid"].as_i64().unwrap() as i32);
    assert_eq!(
        store.load(Some(run.id())).unwrap().control(),
        RunControl::Run
    );
    assert_eq!(
        store.load(Some(run.id())).unwrap().status(),
        RunStatus::Running
    );
}
#[test]
fn failed_spawn_terminal_and_foreign_record_preserve_run_state() {
    let fixture = Fixture::new();
    fixture.repo();
    let run = create_run(&fixture.project, &fixture.layout, &plan()).unwrap();
    let store = AutomationRunStore::new(fixture.layout.clone());
    store.save(run.document(), RunStatus::Blocked).unwrap();
    store.set_control(run.id(), RunControl::Pause).unwrap();
    let executable = fixture.root.join("not-executable");
    fs::write(&executable, "fixture").unwrap();
    let before = store.load(Some(run.id())).unwrap();
    assert_eq!(
        launch_command(
            &fixture.layout,
            &fixture.project,
            run.id(),
            &executable,
            vec![],
            true
        )
        .err(),
        Some(Error::State)
    );
    let after = store.load(Some(run.id())).unwrap();
    assert_eq!(before.document(), after.document());
    assert_eq!(before.control(), after.control());
    let dir = directory(&fixture.layout, run.id());
    let own = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{own}/stat")).unwrap();
    let start = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap();
    fs::write(dir.join("process.json"),json!({"pid":own,"start":start,"boot_id":fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),"project_id":"foreign","workspace":fixture.project.workspace(),"run_id":run.id().to_string(),"kind":"automation"}).to_string()).unwrap();
    assert_eq!(
        supervisor_running(&fixture.layout, &fixture.project, run.id()),
        Err(Error::Binding)
    );
    store.save(run.document(), RunStatus::Stopped).unwrap();
    assert_eq!(
        launch_command(
            &fixture.layout,
            &fixture.project,
            run.id(),
            std::path::Path::new("/usr/bin/python3"),
            vec![],
            true
        )
        .err(),
        Some(Error::State)
    );
}

fn submit_intent(
    f: &Fixture,
) -> (
    bridge_storage::automation::AutomationRun,
    Value,
    bridge_mcp::McpServer,
) {
    let run = create_run(&f.project, &f.layout, &plan()).unwrap();
    let mut doc = run.document().clone();
    doc["steps"][0]["prepared_task"] = json!("Approved implementation task");
    doc["steps"][0]["phase"] = json!("submit");
    let run = AutomationRunStore::new(f.layout.clone())
        .save(&doc, RunStatus::Running)
        .unwrap();
    let step = &doc["steps"][0]["step"];
    let args = json!({"request_id":format!("auto:{}:{}:submit",run.id(),step["id"].as_str().unwrap()),"task":doc["steps"][0]["prepared_task"],"allowed_paths":step["allowed_paths"],"test_commands":step["test_commands"],"profile":step["profile"],"workflow_id":run.id().to_string()});
    let view = f.project.automation_view(3).unwrap();
    let server = bridge_mcp::McpServer::internal(
        view.clone(),
        f.layout.clone(),
        Arc::new(|_| Ok(())),
        vec![view],
    )
    .unwrap();
    (run, args, server)
}
#[test]
fn internal_submission_freezes_provenance_and_public_wrapper_cannot_mint_it() {
    let f = Fixture::new();
    f.repo();
    let (run, args, server) = submit_intent(&f);
    let result = server.call_automation("submit_task", &args, run.id());
    assert!(result.get("error").is_none(), "{result}");
    let id: bridge_domain::TaskId = result["task_id"].as_str().unwrap().parse().unwrap();
    let task = f
        .layout
        .open_readonly()
        .unwrap()
        .get_task(id)
        .unwrap()
        .unwrap();
    assert_eq!(
        task.snapshot.as_ref().unwrap()["automation_run_id"],
        run.id().to_string()
    );
    assert_eq!(task.delivery_mode, bridge_domain::DeliveryMode::Manual);
    assert_eq!(
        server.call_automation("submit_task", &args, run.id())["task_id"],
        result["task_id"]
    );
    let mut invalid = args.clone();
    invalid["allowed_paths"] = json!(["module.py", "outside.py"]);
    assert_eq!(
        server.call_automation("submit_task", &invalid, run.id())["error"],
        "automation_approved_step_mismatch"
    );
    let mut public = args.clone();
    public["automation_run_id"] = json!(run.id().to_string());
    let response=bridge_mcp::protocol::Protocol::stateless_http().handle(&server,json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"submit_task","arguments":public}})).unwrap();
    assert_eq!(response["error"]["code"], -32602);
    let revision = json!({"task_id":id,"request_id":"foreign","findings":"defect"});
    let response=bridge_mcp::protocol::Protocol::stateless_http().handle(&server,json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"request_changes","arguments":revision}})).unwrap();
    assert!(response.to_string().contains("automation_managed"));
}
#[test]
fn internal_submission_requires_approved_intent_and_running_control() {
    let f = Fixture::new();
    f.repo();
    let (run, args, server) = submit_intent(&f);
    for key in ["task", "test_commands", "workflow_id", "profile"] {
        let mut invalid = args.clone();
        invalid[key] = json!("unapproved");
        assert!(
            server
                .call_automation("submit_task", &invalid, run.id())
                .get("error")
                .is_some(),
            "{key}"
        );
    }
    let store = AutomationRunStore::new(f.layout.clone());
    store.set_control(run.id(), RunControl::Pause).unwrap();
    assert_eq!(
        server.call_automation("submit_task", &args, run.id())["error"],
        "automation_not_running"
    );
    assert!(
        f.layout
            .open_readonly()
            .unwrap()
            .list_tasks(f.project.id(), false, 100, 0)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn managed_revision_identity_phase_and_pending_intent_are_pinned() {
    let f = Fixture::new();
    f.repo();
    let (run, args, server) = submit_intent(&f);
    let result = server.call_automation("submit_task", &args, run.id());
    let id: bridge_domain::TaskId = result["task_id"].as_str().unwrap().parse().unwrap();
    let task = f
        .layout
        .open_readonly()
        .unwrap()
        .get_task(id)
        .unwrap()
        .unwrap();
    let store = AutomationRunStore::new(f.layout.clone());
    let mut doc = run.document().clone();
    doc["steps"][0]["task_id"] = json!(id);
    doc["steps"][0]["phase"] = json!("revise");
    doc["steps"][0]["pending_revision"] = json!({"request_id":format!("auto:{}:fix:rev:1",run.id()),"findings":"Approved correction"});
    store.save(&doc, RunStatus::Running).unwrap();
    let revision = json!({"task_id":id,"request_id":doc["steps"][0]["pending_revision"]["request_id"],"findings":"Approved correction"});
    assert!(
        bridge_worker::automation::revision_authorized(
            &f.layout,
            &f.project,
            &task,
            Some(run.id()),
            &revision
        )
        .is_ok()
    );
    assert_eq!(
        bridge_worker::automation::revision_authorized(
            &f.layout, &f.project, &task, None, &revision
        ),
        Err("automation_managed")
    );
    let foreign = uuid::Uuid::new_v4().to_string().parse().unwrap();
    assert_eq!(
        bridge_worker::automation::revision_authorized(
            &f.layout,
            &f.project,
            &task,
            Some(foreign),
            &revision
        ),
        Err("automation_managed")
    );
    let mut modified = revision.clone();
    modified["findings"] = json!("Unapproved change");
    assert_eq!(
        bridge_worker::automation::revision_authorized(
            &f.layout,
            &f.project,
            &task,
            Some(run.id()),
            &modified
        ),
        Err("automation_revision_mismatch")
    );
    store.set_control(run.id(), RunControl::Stop).unwrap();
    assert_eq!(
        bridge_worker::automation::revision_authorized(
            &f.layout,
            &f.project,
            &task,
            Some(run.id()),
            &revision
        ),
        Err("automation_not_running")
    );
}
