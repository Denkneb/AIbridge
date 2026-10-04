use bridge_config::{ProjectEntry, load_config};
use bridge_domain::TaskStatus;
use bridge_storage::{CreateTaskInput, RoundRef, RustStateLayout};
use bridge_worker::{WorkerLock, WorkerLockOutcome};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
const ID: &str = "550e8400-e29b-41d4-a716-446655440000";
struct Fixture {
    root: PathBuf,
    project: ProjectEntry,
    layout: RustStateLayout,
    calls: Arc<Mutex<Vec<String>>>,
    outbound: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-cli-worker-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );
        let password = root.join("password");
        fs::write(&password, "secret").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&password, fs::Permissions::from_mode(0o600)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let outbound = Arc::new(Mutex::new(None::<String>));
        let stop = Arc::new(AtomicBool::new(false));
        let (recorded, message_id, end, directory) = (
            calls.clone(),
            outbound.clone(),
            stop.clone(),
            workspace.clone(),
        );
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
                        let path = fields.next().unwrap().split('?').next().unwrap();
                        let length = text
                            .lines()
                            .filter_map(|l| l.split_once(':'))
                            .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                            .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                            .unwrap_or(0);
                        let mut payload = vec![0; length];
                        stream.read_exact(&mut payload).unwrap();
                        recorded.lock().unwrap().push(format!("{method} {path}"));
                        let (status, value) = match (method, path) {
                            ("GET", "/path") => (200, json!({"directory":directory})),
                            ("GET", "/session") => (200, json!([])),
                            ("POST", "/session") | ("GET", "/session/session") => {
                                (200, json!({"id":"session","directory":directory}))
                            }
                            ("POST", "/session/session/prompt_async") => {
                                let body: Value = serde_json::from_slice(&payload).unwrap();
                                *message_id.lock().unwrap() =
                                    Some(body["messageID"].as_str().unwrap().into());
                                (204, json!({}))
                            }
                            ("GET", "/session/session/message") => {
                                let id = message_id.lock().unwrap().clone().unwrap();
                                (
                                    200,
                                    json!([
                                        {"info":{"id":id,"role":"user","sessionID":"session"},"parts":[]},
                                        {"info":{"id":"answer","role":"assistant","parentID":id,"sessionID":"session","finish":"stop","time":{"completed":1}},"parts":[{"type":"text","text":"done"}]}
                                    ]),
                                )
                            }
                            _ => panic!("unexpected HTTP route"),
                        };
                        let payload = if status == 204 {
                            String::new()
                        } else {
                            value.to_string()
                        };
                        write!(stream,"HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",payload.len()).unwrap();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        let config = root.join("projects.toml");
        fs::write(&config,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:{port}\"\npassword_file={}\nmax_rounds=3\n",json!(workspace),json!(password))).unwrap();
        let project = load_config(&config)
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
            calls,
            outbound,
            stop,
            server: Some(server),
        }
    }
    fn base(&self, state: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        command
            .current_dir(&self.root)
            .args([
                "worker",
                "--project",
                "proj",
                "--config",
                "projects.toml",
                "--state-root",
            ])
            .arg(state)
            .env("AB_POLL_INTERVAL", "0.001")
            .env("AB_ROUND_DEADLINE", "2")
            .env("AB_HTTP_TIMEOUT", "1")
            .env("AB_DELIVERY_GRACE", "0.1")
            .env("AB_STALE_BLOCKER_GRACE", "0");
        command
    }
    fn command(&self) -> Command {
        let mut c = self.base(&self.root.join("state"));
        c.args(["--task", ID, "--round", "1"]);
        c
    }
    fn run(&self) -> Output {
        self.command().output().unwrap()
    }
    fn reference(&self) -> RoundRef {
        RoundRef {
            task_id: ID.parse().unwrap(),
            project_id: self.project.id().clone(),
            round_number: 1,
        }
    }
    fn status(&self) -> TaskStatus {
        self.layout
            .open()
            .unwrap()
            .get_task(ID.parse().unwrap())
            .unwrap()
            .unwrap()
            .status
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.server.take().unwrap().join().unwrap();
        fs::remove_dir_all(&self.root).unwrap();
    }
}
#[test]
fn worker_cli_dispatches_or_resumes_and_terminal_repeat_has_no_http() {
    for resume in [false, true] {
        let f = Fixture::new();
        if resume {
            let mut s = f.layout.open().unwrap();
            s.bind_round_session(f.reference(), "session".into())
                .unwrap();
            s.prepare_round(f.reference(), "outbound".into()).unwrap();
            s.mark_round_sent(f.reference()).unwrap();
            *f.outbound.lock().unwrap() = Some("outbound".into());
        }
        let output = f.run();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert_eq!(f.status(), TaskStatus::AwaitingReview);
        let calls = f.calls.lock().unwrap().clone();
        assert_eq!(
            calls.iter().filter(|p| p.contains("prompt_async")).count(),
            usize::from(!resume)
        );
        assert!(f.run().status.success());
        assert_eq!(*f.calls.lock().unwrap(), calls);
    }
}
#[test]
fn worker_cli_requires_valid_task_round_and_rejects_duplicates_without_echoing_values() {
    let f = Fixture::new();
    for args in [
        vec![],
        vec!["--task", "SECRET_INPUT", "--round", "1"],
        vec!["--task", ID, "--round", "0"],
        vec!["--task", ID, "--round", "4294967296"],
        vec!["--task", ID, "--round", "1", "--round", "2"],
    ] {
        let output = f.base(&f.root.join("state")).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SECRET_INPUT"));
        assert!(output.stdout.is_empty());
    }
    assert!(f.calls.lock().unwrap().is_empty());
    assert_eq!(f.status(), TaskStatus::Implementing);
}
#[test]
fn worker_cli_reports_unknown_busy_unowned_and_invalid_settings_with_stable_codes() {
    let f = Fixture::new();
    let output = f
        .base(&f.root.join("state"))
        .args([
            "--task",
            "550e8400-e29b-41d4-a716-446655440001",
            "--round",
            "1",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let output = f
        .base(&f.root.join("state"))
        .args(["--task", ID, "--round", "2"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let guard = match WorkerLock::try_acquire(&f.layout).unwrap() {
        WorkerLockOutcome::Acquired(g) => g,
        _ => panic!("busy"),
    };
    assert_eq!(f.run().status.code(), Some(3));
    drop(guard);
    let output = f
        .base(&f.root.join("foreign-state"))
        .args(["--task", ID, "--round", "1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!f.root.join("foreign-state").exists());
    let output = f.command().env("AB_POLL_INTERVAL", "NaN").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(f.calls.lock().unwrap().is_empty());
}
#[test]
fn worker_cli_completes_requested_close_without_http() {
    let f = Fixture::new();
    f.layout
        .open()
        .unwrap()
        .request_task_close(ID.parse().unwrap(), "close")
        .unwrap();
    let output = f.run();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(f.status(), TaskStatus::Closed);
    assert!(f.calls.lock().unwrap().is_empty());
}
