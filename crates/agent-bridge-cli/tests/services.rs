use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output},
};
struct Fixture {
    root: PathBuf,
    config: PathBuf,
    blocked: Option<TcpListener>,
}
fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
impl Fixture {
    fn new(second: bool) -> Self {
        let root = std::env::temp_dir().join(format!("bridge-services-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("main")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        let config = root.join("projects.toml");
        let mut text = format!(
            "[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:{}\"\nmcp_url=\"http://127.0.0.1:{}/mcp\"\npassword_file={}\nmcp_token_file={}\nmax_rounds=3\n",
            json!(root.join("main")),
            port(),
            port(),
            json!(root.join("password")),
            json!(root.join("token"))
        );
        let blocked = if second {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            fs::create_dir(root.join("second")).unwrap();
            text.push_str(&format!("[projects.second]\nworkspace={}\nopencode_url=\"http://127.0.0.1:{}\"\nmcp_url=\"http://127.0.0.1:{}/mcp\"\npassword_file={}\nmcp_token_file={}\nmax_rounds=3\n",json!(root.join("second")),port(),l.local_addr().unwrap().port(),json!(root.join("second-password")),json!(root.join("second-token"))));
            Some(l)
        } else {
            None
        };
        fs::write(&config, text).unwrap();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixture = base.join("../bridge-automation/tests/fixtures/opencode.py");
        let doc = base.join("../bridge-runtime/tests/fixtures/openapi.json");
        let program = format!(
            "#!/usr/bin/python3\nimport sys,runpy,os,json\nif sys.argv[1]=='attach':\n open(os.environ['PROOF_ATTACH_OUTPUT'],'w').write(json.dumps({{'argv':sys.argv[1:],'cwd':os.getcwd(),'auth':bool(os.environ.get('OPENCODE_SERVER_PASSWORD'))}}))\n sys.exit(7)\nsys.argv=[{},'--doc',{}]+sys.argv[1:]\nrunpy.run_path({},run_name='__main__')\n",
            json!(fixture),
            json!(doc),
            json!(fixture)
        );
        fs::write(root.join("bin/opencode"), program).unwrap();
        fs::set_permissions(root.join("bin/opencode"), fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            root,
            config,
            blocked,
        }
    }
    fn cli(&self, cmd: &str, all: bool, flags: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        c.arg(cmd);
        if all {
            c.arg("--all");
        } else {
            c.args(["--project", "proj"]);
        }
        c.arg("--config")
            .arg(&self.config)
            .arg("--state-root")
            .arg(self.root.join("state"))
            .args(flags)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("PROOF_ATTACH_OUTPUT", self.root.join("attach.json"));
        c.output().unwrap()
    }
    fn record(&self, kind: &str) -> PathBuf {
        self.root
            .join("state/proj")
            .join(format!("{kind}.process.json"))
    }
    fn ok(&self, cmd: &str, all: bool, flags: &[&str]) -> Value {
        let o = self.cli(cmd, all, flags);
        assert!(
            o.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        serde_json::from_slice(&o.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.cli("stop", true, &[]);
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn readiness_reports_live_timeout_auth_workspace_and_api_without_replacing_server() {
    let f = Fixture::new(false);
    f.ok("setup", false, &[]);
    assert_eq!(f.cli("console", false, &[]).status.code(), Some(7));
    let original = fs::read(f.record("opencode")).unwrap();
    for (mode, code) in [
        ("timeout", "opencode_server_unresponsive"),
        ("auth", "opencode_authentication_failed"),
        ("workspace", "opencode_workspace_mismatch"),
        ("api", "opencode_api_incompatible"),
        ("unhealthy", "opencode_server_unhealthy"),
        ("http", "opencode_http_error"),
    ] {
        fs::write(f.root.join("runtime/proof-readiness-mode"), mode).unwrap();
        let result = f.cli("console", false, &[]);
        assert_eq!(result.status.code(), Some(1), "{mode}");
        let error = String::from_utf8(result.stderr).unwrap();
        assert!(error.contains(code), "{mode}: {error}");
        assert!(!error.contains("fixture-response-secret"));
        let doctor = f.cli("doctor", false, &["--json"]);
        let report: Value = serde_json::from_slice(&doctor.stdout).unwrap();
        let server = &report["projects"][0]["servers"]["opencode"];
        assert_eq!(server["ready"], false);
        assert_eq!(server["managed"], true);
        assert_eq!(server["error"], code);
        assert!(server["message"].as_str().is_some_and(|m| !m.is_empty()));
        if mode == "http" {
            assert_eq!(server["http_status"], 500);
        }
        assert_eq!(fs::read(f.record("opencode")).unwrap(), original);
    }
    fs::remove_file(f.root.join("runtime/proof-readiness-mode")).unwrap();
    assert_eq!(f.cli("console", false, &[]).status.code(), Some(7));
    assert_eq!(fs::read(f.record("opencode")).unwrap(), original);
    let report: Value =
        serde_json::from_slice(&f.cli("doctor", false, &["--json"]).stdout).unwrap();
    let server = &report["projects"][0]["servers"]["opencode"];
    assert_eq!(server["ready"], true);
    assert!(server.get("error").is_none());
}

#[test]
fn setup_start_readonly_status_doctor_attach_stop_and_foreign_record_guards() {
    let f = Fixture::new(false);
    assert_eq!(f.cli("doctor", false, &["--json"]).status.code(), Some(1));
    assert!(!f.root.join("state").exists());
    f.ok("setup", false, &[]);
    let pwd = fs::read(f.root.join("password")).unwrap();
    assert_eq!(
        fs::metadata(f.root.join("password"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    f.ok("setup", false, &[]);
    assert_eq!(fs::read(f.root.join("password")).unwrap(), pwd);
    f.ok("start", false, &[]);
    assert!(!f.record("opencode").exists());
    let mcp = fs::read(f.record("mcp")).unwrap();
    f.ok("start", false, &[]);
    assert_eq!(fs::read(f.record("mcp")).unwrap(), mcp);
    let report = f.ok("status", false, &["--json"]);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["projects"][0]["servers"]["opencode"]["idle"], true);
    assert_eq!(
        report["projects"][0]["servers"]["opencode"]["managed"],
        false
    );
    assert_eq!(report["projects"][0]["servers"]["mcp"]["ready"], true);
    assert_eq!(report["projects"][0]["servers"]["mcp"]["managed"], true);
    f.ok("doctor", false, &["--json"]);
    assert_eq!(f.cli("console", false, &[]).status.code(), Some(7));
    let capture: Value =
        serde_json::from_slice(&fs::read(f.root.join("attach.json")).unwrap()).unwrap();
    assert_eq!(capture["cwd"], json!(f.root.join("main")));
    assert_eq!(capture["auth"], true);
    assert_eq!(capture["argv"][0], "attach");
    let (project, layout) = project_layout(&f);
    let _lease = bridge_runtime::project::OpenCodeLease::acquire(&project, &layout).unwrap();
    let old = fs::read(f.record("opencode")).unwrap();
    let mut record: Value = serde_json::from_slice(&old).unwrap();
    record["project_id"] = json!("foreign");
    fs::write(f.record("opencode"), record.to_string()).unwrap();
    drop(_lease);
    assert!(bridge_runtime::project::stop_idle_opencode(&project, &layout).is_err());
    assert_eq!(
        fs::read_to_string(f.record("opencode")).unwrap(),
        record.to_string()
    );
    assert!(!f.cli("stop", false, &[]).status.success());
    assert!(f.record("opencode").exists());
    assert_eq!(f.cli("status", false, &["--json"]).status.code(), Some(1));
    fs::write(f.record("opencode"), old).unwrap();
    f.ok("stop", false, &[]);
    f.ok("stop", false, &[]);
    assert!(!f.record("opencode").exists());
    assert_eq!(f.cli("status", false, &["--json"]).status.code(), Some(1));
}
#[test]
fn multi_project_failure_rolls_back_new_services_and_preserves_existing_ones() {
    let f = Fixture::new(true);
    assert!(f.blocked.is_some());
    f.ok("setup", true, &[]);
    assert!(!f.cli("start", true, &[]).status.success());
    assert!(!f.record("opencode").exists());
    assert!(!f.record("mcp").exists());
    f.ok("start", false, &[]);
    let before = fs::read(f.record("mcp")).unwrap();
    assert!(!f.cli("start", true, &[]).status.success());
    assert_eq!(fs::read(f.record("mcp")).unwrap(), before);
    assert!(!f.record("opencode").exists());
    f.ok("status", false, &["--json"]);
    let all = f.cli("status", true, &["--json"]);
    assert_eq!(all.status.code(), Some(1));
    let reports: Value = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(reports["projects"].as_array().unwrap().len(), 2);
    assert_eq!(reports["projects"][0]["ready"], true);
    assert_eq!(reports["projects"][1]["ready"], false);
    f.ok("stop", true, &[]);
}

#[test]
fn setup_refuses_symlink_credentials_and_foreign_state_without_writes() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new(false);
    let target = f.root.join("untouched");
    fs::write(&target, "unchanged").unwrap();
    symlink(&target, f.root.join("password")).unwrap();
    assert!(!f.cli("setup", false, &[]).status.success());
    assert!(!f.root.join("state").exists());
    assert!(!f.root.join("token").exists());
    assert_eq!(fs::read_to_string(&target).unwrap(), "unchanged");
    fs::remove_file(f.root.join("password")).unwrap();
    fs::create_dir_all(f.root.join("state/proj")).unwrap();
    fs::write(f.root.join("state/proj/foreign"), "unchanged").unwrap();
    assert!(!f.cli("setup", false, &[]).status.success());
    assert!(!f.root.join("password").exists());
    assert!(!f.root.join("token").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("state/proj/foreign")).unwrap(),
        "unchanged"
    );
    assert!(fs::read_dir(f.root.join("main")).unwrap().next().is_none());
}

fn project_layout(f: &Fixture) -> (bridge_config::ProjectEntry, bridge_storage::RustStateLayout) {
    let project = bridge_config::load_config(&f.config)
        .unwrap()
        .project("proj")
        .unwrap()
        .clone();
    let layout =
        bridge_storage::RustStateLayout::new(f.root.join("state"), project.id().clone()).unwrap();
    (project, layout)
}
#[test]
fn idle_shutdown_preserves_tasks_and_mcp_and_fences_consoles_and_submission() {
    use bridge_domain::{TaskId, TaskStatus};
    use bridge_runtime::project::{OpenCodeLease, stop_idle_opencode};
    let f = Fixture::new(false);
    f.ok("setup", false, &[]);
    f.ok("start", false, &[]);
    let (project, layout) = project_layout(&f);
    // A console wakes the main server. Keep a lease just like a running console.
    let lease = OpenCodeLease::acquire(&project, &layout).unwrap();
    assert_eq!(f.cli("console", false, &[]).status.code(), Some(7));
    let old = fs::read(f.record("opencode")).unwrap();
    assert!(stop_idle_opencode(&project, &layout).is_err());
    let task: TaskId = "11111111-1111-4111-8111-111111111111".parse().unwrap();
    let mut storage = layout.open().unwrap();
    storage
        .create_task(bridge_storage::CreateTaskInput {
            task_id: task,
            project_id: project.id().clone(),
            workspace: project.workspace().to_str().unwrap().into(),
            task: "fixture".into(),
            request_id: "idle-test".into(),
            payload_hash: "hash".into(),
            base_head: None,
            allowed_paths: vec!["**".into()],
            test_commands: vec![],
            snapshot: None,
        })
        .unwrap();
    drop(lease);
    for status in TaskStatus::ALL.into_iter().filter(|s| s.is_active()) {
        storage
            .connection()
            .execute(
                "UPDATE tasks SET status=?1 WHERE task_id=?2",
                [status.as_str(), &task.to_string()],
            )
            .unwrap();
        assert!(
            !stop_idle_opencode(&project, &layout).unwrap(),
            "{status:?}"
        );
        assert_eq!(fs::read(f.record("opencode")).unwrap(), old);
    }
    storage
        .connection()
        .execute(
            "UPDATE tasks SET status='accepted' WHERE task_id=?1",
            [task.to_string()],
        )
        .unwrap();
    let admission = match bridge_worker::WorkerLock::try_acquire_admission(&layout).unwrap() {
        bridge_worker::WorkerLockOutcome::Acquired(g) => g,
        _ => panic!("admission busy"),
    };
    assert!(stop_idle_opencode(&project, &layout).is_err());
    assert_eq!(fs::read(f.record("opencode")).unwrap(), old);
    drop(admission);
    let worker = match bridge_worker::WorkerLock::try_acquire(&layout).unwrap() {
        bridge_worker::WorkerLockOutcome::Acquired(g) => g,
        _ => panic!("worker busy"),
    };
    assert!(stop_idle_opencode(&project, &layout).is_err());
    assert_eq!(fs::read(f.record("opencode")).unwrap(), old);
    drop(worker);
    // Production MCP loop performs the shutdown without another user action.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while f.record("opencode").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(!f.record("opencode").exists());
    assert_eq!(
        storage.get_task(task).unwrap().unwrap().status,
        TaskStatus::Accepted
    );
    let report = f.ok("doctor", false, &["--json"]);
    assert_eq!(report["projects"][0]["servers"]["mcp"]["ready"], true);
    assert_eq!(report["projects"][0]["servers"]["opencode"]["idle"], true);
    assert_eq!(f.cli("console", false, &[]).status.code(), Some(7));
    assert!(f.record("opencode").exists());
}

fn tool(f: &Fixture, name: &str, args: Value) -> Value {
    use std::io::{Read, Write};
    let (project, _) = project_layout(f);
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", project.mcp_endpoint().unwrap().port()))
            .unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(25)))
        .unwrap();
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}).to_string();
    let token = fs::read_to_string(f.root.join("token")).unwrap();
    write!(stream, "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", project.mcp_endpoint().unwrap().port(), token.trim(), body.len(), body).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let value: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(value.get("error").is_none(), "{value}");
    assert_ne!(value["result"]["isError"], true, "{value}");
    value["result"]["structuredContent"].clone()
}

#[test]
fn direct_submission_wakes_idle_server_executes_and_returns_to_idle_after_acceptance() {
    let f = Fixture::new(false);
    let workspace = f.root.join("main");
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );
    }
    f.ok("setup", false, &[]);
    f.ok("start", false, &[]);
    assert!(!f.record("opencode").exists());
    let submitted = tool(
        &f,
        "submit_task",
        json!({"request_id":"wake-task", "task":"PROOF:left", "allowed_paths":["left.txt"], "test_commands":[]}),
    );
    let id = submitted["task_id"].as_str().expect("submitted task");
    assert!(f.record("opencode").exists());
    let (_, layout) = project_layout(&f);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let saved = layout
            .open_readonly()
            .unwrap()
            .get_task(id.parse().unwrap())
            .unwrap()
            .unwrap();
        if saved.status == bridge_domain::TaskStatus::AwaitingReview {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker: {:?}",
            saved.status
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(
        fs::read_to_string(workspace.join("left.txt")).unwrap(),
        "left\n"
    );
    let accepted = tool(&f, "accept_task", json!({"task_id":id}));
    assert_eq!(accepted["status"], "accepted");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while f.record("opencode").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(!f.record("opencode").exists());
    assert_eq!(
        layout
            .open_readonly()
            .unwrap()
            .get_task(id.parse().unwrap())
            .unwrap()
            .unwrap()
            .status,
        bridge_domain::TaskStatus::Accepted
    );
    assert_eq!(
        f.ok("doctor", false, &["--json"])["projects"][0]["ready"],
        true
    );
}

#[test]
fn desktop_recovers_manual_continuation_after_401_runs_verifier_without_resending() {
    use std::io::{Read, Write};
    let f = Fixture::new(false);
    let workspace = f.root.join("main");
    fs::write(
        workspace.join("check.py"),
        "from pathlib import Path\nassert Path('left.txt').read_text() == 'left\\n'\n",
    )
    .unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "check.py"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );
    }
    f.ok("setup", false, &[]);
    f.ok("start", false, &[]);
    let submitted = tool(
        &f,
        "submit_task",
        json!({"request_id":"auth-task", "task":"PROOF:auth", "allowed_paths":["left.txt"], "test_commands":["python3 check.py"]}),
    );
    let id = submitted["task_id"].as_str().unwrap();
    let (project, layout) = project_layout(&f);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if layout
            .open_readonly()
            .unwrap()
            .get_task(id.parse().unwrap())
            .unwrap()
            .unwrap()
            .status
            == bridge_domain::TaskStatus::Failed
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let storage = layout.open_readonly().unwrap();
    let (session, outbound, no_result, no_verifier): (String, String, bool, bool) = storage.connection().query_row(
        "SELECT session_id,outbound_message_id,(result_json IS NULL OR json_extract(result_json,'$.error') IS NOT NULL),verifier_json IS NULL FROM rounds WHERE task_id=?1", [id],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).unwrap();
    assert!(no_result && no_verifier);
    // User continues the original session manually through another provider.
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", project.opencode_endpoint().port())).unwrap();
    let body =
        json!({"messageID":"msg_manual_continue","parts":[{"type":"text","text":"PROOF:left"}]})
            .to_string();
    write!(stream,"POST /session/{session}/prompt_async HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.0 204"));
    let service =
        bridge_desktop::projects::ProjectService::new(f.config.clone(), f.root.join("state"))
            .unwrap();
    let query = || bridge_desktop::dashboard::Query {
        project: "proj".into(),
        active_only: true,
        linked: false,
        offset: 0,
        limit: 100,
        search: String::new(),
        status: None,
    };
    assert_eq!(
        service.dashboard(query()).unwrap()["tasks"][0]["recoverable"],
        true
    );
    loop {
        match service.recover_failed_task("proj", id) {
            Ok(v) => {
                assert_eq!(v["status"], "observing");
                break;
            }
            Err(e) => {
                assert!(e.contains("занята"), "{e}");
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let task = storage.get_task(id.parse().unwrap()).unwrap().unwrap();
        if task.status == bridge_domain::TaskStatus::AwaitingReview {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{:?}", task.status);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let (saved_session,saved_outbound,result,verifier):(String,String,String,String)=storage.connection().query_row(
        "SELECT session_id,outbound_message_id,result_json,verifier_json FROM rounds WHERE task_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).unwrap();
    assert_eq!(saved_session, session);
    assert_eq!(saved_outbound, outbound);
    assert!(serde_json::from_str::<Value>(&result).unwrap().is_object());
    assert_eq!(
        serde_json::from_str::<Value>(&verifier).unwrap()["status"],
        "passed"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("left.txt")).unwrap(),
        "left\n"
    );
    let prompts = fs::read_to_string(f.root.join("runtime/proof-prompts.jsonl")).unwrap();
    assert_eq!(
        prompts.lines().count(),
        2,
        "recovery must not send another prompt"
    );
    assert_eq!(
        tool(&f, "accept_task", json!({"task_id":id}))["status"],
        "accepted"
    );
}
