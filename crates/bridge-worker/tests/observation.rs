//! Real message HTTP and owned SQLite; deterministic clocks, no live OpenCode.
use bridge_config::load_config;
use bridge_domain::{TaskId, TaskStatus};
use bridge_runtime::{RuntimeOptions, ServerCommand};
use bridge_storage::{CreateTaskInput, RoundRef, RustStateLayout};
use bridge_worker::{
    execution::{FencedRoundExecution, prepare_fenced_round_execution},
    observation::{Observation, RoundObserver},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

type RequestHook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

struct Fixture {
    root: PathBuf,
    layout: RustStateLayout,
    execution: FencedRoundExecution,
    body: Arc<Mutex<(u16, Value)>>,
    permissions: Arc<Mutex<(u16, Value)>>,
    questions: Arc<Mutex<(u16, Value)>>,
    replies_status: Arc<Mutex<u16>>,
    calls: Arc<Mutex<Vec<String>>>,
    permission_hook: RequestHook,
    hook: RequestHook,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_external(0)
    }
    fn with_external(count: usize) -> Self {
        let root = std::env::temp_dir().join(format!("bridge-observer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let workspace = std::fs::canonicalize(root.join("workspace")).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );
        if count > 0 {
            commit_seed(&workspace);
        }
        let mut snapshot = bridge_git::take_snapshot(&workspace)
            .unwrap()
            .to_json()
            .unwrap();
        let mut external_roots = Vec::new();
        let mut external_snapshots = Vec::new();
        let mut allowed_paths = vec!["**".to_owned()];
        for i in 0..count {
            let path = root.join(format!("external-{i}"));
            std::fs::create_dir(&path).unwrap();
            git(&path, &["init", "-q"]);
            commit_seed(&path);
            let mut external = bridge_git::take_snapshot(&path).unwrap().to_json().unwrap();
            external["root"] = json!(path.to_str().unwrap());
            external_snapshots.push(external);
            allowed_paths.push(path.join("same.txt").to_str().unwrap().to_owned());
            external_roots.push(path.to_str().unwrap().to_owned());
        }
        snapshot["external_repositories"] = json!(external_snapshots);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let body = Arc::new(Mutex::new((200, json!([]))));
        let permissions = Arc::new(Mutex::new((200, json!([]))));
        let questions = Arc::new(Mutex::new((200, json!([]))));
        let replies_status = Arc::new(Mutex::new(200));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let permission_hook: RequestHook = Arc::new(Mutex::new(None));
        let hook: RequestHook = Arc::new(Mutex::new(None));
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (b, h, r, s) = (body.clone(), hook.clone(), requests.clone(), stop.clone());
        let (p, q, rs, recorded, ph) = (
            permissions.clone(),
            questions.clone(),
            replies_status.clone(),
            calls.clone(),
            permission_hook.clone(),
        );
        let server = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut request = Vec::new();
                        let mut byte = [0];
                        while !request.ends_with(b"\r\n\r\n") {
                            if stream.read(&mut byte).unwrap_or(0) == 0 {
                                break;
                            }
                            request.push(byte[0]);
                        }
                        let request = String::from_utf8(request).unwrap();
                        let first = request.lines().next().unwrap();
                        let target = first.split_whitespace().nth(1).unwrap();
                        let path = target.split('?').next().unwrap();
                        let length = request
                            .lines()
                            .filter_map(|l| l.split_once(':'))
                            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                            .unwrap_or(0);
                        let mut payload = vec![0; length];
                        stream.read_exact(&mut payload).unwrap();
                        recorded
                            .lock()
                            .unwrap()
                            .push(format!("{first}|{}", String::from_utf8(payload).unwrap()));
                        let (status, value) = match (first.split_whitespace().next().unwrap(), path)
                        {
                            ("GET", "/session/session/message") => {
                                r.fetch_add(1, Ordering::Relaxed);
                                if let Some(action) = h.lock().unwrap().take() {
                                    action();
                                }
                                b.lock().unwrap().clone()
                            }
                            ("GET", "/permission") => {
                                if let Some(action) = ph.lock().unwrap().take() {
                                    action();
                                }
                                p.lock().unwrap().clone()
                            }
                            ("GET", "/question") => q.lock().unwrap().clone(),
                            ("POST", path)
                                if path.starts_with("/permission/") && path.ends_with("/reply") =>
                            {
                                (*rs.lock().unwrap(), json!({}))
                            }
                            _ => panic!("unexpected method/path"),
                        };
                        let payload = value.to_string();
                        write!(stream, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).unwrap();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("mock accept: {e}"),
                }
            }
        });
        let password = root.join("password");
        std::fs::write(&password, "secret").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&password, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, format!("[projects.proj]\nworkspace = {:?}\nopencode_url = \"http://127.0.0.1:{port}\"\npassword_file = {:?}\nauto_approve_external_directories = {:?}\nmax_rounds = 3\n", workspace.to_str().unwrap(), password.to_str().unwrap(), external_roots)).unwrap();
        let config = load_config(&config_path).unwrap();
        let project = config.project("proj").unwrap();
        let layout = RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
        layout.initialize().unwrap();
        let round = RoundRef {
            task_id: "550e8400-e29b-41d4-a716-446655440000"
                .parse::<TaskId>()
                .unwrap(),
            project_id: project.id().clone(),
            round_number: 1,
        };
        let mut storage = layout.open().unwrap();
        storage
            .create_task(CreateTaskInput {
                task_id: round.task_id,
                project_id: round.project_id.clone(),
                workspace: workspace.to_str().unwrap().into(),
                task: "implement".into(),
                request_id: "submit".into(),
                payload_hash: "hash".into(),
                base_head: None,
                allowed_paths,
                test_commands: vec!["cargo test --offline".into()],
                snapshot: Some(snapshot),
            })
            .unwrap();
        let execution = prepare_fenced_round_execution(
            &layout,
            project,
            round.clone(),
            &[&layout],
            &[project],
            &ServerCommand::opencode(),
            RuntimeOptions::default(),
        )
        .unwrap();
        storage
            .bind_round_session(round.clone(), "session".into())
            .unwrap();
        storage
            .prepare_round(round.clone(), "outbound".into())
            .unwrap();
        storage.mark_round_sent(round).unwrap();
        Self {
            root,
            layout,
            execution,
            body,
            permissions,
            questions,
            replies_status,
            calls,
            permission_hook,
            hook,
            requests,
            stop,
            server: Some(server),
        }
    }
    fn history(&self, value: Value) {
        *self.body.lock().unwrap() = (200, value);
    }
    fn observer(&self) -> RoundObserver<'_> {
        RoundObserver::new(
            &self.execution,
            &self.layout,
            Duration::from_secs(5),
            Duration::from_secs(10),
        )
        .unwrap()
    }
    fn task_status(&self) -> TaskStatus {
        self.layout
            .open()
            .unwrap()
            .get_task("550e8400-e29b-41d4-a716-446655440000".parse().unwrap())
            .unwrap()
            .unwrap()
            .status
    }
    fn result(&self) -> Value {
        let s = self.layout.open().unwrap();
        let v: String = s
            .connection()
            .query_row("SELECT result_json FROM rounds", [], |r| r.get(0))
            .unwrap();
        serde_json::from_str(&v).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.server.take().unwrap().join().unwrap();
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}
fn user(id: &str) -> Value {
    json!({"info":{"id":id,"role":"user","sessionID":"session"},"parts":[]})
}
fn assistant(parent: &str, finish: &str, parts: Value) -> Value {
    json!({"info":{"id":"answer","role":"assistant","parentID":parent,"sessionID":"session","finish":finish,"time":{"completed":1},"tokens":{"input":12,"output":3},"providerID":"p","modelID":"m"},"parts":parts})
}
fn final_history() -> Value {
    json!([
        user("outbound"),
        assistant("outbound", "stop", json!([{"type":"text","text":" done "}]))
    ])
}
fn seconds(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn final_is_candidate_until_verifier_and_never_resends() {
    let f = Fixture::new();
    f.history(final_history());
    let mut observer = f.observer();
    let Observation::Final(candidate) = observer.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    assert_eq!(candidate.response().text(), "done");
    assert_eq!(candidate.messages().len(), 2);
    assert_eq!(candidate.result()["tool_errors"], json!([]));
    assert_eq!(f.task_status(), TaskStatus::Implementing);
    assert!(observer.poll(|| seconds(2), &[]).is_err());
    assert_eq!(f.requests.load(Ordering::Relaxed), 1);
    assert!(!format!("{candidate:?}").contains("done"));
}
#[test]
fn evidence_is_checked_before_deadline_and_boundary_is_strict() {
    let f = Fixture::new();
    f.history(final_history());
    let mut observer = f.observer();
    assert!(matches!(
        observer.poll(|| seconds(11), &[]).unwrap(),
        Observation::Finished(_)
    ));
    assert_eq!(f.task_status(), TaskStatus::NeedsUser);
    assert_eq!(f.result()["usage"]["input"].as_f64(), Some(12.0));
    let f = Fixture::new();
    f.history(final_history());
    assert!(matches!(
        f.observer().poll(|| seconds(10), &[]).unwrap(),
        Observation::Final(_)
    ));
}
#[test]
fn unknown_delivery_after_grace_but_not_at_boundary() {
    let f = Fixture::new();
    let mut o = f.observer();
    assert!(matches!(
        o.poll(|| seconds(5), &[]).unwrap(),
        Observation::Pending
    ));
    assert!(matches!(
        o.poll(|| seconds(6), &[]).unwrap(),
        Observation::Finished(_)
    ));
    assert_eq!(f.task_status(), TaskStatus::DeliveryUnknown);
    let f = Fixture::new();
    *f.body.lock().unwrap() = (503, json!({"secret":"never expose"}));
    assert!(matches!(
        f.observer().poll(|| seconds(11), &[]).unwrap(),
        Observation::Finished(_)
    ));
    assert_eq!(f.task_status(), TaskStatus::DeliveryUnknown);
    assert!(!f.result().to_string().contains("never expose"));
}
#[test]
fn delivered_latch_and_accounting_survive_transport_failure() {
    let f = Fixture::new();
    f.history(json!([
        user("outbound"),
        assistant("outbound", "tool-calls", json!([]))
    ]));
    let mut o = f.observer();
    assert!(matches!(
        o.poll(|| seconds(1), &[]).unwrap(),
        Observation::Pending
    ));
    *f.body.lock().unwrap() = (503, json!({}));
    assert!(matches!(
        o.poll(|| seconds(6), &[]).unwrap(),
        Observation::Pending
    ));
    let blocker = json!({"type":"question","id":"pending"});
    assert!(matches!(
        o.poll(|| seconds(11), std::slice::from_ref(&blocker))
            .unwrap(),
        Observation::Finished(_)
    ));
    assert_eq!(f.task_status(), TaskStatus::NeedsUser);
    assert_eq!(f.result()["usage"]["input"].as_f64(), Some(12.0));
    assert_eq!(f.result()["blockers"], json!([blocker]));
}
#[test]
fn old_error_with_later_tui_user_is_not_terminal() {
    let f = Fixture::new();
    let mut error = assistant("outbound", "stop", json!([]));
    error["info"]["error"] = json!({"secret":"private"});
    f.history(json!([user("outbound"), error.clone(), user("tui")]));
    let mut o = f.observer();
    assert!(matches!(
        o.poll(|| seconds(1), &[]).unwrap(),
        Observation::Pending
    ));
    f.history(json!([
        user("outbound"),
        error,
        user("tui"),
        assistant("tui", "stop", json!([]))
    ]));
    assert!(matches!(
        o.poll(|| seconds(2), &[]).unwrap(),
        Observation::Final(_)
    ));
    let f = Fixture::new();
    let mut error = assistant("outbound", "stop", json!([]));
    error["info"]["error"] = json!("private");
    f.history(json!([user("outbound"), error]));
    assert!(matches!(
        f.observer().poll(|| seconds(1), &[]).unwrap(),
        Observation::Finished(_)
    ));
    assert_eq!(f.task_status(), TaskStatus::Failed);
    assert!(!f.result().to_string().contains("private"));
}
#[test]
fn tool_lifecycle_and_unrelated_assistants_cannot_finish_round() {
    let f = Fixture::new();
    let mut o = f.observer();
    for finish in ["", "unknown", "tool-calls"] {
        f.history(json!([
            user("outbound"),
            assistant("outbound", finish, json!([]))
        ]));
        assert!(matches!(
            o.poll(|| seconds(1), &[]).unwrap(),
            Observation::Pending
        ));
    }
    f.history(json!([
        user("outbound"),
        assistant("other", "stop", json!([]))
    ]));
    assert!(matches!(
        o.poll(|| seconds(1), &[]).unwrap(),
        Observation::Pending
    ));
    f.history(json!([
        user("outbound"),
        assistant(
            "outbound",
            "stop",
            json!([{"type":"tool","state":{"status":"completed"}}])
        )
    ]));
    assert!(matches!(
        o.poll(|| seconds(1), &[]).unwrap(),
        Observation::Pending
    ));
    f.history(json!([
        user("outbound"),
        assistant(
            "outbound",
            "stop",
            json!([{"type":"tool","state":{"status":"error","metadata":{"interrupted":true}}}])
        )
    ]));
    let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
        panic!("interrupted tool resolved");
    };
    assert_eq!(
        candidate.result()["tool_errors"].as_array().unwrap().len(),
        1
    );
}
#[test]
fn close_during_get_prevents_result_write() {
    let f = Fixture::new();
    f.history(final_history());
    let layout = f.layout.clone();
    *f.hook.lock().unwrap() = Some(Box::new(move || {
        layout
            .open()
            .unwrap()
            .connection()
            .execute(
                "UPDATE tasks SET close_requested_at='2026-10-05T00:00:00Z'",
                [],
            )
            .unwrap();
    }));
    assert!(f.observer().poll(|| seconds(1), &[]).is_err());
    assert_eq!(f.task_status(), TaskStatus::Implementing);
    let count: i64 = f
        .layout
        .open()
        .unwrap()
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM rounds WHERE result_json IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}
#[test]
fn changed_binding_and_decreasing_clock_fail_closed() {
    let f = Fixture::new();
    let mut o = f.observer();
    assert!(matches!(
        o.poll(|| seconds(1), &[]).unwrap(),
        Observation::Pending
    ));
    assert!(o.poll(|| Duration::ZERO, &[]).is_err());
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute("UPDATE rounds SET session_id='replacement'", [])
        .unwrap();
    let before = f.requests.load(Ordering::Relaxed);
    assert!(o.poll(|| seconds(2), &[]).is_err());
    assert_eq!(f.requests.load(Ordering::Relaxed), before);
}

#[test]
fn foreign_session_history_is_rejected_before_mutation() {
    let f = Fixture::new();
    let mut history = final_history();
    history[1]["info"]["sessionID"] = json!("foreign");
    f.history(history);
    assert!(f.observer().poll(|| seconds(1), &[]).is_err());
    let status: String = f
        .layout
        .open()
        .unwrap()
        .connection()
        .query_row("SELECT status FROM rounds", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "sent");
}

#[test]
fn publication_runs_saved_verifier_and_atomically_exposes_review_result() {
    let f = Fixture::new();
    f.history(final_history());
    std::fs::write(f.execution.execution.root.join("change.txt"), "change").unwrap();
    let mut o = f.observer();
    let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    assert_eq!(f.task_status(), TaskStatus::Implementing);
    let outcome = o
        .publish_final(&candidate, Duration::from_secs(5), 4096)
        .unwrap();
    assert_eq!(outcome.task.status, TaskStatus::AwaitingReview);
    assert_eq!(outcome.round.status, bridge_domain::RoundStatus::Complete);
    assert_eq!(outcome.round.response.as_deref(), Some("done"));
    assert_eq!(outcome.round.response_message_id.as_deref(), Some("answer"));
    let result = f.result();
    assert_eq!(result["verification"]["status"], "failed");
    assert_eq!(
        result["verification"]["commands"][0]["command"],
        "cargo test --offline"
    );
    assert!(
        result["changed_paths"]
            .as_array()
            .unwrap()
            .contains(&json!("change.txt"))
    );
    assert_eq!(result["changed_paths"], result["task_changed_paths"]);
    assert_eq!(result["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(result["usage"]["input"].as_f64(), Some(12.0));
    assert_eq!(result["model"], json!({"provider_id":"p","model_id":"m"}));
    let storage = f.layout.open().unwrap();
    let checkpoints: i64 = storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM rounds WHERE checkpoint_json IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(checkpoints, 1);
    let events: i64 = storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind='complete'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(events, 1);
    assert!(
        o.publish_final(&candidate, Duration::from_secs(5), 4096)
            .is_err()
    );
    assert_eq!(f.requests.load(Ordering::Relaxed), 1);
}

#[test]
fn publication_keeps_tool_errors_and_unsafe_verification_for_review() {
    let f = Fixture::new();
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute("UPDATE tasks SET test_commands='[\"git push\"]'", [])
        .unwrap();
    f.history(json!([user("outbound"), assistant("outbound", "stop", json!([{"type":"tool","tool":"bash","state":{"status":"error","error":"private diagnostic","metadata":{"interrupted":true}}}]))]));
    let mut o = f.observer();
    let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    o.publish_final(&candidate, Duration::from_secs(5), 4096)
        .unwrap();
    assert_eq!(f.task_status(), TaskStatus::AwaitingReview);
    assert_eq!(f.result()["verification"]["status"], "unsafe");
    assert_eq!(f.result()["tool_errors"], json!(["tool execution failed"]));
    assert!(!f.result().to_string().contains("private diagnostic"));
}

#[test]
fn foreign_candidate_and_close_before_publication_do_not_start_verifier() {
    let a = Fixture::new();
    a.history(final_history());
    let b = Fixture::new();
    b.history(final_history());
    let mut oa = a.observer();
    let mut ob = b.observer();
    let Observation::Final(ca) = oa.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    let Observation::Final(cb) = ob.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    assert!(oa.publish_final(&cb, seconds(5), 4096).is_err());
    a.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET close_requested_at='2026-10-05T00:00:00Z'",
            [],
        )
        .unwrap();
    assert!(oa.publish_final(&ca, seconds(5), 4096).is_err());
    for f in [&a, &b] {
        let marker: Option<String> = f
            .layout
            .open()
            .unwrap()
            .connection()
            .query_row("SELECT verifier_state FROM rounds", [], |r| r.get(0))
            .unwrap();
        assert_eq!(marker, None);
    }
}

#[test]
fn changed_external_snapshot_is_rejected_before_verifier() {
    let f = Fixture::new();
    f.history(final_history());
    let mut o = f.observer();
    let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    let storage = f.layout.open().unwrap();
    let raw: String = storage
        .connection()
        .query_row("SELECT snapshot FROM tasks", [], |r| r.get(0))
        .unwrap();
    let mut snapshot: Value = serde_json::from_str(&raw).unwrap();
    snapshot["external_repositories"] = json!([{"root":"/external"}]);
    storage
        .connection()
        .execute("UPDATE tasks SET snapshot=?1", [snapshot.to_string()])
        .unwrap();
    assert_eq!(
        o.publish_final(&candidate, seconds(5), 4096).unwrap_err(),
        bridge_worker::execution::ExecutionError::Baseline
    );
    let marker: Option<String> = storage
        .connection()
        .query_row("SELECT verifier_state FROM rounds", [], |r| r.get(0))
        .unwrap();
    assert_eq!(marker, None);
    assert_eq!(f.task_status(), TaskStatus::Implementing);
}

#[test]
fn collection_failure_reuses_verifier_on_retry_without_lossy_paths() {
    use std::os::unix::ffi::OsStringExt;
    let f = Fixture::new();
    f.history(final_history());
    let bad = f
        .execution
        .execution
        .root
        .join(std::ffi::OsString::from_vec(b"bad-\xff".to_vec()));
    std::fs::write(&bad, "data").unwrap();
    let mut o = f.observer();
    let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    assert!(o.publish_final(&candidate, seconds(5), 4096).is_err());
    assert_eq!(f.task_status(), TaskStatus::Implementing);
    let verified = f
        .execution
        .execution
        .verify(&f.layout, seconds(5), 4096)
        .unwrap();
    assert!(verified.reused());
    std::fs::remove_file(bad).unwrap();
    o.publish_final(&candidate, seconds(5), 4096).unwrap();
    assert_eq!(f.task_status(), TaskStatus::AwaitingReview);
    assert_eq!(
        f.result()["verification"],
        serde_json::to_value(verified.verification()).unwrap()
    );
    assert_eq!(f.requests.load(Ordering::Relaxed), 1);
}

fn slow_cargo_project(f: &Fixture) {
    let workspace = &f.execution.execution.root;
    std::fs::create_dir(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"observer_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(workspace.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(
        workspace.join("src/lib.rs"),
        "#[test] fn slow() { std::thread::sleep(std::time::Duration::from_secs(10)); }\n",
    )
    .unwrap();
}

#[test]
fn verifier_timeout_remains_reviewable_and_worker_fences_stay_held() {
    use bridge_worker::{WorkerLock, WorkerLockOutcome};
    let f = Fixture::new();
    slow_cargo_project(&f);
    f.history(final_history());
    let mut o = f.observer();
    let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    assert!(matches!(
        WorkerLock::try_acquire(&f.layout).unwrap(),
        WorkerLockOutcome::Busy
    ));
    o.publish_final(&candidate, Duration::from_millis(20), 4096)
        .unwrap();
    assert_eq!(f.task_status(), TaskStatus::AwaitingReview);
    assert_eq!(f.result()["verification"]["status"], "timed_out");
    assert!(matches!(
        WorkerLock::try_acquire(&f.layout).unwrap(),
        WorkerLockOutcome::Busy
    ));
}

#[test]
fn cooperative_close_during_verifier_prevents_review_publication() {
    let f = Fixture::new();
    slow_cargo_project(&f);
    f.history(final_history());
    let mut o = f.observer();
    let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    let layout = f.layout.clone();
    let closer = thread::spawn(move || {
        for _ in 0..2000 {
            let storage = layout.open().unwrap();
            let marker: Option<String> = storage
                .connection()
                .query_row("SELECT verifier_state FROM rounds", [], |r| r.get(0))
                .unwrap();
            if marker.as_deref() == Some("running") {
                storage
                    .connection()
                    .execute(
                        "UPDATE tasks SET close_requested_at='2026-10-05T00:00:00Z'",
                        [],
                    )
                    .unwrap();
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("verifier did not start");
    });
    assert!(
        o.publish_final(&candidate, Duration::from_millis(300), 4096)
            .is_err()
    );
    closer.join().unwrap();
    assert_eq!(f.task_status(), TaskStatus::Implementing);
    let result: Option<String> = f
        .layout
        .open()
        .unwrap()
        .connection()
        .query_row("SELECT result_json FROM rounds", [], |r| r.get(0))
        .unwrap();
    assert_eq!(result, None);
}

fn git(root: &std::path::Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
}
fn commit_seed(root: &std::path::Path) {
    std::fs::write(root.join("tracked.txt"), "baseline").unwrap();
    git(root, &["add", "tracked.txt"]);
    git(
        root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ],
    );
}

#[test]
fn multi_repository_publication_qualifies_paths_and_preserves_main_checkpoint() {
    let f = Fixture::with_external(2);
    f.history(final_history());
    for root in [
        f.root.join("workspace"),
        f.root.join("external-0"),
        f.root.join("external-1"),
    ] {
        std::fs::write(root.join("same.txt"), "changed").unwrap();
    }
    let ext = f.root.join("external-0");
    std::fs::write(ext.join("outside.txt"), "out of scope").unwrap();
    git(&ext, &["add", "outside.txt"]);
    git(
        &ext,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "outside scope",
        ],
    );
    let mut o = f.observer();
    let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    o.publish_final(&candidate, seconds(5), 4096).unwrap();
    let result = f.result();
    assert_eq!(result["task_changed_paths"], json!(["same.txt"]));
    for path in [
        "external-0/same.txt",
        "external-1/same.txt",
        "external-0/outside.txt",
    ] {
        assert!(
            result["changed_paths"]
                .as_array()
                .unwrap()
                .contains(&json!(f.root.join(path).to_str().unwrap()))
        );
    }
    assert!(
        result["scope_violations"]
            .as_array()
            .unwrap()
            .contains(&json!(ext.join("outside.txt").to_str().unwrap()))
    );
    assert!(
        result["committed_paths"]
            .as_array()
            .unwrap()
            .contains(&json!(ext.join("outside.txt").to_str().unwrap()))
    );
    assert!(
        result["git_policy_violations"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("{}:head_changed", ext.display())))
    );
    assert_eq!(result["repositories"].as_array().unwrap().len(), 3);
    assert_eq!(
        result["verification"]["repositories"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let raw: String = f
        .layout
        .open()
        .unwrap()
        .connection()
        .query_row("SELECT checkpoint_json FROM rounds", [], |r| r.get(0))
        .unwrap();
    let checkpoint: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(checkpoint["repositories"].as_array().unwrap().len(), 3);
    for r in checkpoint["repositories"].as_array().unwrap() {
        assert_eq!(r["available"], true);
    }
    assert!(!raw.contains(f.root.to_str().unwrap()));
}

#[test]
fn vanished_external_is_reported_and_verifier_never_runs_checks() {
    let f = Fixture::with_external(1);
    f.history(final_history());
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET test_commands='[\"touch must-not-run\"]'",
            [],
        )
        .unwrap();
    std::fs::remove_dir_all(f.root.join("external-0")).unwrap();
    let mut o = f.observer();
    let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    o.publish_final(&candidate, seconds(5), 4096).unwrap();
    let result = f.result();
    assert_eq!(result["verification"]["status"], "error");
    assert_eq!(
        result["repositories"][1]["git_policy_violations"],
        json!(["external_repo_missing"])
    );
    assert!(!f.execution.execution.root.join("must-not-run").exists());
    assert_eq!(f.task_status(), TaskStatus::AwaitingReview);
}

#[test]
fn rebound_external_root_and_scope_symlinks_are_rejected_before_http_or_verifier() {
    use std::os::unix::fs::symlink;
    for rebind_scope in [false, true] {
        let f = Fixture::with_external(1);
        f.history(final_history());
        let ext = f.root.join("external-0");
        let foreign = f.root.join("foreign");
        std::fs::create_dir(&foreign).unwrap();
        if rebind_scope {
            symlink(&foreign, ext.join("same.txt")).unwrap();
        } else {
            std::fs::rename(&ext, f.root.join("old-external")).unwrap();
            symlink(&foreign, &ext).unwrap();
        }
        assert!(RoundObserver::new(&f.execution, &f.layout, seconds(5), seconds(10)).is_err());
        assert_eq!(f.requests.load(Ordering::Relaxed), 0);
        let marker: Option<String> = f
            .layout
            .open()
            .unwrap()
            .connection()
            .query_row("SELECT verifier_state FROM rounds", [], |r| r.get(0))
            .unwrap();
        assert_eq!(marker, None);
    }
}

#[test]
fn checks_touching_external_files_are_in_verifier_effects_and_final_collection() {
    let f = Fixture::with_external(1);
    f.history(final_history());
    let ext = f.root.join("external-0");
    let command = format!("touch main-effect.txt '{}/same.txt'", ext.display());
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET test_commands=?1",
            [json!([command]).to_string()],
        )
        .unwrap();
    let mut o = f.observer();
    let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
        panic!("final");
    };
    o.publish_final(&candidate, seconds(5), 4096).unwrap();
    let result = f.result();
    assert_eq!(result["verification"]["status"], "passed");
    assert_eq!(
        result["verification"]["repositories"][0]["side_effects"],
        json!(["same.txt"])
    );
    assert!(
        result["verification"]["side_effects"]
            .as_array()
            .unwrap()
            .contains(&json!(ext.join("same.txt").to_str().unwrap()))
    );
    assert!(
        result["changed_paths"]
            .as_array()
            .unwrap()
            .contains(&json!(ext.join("same.txt").to_str().unwrap()))
    );
    assert_eq!(result["task_changed_paths"], json!(["main-effect.txt"]));
}

#[test]
fn preparation_rejects_untrusted_corrupt_duplicate_or_unreferenced_external_baselines() {
    use bridge_worker::execution::{ExecutionError, prepare_round_execution};
    for case in [
        "untrusted",
        "corrupt",
        "duplicate",
        "undeclared",
        "unused",
        "nested",
        "alias",
        "too_many",
    ] {
        let f = Fixture::with_external(2);
        let storage = f.layout.open().unwrap();
        let task = storage
            .get_task("550e8400-e29b-41d4-a716-446655440000".parse().unwrap())
            .unwrap()
            .unwrap();
        let mut snapshot = task.snapshot.unwrap();
        let mut scopes = task.allowed_paths;
        let expected = match case {
            "untrusted" => {
                snapshot["external_repositories"][0]["root"] =
                    json!(f.root.join("untrusted").to_str().unwrap());
                ExecutionError::Binding
            }
            "corrupt" => {
                snapshot["external_repositories"][0]["manifest"] = json!([]);
                ExecutionError::Baseline
            }
            "duplicate" => {
                let duplicate = snapshot["external_repositories"][0].clone();
                snapshot["external_repositories"]
                    .as_array_mut()
                    .unwrap()
                    .push(duplicate);
                ExecutionError::Binding
            }
            "undeclared" => {
                snapshot
                    .as_object_mut()
                    .unwrap()
                    .remove("external_repositories");
                ExecutionError::Binding
            }
            "unused" => {
                scopes.pop();
                ExecutionError::Binding
            }
            "nested" => {
                let nested = f.root.join("external-0/nested");
                std::fs::create_dir(&nested).unwrap();
                snapshot["external_repositories"][0]["root"] = json!(nested.to_str().unwrap());
                ExecutionError::Binding
            }
            "alias" => {
                snapshot["external_repositories"][0]["root"] =
                    json!(format!("{}/", f.root.join("external-0").display()));
                ExecutionError::Binding
            }
            "too_many" => {
                snapshot["external_repositories"] =
                    json!(vec![snapshot["external_repositories"][0].clone(); 64]);
                ExecutionError::Baseline
            }
            _ => unreachable!(),
        };
        storage
            .connection()
            .execute(
                "UPDATE tasks SET snapshot=?1, allowed_paths=?2",
                [snapshot.to_string(), json!(scopes).to_string()],
            )
            .unwrap();
        storage
            .connection()
            .execute("UPDATE rounds SET status='pending', attempted=0", [])
            .unwrap();
        let config = load_config(&f.root.join("config.toml")).unwrap();
        let project = config.project("proj").unwrap();
        let round = RoundRef {
            task_id: task.task_id,
            project_id: task.project_id,
            round_number: 1,
        };
        let error = prepare_round_execution(
            &f.layout,
            project,
            round,
            &[&f.layout],
            &[project],
            &ServerCommand::opencode(),
            RuntimeOptions::default(),
        )
        .unwrap_err();
        assert_eq!(error, expected, "{case}");
        assert_eq!(f.requests.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn reconstruct_attempted_direct_round_reuses_session_and_never_dispatches() {
    use bridge_worker::execution::resume_round_execution;
    for (round_status, task_status) in [
        ("sent", "implementing"),
        ("observing", "implementing"),
        ("needs_user", "implementing"),
        ("delivery_unknown", "implementing"),
    ] {
        let mut f = Fixture::with_external(1);
        f.history(final_history());
        let storage = f.layout.open().unwrap();
        storage
            .connection()
            .execute("UPDATE rounds SET status=?1", [round_status])
            .unwrap();
        storage
            .connection()
            .execute("UPDATE tasks SET status=?1", [task_status])
            .unwrap();
        let config = load_config(&f.root.join("config.toml")).unwrap();
        let project = config.project("proj").unwrap();
        let round = RoundRef {
            task_id: "550e8400-e29b-41d4-a716-446655440000".parse().unwrap(),
            project_id: project.id().clone(),
            round_number: 1,
        };
        let before = f.execution.execution.baseline_json().unwrap();
        f.execution.execution =
            resume_round_execution(&f.layout, project, round, seconds(2)).unwrap();
        assert_eq!(f.execution.execution.baseline_json().unwrap(), before);
        assert_eq!(f.requests.load(Ordering::Relaxed), 0);
        assert!(f.execution.execution.dispatch(&f.layout).is_err());
        assert_eq!(f.requests.load(Ordering::Relaxed), 0);
        let mut o = f.observer();
        let Observation::Final(candidate) = o.poll(|| seconds(1), &[]).unwrap() else {
            panic!("final");
        };
        o.publish_final(&candidate, seconds(5), 4096).unwrap();
        assert_eq!(f.task_status(), TaskStatus::AwaitingReview);
        assert_eq!(f.requests.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn resume_requires_attempt_and_saved_identifiers_and_refuses_terminal_or_close() {
    use bridge_worker::execution::resume_round_execution;
    for sql in [
        "UPDATE rounds SET attempted=0",
        "UPDATE rounds SET session_id=NULL",
        "UPDATE rounds SET outbound_message_id=NULL",
        "UPDATE rounds SET status='complete'",
        "UPDATE tasks SET close_requested_at='2026-10-05T00:00:00Z'",
    ] {
        let f = Fixture::new();
        f.layout
            .open()
            .unwrap()
            .connection()
            .execute(sql, [])
            .unwrap();
        let config = load_config(&f.root.join("config.toml")).unwrap();
        let project = config.project("proj").unwrap();
        let round = RoundRef {
            task_id: "550e8400-e29b-41d4-a716-446655440000".parse().unwrap(),
            project_id: project.id().clone(),
            round_number: 1,
        };
        assert!(resume_round_execution(&f.layout, project, round, seconds(2)).is_err());
        assert_eq!(f.requests.load(Ordering::Relaxed), 0);
    }
}

fn running_history() -> Value {
    json!([
        user("outbound"),
        assistant("outbound", "tool-calls", json!([]))
    ])
}
fn permission(id: &str, session: &str) -> Value {
    json!({"id":id,"sessionID":session,"permission":"bash","patterns":["cargo test --offline"]})
}
fn project(f: &Fixture, approve: bool) -> bridge_config::ProjectEntry {
    if approve {
        use std::io::Write;
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(f.root.join("config.toml"))
            .unwrap();
        writeln!(config, "auto_approve_permissions = [\"bash\"]").unwrap();
    }
    load_config(&f.root.join("config.toml"))
        .unwrap()
        .project("proj")
        .unwrap()
        .clone()
}
#[test]
fn stale_blocker_clears_or_final_response_wins_before_grace() {
    let f = Fixture::new();
    f.history(running_history());
    *f.permissions.lock().unwrap() = (200, json!([permission("pending", "session")]));
    let project = project(&f, false);
    let mut o = f.observer();
    assert!(matches!(
        o.poll_with_blockers(&project, || seconds(1), seconds(3))
            .unwrap(),
        Observation::Pending
    ));
    assert_eq!(f.task_status(), TaskStatus::Implementing);
    *f.permissions.lock().unwrap() = (200, json!([]));
    assert!(matches!(
        o.poll_with_blockers(&project, || seconds(3), seconds(3))
            .unwrap(),
        Observation::Pending
    ));
    *f.permissions.lock().unwrap() = (200, json!([permission("pending", "session")]));
    assert!(matches!(
        o.poll_with_blockers(&project, || seconds(4), seconds(3))
            .unwrap(),
        Observation::Pending
    ));
    f.history(final_history());
    assert!(matches!(
        o.poll_with_blockers(&project, || seconds(5), seconds(3))
            .unwrap(),
        Observation::Final(_)
    ));
    assert!(
        !f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("POST"))
    );
}
#[test]
fn grace_boundary_combines_permissions_and_questions_preserving_usage_and_changes() {
    let f = Fixture::with_external(1);
    f.history(running_history());
    std::fs::write(f.root.join("external-0/same.txt"), "executor").unwrap();
    *f.permissions.lock().unwrap() = (
        200,
        json!([
            permission("pending", "session"),
            permission("foreign", "other")
        ]),
    );
    *f.questions.lock().unwrap() = (
        200,
        json!([{"id":"q","sessionID":"session","questions":[{"question":"Which approach?","header":"Question","options":[]}]}, {"id":"other","sessionID":"other","questions":[{"question":"foreign","header":"Question","options":[]}]}]),
    );
    let project = project(&f, false);
    let mut o = f.observer();
    assert!(matches!(
        o.poll_with_blockers(&project, || seconds(1), seconds(3))
            .unwrap(),
        Observation::Pending
    ));
    assert!(matches!(
        o.poll_with_blockers(&project, || seconds(4), seconds(3))
            .unwrap(),
        Observation::Finished(_)
    ));
    let result = f.result();
    assert_eq!(result["blockers"].as_array().unwrap().len(), 2);
    assert_eq!(result["blockers"][1]["text"], "Which approach?");
    assert_eq!(result["usage"]["input"].as_f64(), Some(12.0));
    assert_eq!(result["repositories"].as_array().unwrap().len(), 2);
    assert!(!result.to_string().contains("foreign"));
    assert!(
        !f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("POST"))
    );
}
#[test]
fn deadline_inside_stale_grace_reports_real_question_without_answering_it() {
    let f = Fixture::new();
    f.history(running_history());
    *f.questions.lock().unwrap() = (
        200,
        json!([{"id":"q","sessionID":"session","questions":[{"question":"Answer in TUI","header":"Question","options":[]}]}]),
    );
    let project = project(&f, false);
    let mut o = f.observer();
    assert!(matches!(
        o.poll_with_blockers(&project, || seconds(9), seconds(15))
            .unwrap(),
        Observation::Pending
    ));
    assert!(matches!(
        o.poll_with_blockers(&project, || seconds(11), seconds(15))
            .unwrap(),
        Observation::Finished(_)
    ));
    assert_eq!(f.result()["blockers"][0]["type"], "question");
    assert!(
        !f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("POST"))
    );
}
#[test]
fn configured_permission_once_is_cached_and_foreign_session_never_replied() {
    let f = Fixture::new();
    f.history(running_history());
    *f.permissions.lock().unwrap() = (
        200,
        json!([
            permission("allow", "session"),
            permission("foreign", "other")
        ]),
    );
    let project = project(&f, true);
    let mut o = f.observer();
    for now in [1, 2, 3] {
        assert!(matches!(
            o.poll_with_blockers(&project, || seconds(now), seconds(3))
                .unwrap(),
            Observation::Pending
        ));
    }
    let calls = f.calls.lock().unwrap();
    let posts: Vec<_> = calls.iter().filter(|c| c.starts_with("POST")).collect();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].starts_with("POST /permission/allow/reply?"));
    assert!(posts[0].contains("\"reply\":\"once\""));
}
#[test]
fn failed_permission_reply_is_a_blocker_and_close_during_get_prevents_reply() {
    let f = Fixture::new();
    f.history(running_history());
    *f.permissions.lock().unwrap() = (200, json!([permission("allow", "session")]));
    *f.replies_status.lock().unwrap() = 503;
    let configured = project(&f, true);
    assert!(matches!(
        f.observer()
            .poll_with_blockers(&configured, || seconds(1), Duration::ZERO)
            .unwrap(),
        Observation::Finished(_)
    ));
    assert_eq!(f.result()["blockers"][0]["reason"], "reply_failed");
    let f = Fixture::new();
    f.history(running_history());
    *f.permissions.lock().unwrap() = (200, json!([permission("allow", "session")]));
    let configured = project(&f, true);
    let layout = f.layout.clone();
    *f.permission_hook.lock().unwrap() = Some(Box::new(move || {
        layout
            .open()
            .unwrap()
            .request_task_close(
                "550e8400-e29b-41d4-a716-446655440000".parse().unwrap(),
                "close",
            )
            .unwrap();
    }));
    assert!(
        f.observer()
            .poll_with_blockers(&configured, || seconds(1), Duration::ZERO)
            .is_err()
    );
    assert!(
        !f.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("POST"))
    );
}
