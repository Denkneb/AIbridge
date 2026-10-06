use bridge_config::{ProjectEntry, load_config};
use bridge_domain::{TaskId, TaskStatus};
use bridge_runtime::{RuntimeOptions, ServerCommand};
use bridge_storage::{CreateTaskInput, RoundRef, RustStateLayout};
use bridge_worker::{
    WorkerLock, WorkerLockOutcome,
    runner::{WorkerOutcome, WorkerRunError, WorkerSettings, run_worker},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
const ID: &str = "550e8400-e29b-41d4-a716-446655440000";
type Hook = Option<Box<dyn FnOnce() + Send>>;
struct ServerState {
    directory: Value,
    session: (u16, Value),
    outbound: Option<String>,
    post_status: u16,
    delivered: bool,
    calls: Vec<String>,
    hook_path: Option<String>,
    hook: Hook,
}
struct Fixture {
    root: PathBuf,
    project: ProjectEntry,
    layout: RustStateLayout,
    state: Arc<Mutex<ServerState>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bridge-runner-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );
        let password = root.join("password");
        std::fs::write(&password, "secret").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&password, std::fs::Permissions::from_mode(0o600)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(ServerState {
            directory: json!(workspace),
            session: (200, json!({"id":"session","directory":workspace})),
            outbound: None,
            post_status: 204,
            delivered: true,
            calls: Vec::new(),
            hook_path: None,
            hook: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, end) = (state.clone(), stop.clone());
        let server = thread::spawn(move || {
            while !end.load(Ordering::Relaxed) {
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
                        let text = String::from_utf8(request).unwrap();
                        let first = text.lines().next().unwrap();
                        let mut fields = first.split_whitespace();
                        let method = fields.next().unwrap();
                        let target = fields.next().unwrap();
                        let path = target.split('?').next().unwrap();
                        let length = text
                            .lines()
                            .filter_map(|l| l.split_once(':'))
                            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                            .unwrap_or(0);
                        let mut payload = vec![0; length];
                        stream.read_exact(&mut payload).unwrap();
                        let mut state = s.lock().unwrap();
                        state.calls.push(format!("{method} {path}"));
                        if state.hook_path.as_deref() == Some(path)
                            && let Some(action) = state.hook.take()
                        {
                            action();
                        }
                        let (status, value) = match (method, path) {
                            ("GET", "/path") => {
                                assert_eq!(target, "/path");
                                (200, json!({"directory":state.directory}))
                            }
                            ("GET", "/session") => (200, json!([])),
                            ("POST", "/session") => (200, state.session.1.clone()),
                            ("GET", "/session/session") => state.session.clone(),
                            ("POST", "/session/session/prompt_async") => {
                                let body: Value = serde_json::from_slice(&payload).unwrap();
                                state.outbound = Some(body["messageID"].as_str().unwrap().into());
                                (state.post_status, json!({}))
                            }
                            ("GET", "/session/session/message") => {
                                let body = if state.delivered {
                                    state.outbound.as_ref().map(|id| json!([
                                    {"info":{"id":id,"role":"user","sessionID":"session"},"parts":[]},
                                    {"info":{"id":"answer","role":"assistant","parentID":id,"sessionID":"session","finish":"stop","time":{"completed":1}},"parts":[{"type":"text","text":"done"}]}
                                ])).unwrap_or(json!([]))
                                } else {
                                    json!([])
                                };
                                (200, body)
                            }
                            ("GET", "/permission" | "/question") => (200, json!([])),
                            _ => panic!("unexpected HTTP route"),
                        };
                        let payload = if status == 204 {
                            String::new()
                        } else {
                            value.to_string()
                        };
                        // A close hook can cancel the client while its response
                        // is being sent. Do not poison shared evidence on that
                        // expected disconnect, or mask the worker's outcome.
                        drop(state);
                        if let Err(error) = write!(
                            stream,
                            "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        ) {
                            assert!(
                                matches!(
                                    error.kind(),
                                    std::io::ErrorKind::BrokenPipe
                                        | std::io::ErrorKind::ConnectionReset
                                ),
                                "fixture response failed: {error}"
                            );
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        let config_path = root.join("projects.toml");
        std::fs::write(&config_path,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:{port}\"\npassword_file={}\nmax_rounds=3\n",json!(workspace),json!(password))).unwrap();
        let project = load_config(&config_path)
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
        let layout = RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
        layout.initialize().unwrap();
        layout
            .open()
            .unwrap()
            .create_task(CreateTaskInput {
                task_id: ID.parse().unwrap(),
                project_id: project.id().clone(),
                workspace: workspace.to_str().unwrap().into(),
                task: "implement".into(),
                request_id: "submit".into(),
                payload_hash: "hash".into(),
                base_head: None,
                allowed_paths: vec!["**".into()],
                test_commands: vec![],
                snapshot: Some(
                    bridge_git::take_snapshot(&workspace)
                        .unwrap()
                        .to_json()
                        .unwrap(),
                ),
            })
            .unwrap();
        Self {
            root,
            project,
            layout,
            state,
            stop,
            server: Some(server),
        }
    }
    fn reference(&self) -> RoundRef {
        RoundRef {
            task_id: ID.parse().unwrap(),
            project_id: self.project.id().clone(),
            round_number: 1,
        }
    }
    fn settings() -> WorkerSettings {
        WorkerSettings {
            deadline: Duration::from_secs(2),
            http_timeout: Duration::from_millis(200),
            observation: bridge_worker::observation_loop::ObservationSettings {
                poll_interval: Duration::from_millis(1),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn run(&self) -> Result<WorkerOutcome, WorkerRunError> {
        run_worker(
            &self.layout,
            &self.project,
            self.reference(),
            &[&self.layout],
            &[&self.project],
            &ServerCommand::opencode(),
            RuntimeOptions::default(),
            Self::settings(),
        )
    }
    fn task(&self) -> bridge_storage::Task {
        self.layout
            .open()
            .unwrap()
            .get_task(ID.parse().unwrap())
            .unwrap()
            .unwrap()
    }
    fn seed_sent(&self) {
        let mut storage = self.layout.open().unwrap();
        storage
            .bind_round_session(self.reference(), "session".into())
            .unwrap();
        storage
            .prepare_round(self.reference(), "outbound".into())
            .unwrap();
        storage.mark_round_sent(self.reference()).unwrap();
        self.state.lock().unwrap().outbound = Some("outbound".into());
    }
    fn posts(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| c.contains("prompt_async"))
            .count()
    }
    fn close_on(&self, path: &str) {
        let layout = self.layout.clone();
        let mut state = self.state.lock().unwrap();
        state.hook_path = Some(path.into());
        state.hook = Some(Box::new(move || {
            layout
                .open()
                .unwrap()
                .request_task_close(ID.parse().unwrap(), "close")
                .unwrap();
        }));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let server = self.server.take().unwrap().join();
        let cleanup = std::fs::remove_dir_all(&self.root);
        if !thread::panicking() {
            server.unwrap();
            cleanup.unwrap();
        }
    }
}
#[test]
fn pending_worker_dispatches_once_and_attempted_recovery_never_dispatches() {
    for resume in [false, true] {
        let f = Fixture::new();
        if resume {
            f.seed_sent();
        }
        assert_eq!(f.run().unwrap(), WorkerOutcome::Finished);
        assert_eq!(f.task().status, TaskStatus::AwaitingReview);
        assert_eq!(f.posts(), usize::from(!resume));
        let storage = f.layout.open().unwrap();
        let started: Option<String> = storage
            .connection()
            .query_row("SELECT worker_started_at FROM rounds", [], |r| r.get(0))
            .unwrap();
        assert!(started.is_some());
        assert_eq!(f.run().unwrap(), WorkerOutcome::Skipped);
        assert_eq!(f.posts(), usize::from(!resume));
    }
}
#[test]
fn ambiguous_post_is_observed_without_resend_and_missing_delivery_parks() {
    for delivered in [true, false] {
        let f = Fixture::new();
        {
            let mut s = f.state.lock().unwrap();
            s.post_status = 503;
            s.delivered = delivered;
        }
        let mut settings = Fixture::settings();
        settings.delivery_grace = Duration::ZERO;
        assert_eq!(
            run_worker(
                &f.layout,
                &f.project,
                f.reference(),
                &[&f.layout],
                &[&f.project],
                &ServerCommand::opencode(),
                RuntimeOptions::default(),
                settings
            )
            .unwrap(),
            WorkerOutcome::Finished
        );
        assert_eq!(
            f.task().status,
            if delivered {
                TaskStatus::AwaitingReview
            } else {
                TaskStatus::DeliveryUnknown
            }
        );
        assert_eq!(f.posts(), 1);
        assert_eq!(f.run().unwrap(), WorkerOutcome::Skipped);
        assert_eq!(f.posts(), 1);
    }
}
#[test]
fn wrong_remote_identity_never_sends_prompt_or_creates_session_before_workspace_proof() {
    for workspace in [true, false] {
        let f = Fixture::new();
        {
            let mut s = f.state.lock().unwrap();
            if workspace {
                s.directory = json!("/wrong-root");
            } else {
                s.session.1["directory"] = json!("/wrong-root");
            }
        }
        assert_eq!(f.run().unwrap(), WorkerOutcome::Finished);
        assert_eq!(f.task().status, TaskStatus::Failed);
        assert_eq!(f.posts(), 0);
        if workspace {
            assert_eq!(f.state.lock().unwrap().calls, vec!["GET /path"]);
        }
        let attempted: bool = f
            .layout
            .open()
            .unwrap()
            .connection()
            .query_row("SELECT attempted FROM rounds", [], |r| r.get(0))
            .unwrap();
        assert!(!attempted);
    }
}
#[test]
fn worker_close_at_start_identity_or_post_releases_fences_before_close() {
    for path in [None, Some("/path"), Some("/session/session/prompt_async")] {
        let f = Fixture::new();
        if let Some(path) = path {
            f.close_on(path);
        } else {
            f.layout
                .open()
                .unwrap()
                .request_task_close(ID.parse().unwrap(), "close")
                .unwrap();
        }
        assert_eq!(f.run().unwrap(), WorkerOutcome::Closed);
        assert_eq!(f.task().status, TaskStatus::Closed);
        assert!(matches!(
            WorkerLock::try_acquire(&f.layout).unwrap(),
            WorkerLockOutcome::Acquired(_)
        ));
        assert_eq!(
            f.posts(),
            usize::from(path == Some("/session/session/prompt_async"))
        );
    }
}
#[test]
fn busy_unknown_and_stale_inputs_have_no_http_or_start_write() {
    let f = Fixture::new();
    let guard = match WorkerLock::try_acquire(&f.layout).unwrap() {
        WorkerLockOutcome::Acquired(g) => g,
        _ => panic!("busy"),
    };
    assert_eq!(f.run().unwrap(), WorkerOutcome::Busy);
    assert!(f.state.lock().unwrap().calls.is_empty());
    drop(guard);
    let mut reference = f.reference();
    reference.round_number = 2;
    assert_eq!(
        run_worker(
            &f.layout,
            &f.project,
            reference,
            &[&f.layout],
            &[&f.project],
            &ServerCommand::opencode(),
            RuntimeOptions::default(),
            Fixture::settings()
        )
        .unwrap_err(),
        WorkerRunError::UnknownRound
    );
    let mut reference = f.reference();
    reference.task_id = "550e8400-e29b-41d4-a716-446655440001"
        .parse::<TaskId>()
        .unwrap();
    assert_eq!(
        run_worker(
            &f.layout,
            &f.project,
            reference,
            &[&f.layout],
            &[&f.project],
            &ServerCommand::opencode(),
            RuntimeOptions::default(),
            Fixture::settings()
        )
        .unwrap_err(),
        WorkerRunError::UnknownTask
    );
    assert!(f.state.lock().unwrap().calls.is_empty());
}
#[test]
fn settings_parse_defaults_and_finite_durations_without_global_environment_mutation() {
    assert_eq!(
        WorkerSettings::from_lookup(|_| None).unwrap().deadline,
        Duration::from_secs(3600)
    );
    let settings = WorkerSettings::from_lookup(|name| {
        Some(
            match name {
                "AB_POLL_INTERVAL" => "0.1",
                "AB_DELIVERY_GRACE" | "AB_STALE_BLOCKER_GRACE" => "0",
                _ => "invalid",
            }
            .into(),
        )
    })
    .unwrap();
    assert_eq!(
        settings.observation.poll_interval,
        Duration::from_millis(100)
    );
    assert_eq!(settings.delivery_grace, Duration::ZERO);
    for value in ["NaN", "inf", "-1", "0", "1e99"] {
        assert_eq!(
            WorkerSettings::from_lookup(|_| Some(value.into())).unwrap_err(),
            WorkerRunError::Settings
        );
    }
}

#[test]
fn unsent_session_failure_recovers_only_after_explicit_claim_and_preserves_atomic_failure() {
    let f = Fixture::new();
    // A saved dedicated session exists, but is temporarily unavailable before send.
    f.layout
        .open()
        .unwrap()
        .bind_round_session(f.reference(), "session".into())
        .unwrap();
    f.state.lock().unwrap().session.0 = 503;
    let mut settings = Fixture::settings();
    settings.deadline = Duration::from_millis(10);
    assert_eq!(
        run_worker(
            &f.layout,
            &f.project,
            f.reference(),
            &[&f.layout],
            &[&f.project],
            &ServerCommand::opencode(),
            RuntimeOptions::default(),
            settings
        )
        .unwrap(),
        WorkerOutcome::Finished
    );
    assert_eq!(f.task().status, TaskStatus::NeedsUser);
    assert_eq!(f.posts(), 0);
    assert_eq!(f.run().unwrap(), WorkerOutcome::Skipped);
    let mut storage = f.layout.open().unwrap();
    let claim = storage
        .claim_needs_user_recovery(ID.parse().unwrap(), f.project.id())
        .unwrap()
        .unwrap();
    assert_eq!(claim.round().status, bridge_domain::RoundStatus::Pending);
    assert!(!claim.round().attempted);
    assert!(storage.release_needs_user_recovery(&claim).unwrap());
    assert_eq!(f.task().status, TaskStatus::NeedsUser);
    storage
        .claim_needs_user_recovery(ID.parse().unwrap(), f.project.id())
        .unwrap()
        .unwrap();
    f.state.lock().unwrap().session.0 = 200;
    assert_eq!(f.run().unwrap(), WorkerOutcome::Finished);
    assert_eq!(f.posts(), 1);
    assert_eq!(f.task().status, TaskStatus::AwaitingReview);

    let f = Fixture::new();
    f.state.lock().unwrap().directory = json!("/wrong-root");
    f.layout.open().unwrap().connection().execute_batch("CREATE TRIGGER reject_failed BEFORE INSERT ON events WHEN NEW.kind='failed' BEGIN SELECT RAISE(ABORT,'blocked'); END;").unwrap();
    assert!(f.run().is_err());
    assert_eq!(f.posts(), 0);
    assert_eq!(f.task().status, TaskStatus::Implementing);
    let status: String = f
        .layout
        .open()
        .unwrap()
        .connection()
        .query_row("SELECT status FROM rounds", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "pending");
}

#[test]
fn pre_send_writer_refuses_arbitrary_or_attempted_terminal_transitions() {
    use bridge_domain::RoundStatus;
    use bridge_storage::FinishRoundInput;
    let f = Fixture::new();
    let mut storage = f.layout.open().unwrap();
    let mut input = FinishRoundInput {
        round: f.reference(),
        round_status: RoundStatus::Failed,
        task_status: TaskStatus::Failed,
        response_message_id: None,
        response: None,
        error_code: Some("assistant_error".into()),
        result_json: Some(json!({})),
    };
    assert!(storage.finish_worker_pre_send(input.clone(), None).is_err());
    input.error_code = Some("workspace_mismatch".into());
    f.seed_sent();
    assert!(storage.finish_worker_pre_send(input, None).is_err());
    assert_eq!(f.task().status, TaskStatus::Implementing);
    assert_eq!(f.posts(), 0);
}
