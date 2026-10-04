//! Real message HTTP and owned SQLite; deterministic clocks, no live OpenCode.
use bridge_config::load_config;
use bridge_domain::{TaskId, TaskStatus};
use bridge_runtime::{RuntimeOptions, ServerCommand};
use bridge_storage::{CreateTaskInput, RoundRef, RustStateLayout};
use bridge_worker::{
    execution::{RoundExecution, prepare_round_execution},
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
    execution: RoundExecution,
    body: Arc<Mutex<(u16, Value)>>,
    hook: RequestHook,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
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
        let snapshot = bridge_git::take_snapshot(&workspace)
            .unwrap()
            .to_json()
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let body = Arc::new(Mutex::new((200, json!([]))));
        let hook: RequestHook = Arc::new(Mutex::new(None));
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (b, h, r, s) = (body.clone(), hook.clone(), requests.clone(), stop.clone());
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
                        assert!(
                            request.starts_with("GET /session/session/message?"),
                            "unexpected method/path"
                        );
                        r.fetch_add(1, Ordering::Relaxed);
                        if let Some(action) = h.lock().unwrap().take() {
                            action();
                        }
                        let (status, value) = b.lock().unwrap().clone();
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
        std::fs::write(&config_path, format!("[projects.proj]\nworkspace = {:?}\nopencode_url = \"http://127.0.0.1:{port}\"\npassword_file = {:?}\nmax_rounds = 3\n", workspace.to_str().unwrap(), password.to_str().unwrap())).unwrap();
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
                allowed_paths: vec!["**".into()],
                test_commands: vec!["cargo test --offline".into()],
                snapshot: Some(snapshot),
            })
            .unwrap();
        let execution = prepare_round_execution(
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
