use bridge_config::{ProjectEntry, load_config};
use bridge_domain::{ProjectId, TaskId, TaskStatus};
use bridge_opencode::OpenCodeClient;
use bridge_storage::{CreateTaskInput, RoundRef, RustStateLayout};
use bridge_worker::{
    PermissionBlockerErrorKind,
    question::{QuestionBlockerError, QuestionBlockerOutcome, handle_question_blocker},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
type Hook = Box<dyn Fn() + Send + Sync>;
struct Mock {
    port: u16,
    body: Arc<Mutex<Value>>,
    requests: Arc<Mutex<Vec<String>>>,
    hook: Arc<Mutex<Option<Hook>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Mock {
    fn new(body: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let body = Arc::new(Mutex::new(body));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let hook: Arc<Mutex<Option<Hook>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (b, r, h, s) = (body.clone(), requests.clone(), hook.clone(), stop.clone());
        let thread = thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("mock accept: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 1024];
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8(bytes).unwrap();
                r.lock()
                    .unwrap()
                    .push(text.lines().next().unwrap_or("").into());
                if let Some(hook) = h.lock().unwrap().take() {
                    hook();
                }
                let body = b.lock().unwrap().to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        });
        Self {
            port,
            body,
            requests,
            hook,
            stop,
            thread: Some(thread),
        }
    }
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.shutdown();
    }
}
struct Fixture {
    root: PathBuf,
    layout: RustStateLayout,
    project: ProjectEntry,
    task: TaskId,
    server: Mock,
}
impl Fixture {
    fn new(body: Value) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-question-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let server = Mock::new(body);
        let password = root.join("password");
        std::fs::write(&password, "fixture-password").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&password, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let config = root.join("projects.toml");
        std::fs::write(&config,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:{}\"\npassword_file={}\nmax_rounds=3\n",json!(root.join("workspace")),server.port,json!(password))).unwrap();
        let project = load_config(&config)
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
        let layout = RustStateLayout::new(
            root.join("state"),
            ProjectId::try_from("proj".to_owned()).unwrap(),
        )
        .unwrap();
        layout.initialize().unwrap();
        let task: TaskId = "11111111-1111-4111-8111-111111111111".parse().unwrap();
        let mut storage = layout.open().unwrap();
        storage
            .create_task(CreateTaskInput {
                task_id: task,
                project_id: project.id().clone(),
                workspace: project.workspace().to_str().unwrap().into(),
                task: "fixture text".into(),
                request_id: "request".into(),
                payload_hash: "hash".into(),
                base_head: None,
                allowed_paths: vec![],
                test_commands: vec![],
                snapshot: None,
            })
            .unwrap();
        storage.connection().execute_batch("UPDATE tasks SET session_id='ses_current'; UPDATE rounds SET status='observing',session_id='ses_current',outbound_message_id='msg_bound',attempted=1;").unwrap();
        Self {
            root,
            layout,
            project,
            task,
            server,
        }
    }
    fn round(&self) -> RoundRef {
        RoundRef {
            task_id: self.task,
            project_id: self.project.id().clone(),
            round_number: 1,
        }
    }
    fn check(&self) -> Result<QuestionBlockerOutcome, QuestionBlockerError> {
        handle_question_blocker(
            &OpenCodeClient::from_project(&self.project, Duration::from_secs(1)).unwrap(),
            &self.layout,
            self.round(),
        )
    }
    fn events(&self) -> i64 {
        self.layout
            .open()
            .unwrap()
            .connection()
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.shutdown();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn question(session: &str, text: &str) -> Value {
    json!({"id":"que_fixture","sessionID":session,"questions":[{"header":"Fixture","question":text,"options":[]}]})
}

#[test]
fn question_filter_unicode_truncation_and_persist_once_never_answer() {
    let text = "ж".repeat(301);
    let f = Fixture::new(json!([
        question("ses_foreign", "foreign"),
        question("ses_current", &text)
    ]));
    let events = f.events();
    let QuestionBlockerOutcome::Blocked(blocked) = f.check().unwrap() else {
        panic!("current question must block")
    };
    assert_eq!(blocked.questions().len(), 1);
    assert_eq!(blocked.questions()[0].text().chars().count(), 300);
    assert_eq!(blocked.session_id(), "ses_current");
    assert_eq!(blocked.user_action().session_id(), Some("ses_current"));
    assert_eq!(
        blocked.round().result_json.as_ref().unwrap()["blockers"][0]["type"],
        "question"
    );
    assert_eq!(blocked.task().status, TaskStatus::NeedsUser);
    assert_eq!(f.events(), events + 1);
    assert!(!format!("{blocked:?}").contains("ses_current"));
    let updated = blocked.task().updated_at.clone();
    let QuestionBlockerOutcome::Blocked(replay) = f.check().unwrap() else {
        panic!()
    };
    assert_eq!(replay.task().updated_at, updated);
    assert_eq!(f.events(), events + 1);
    assert!(
        f.server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.starts_with("GET /question?"))
    );
}
#[test]
fn empty_and_foreign_only_questions_are_noop_and_malformed_is_typed_error() {
    let f = Fixture::new(json!([]));
    let events = f.events();
    assert!(matches!(
        f.check().unwrap(),
        QuestionBlockerOutcome::NoBlocker
    ));
    *f.server.body.lock().unwrap() = json!([question("ses_other", "foreign")]);
    assert!(matches!(
        f.check().unwrap(),
        QuestionBlockerOutcome::NoBlocker
    ));
    *f.server.body.lock().unwrap() = json!({"secret":"private response"});
    let error = f.check().unwrap_err();
    assert!(matches!(error, QuestionBlockerError::Questions(_)));
    assert!(!format!("{error} {error:?}").contains("private response"));
    assert_eq!(f.events(), events);
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(f.task)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Implementing
    );
}
#[test]
fn close_before_or_during_question_get_never_returns_stale_blocker() {
    let f = Fixture::new(json!([question("ses_current", "pending")]));
    let layout = f.layout.clone();
    let task = f.task;
    *f.server.hook.lock().unwrap() = Some(Box::new(move || {
        layout
            .open()
            .unwrap()
            .request_task_close(task, "during GET")
            .unwrap();
    }));
    let error = f.check().unwrap_err();
    assert!(
        matches!(error,QuestionBlockerError::Guard(ref e) if e.kind()==PermissionBlockerErrorKind::CloseRequested)
    );
    assert_eq!(f.server.requests.lock().unwrap().len(), 1);
    assert!(f.check().is_err());
    assert_eq!(f.server.requests.lock().unwrap().len(), 1);
}
#[test]
fn question_storage_failure_rolls_back_without_false_blocked_or_diagnostics_leak() {
    let f = Fixture::new(json!([question("ses_current", "pending")]));
    let events = f.events();
    f.layout.open().unwrap().connection().execute_batch("CREATE TRIGGER reject_blocker BEFORE UPDATE ON tasks WHEN NEW.status='needs_user' BEGIN SELECT RAISE(ABORT,'secret-trigger'); END;").unwrap();
    let error = f.check().unwrap_err();
    assert!(matches!(error, QuestionBlockerError::Storage(_)));
    assert!(!format!("{error} {error:?}").contains("secret-trigger"));
    assert_eq!(f.events(), events);
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(f.task)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Implementing
    );
}

#[test]
fn question_preflight_rejects_zero_round_and_missing_session_without_http() {
    let f = Fixture::new(json!([question("ses_current", "pending")]));
    let client = OpenCodeClient::from_project(&f.project, Duration::from_secs(1)).unwrap();
    let mut round = f.round();
    round.round_number = 0;
    assert!(handle_question_blocker(&client, &f.layout, round).is_err());
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute("UPDATE rounds SET session_id=NULL", [])
        .unwrap();
    assert!(
        matches!(f.check().unwrap_err(),QuestionBlockerError::Guard(ref e) if e.kind()==PermissionBlockerErrorKind::SessionUnknown)
    );
    assert!(f.server.requests.lock().unwrap().is_empty());
}
#[test]
fn question_finish_uses_committed_task_and_close_during_replay_never_reports_blocked() {
    let f = Fixture::new(json!([question("ses_current", "pending")]));
    f.layout.open().unwrap().connection().execute_batch("CREATE TRIGGER close_at_finish AFTER UPDATE OF status ON tasks WHEN NEW.status='needs_user' BEGIN UPDATE tasks SET status='closed',close_requested_at='requested' WHERE task_id=NEW.task_id; END;").unwrap();
    assert!(
        matches!(f.check().unwrap_err(),QuestionBlockerError::Guard(ref e) if e.kind()==PermissionBlockerErrorKind::CloseRequested)
    );
    let g = Fixture::new(json!([question("ses_current", "pending")]));
    assert!(matches!(
        g.check().unwrap(),
        QuestionBlockerOutcome::Blocked(_)
    ));
    let layout = g.layout.clone();
    let task = g.task;
    *g.server.hook.lock().unwrap() = Some(Box::new(move || {
        layout
            .open()
            .unwrap()
            .request_task_close(task, "during replay")
            .unwrap();
    }));
    assert!(
        matches!(g.check().unwrap_err(),QuestionBlockerError::Guard(ref e) if e.kind()==PermissionBlockerErrorKind::CloseRequested)
    );
}
