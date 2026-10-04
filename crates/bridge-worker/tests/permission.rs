//! Integration tests for the current-round permission blocker (task 7.6).
//!
//! Every test runs against a loopback mock HTTP server plus a fresh Rust-owned
//! SQLite state under a temporary root. No live OpenCode server, external
//! network, Python state or long-lived service is used: the mock server owns its
//! listener/thread and is stopped and joined on drop, and every temporary root is
//! removed on drop.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bridge_config::load_config;
use bridge_domain::{ProjectId, RoundStatus, TaskId, TaskStatus};
use bridge_opencode::OpenCodeClient;
use bridge_storage::{CreateTaskInput, RoundRef, StorageConnection, connect};
use bridge_worker::{
    PermissionBlockerErrorKind, UserActionKind, handle_permission_blocker, round_session_title,
};

const PASSWORD: &str = "test-password";
const PASSWORD_BASE64: &str = "b3BlbmNvZGU6dGVzdC1wYXNzd29yZA==";
const TASK_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

// ---------------------------------------------------------------------------
// Temporary directories
// ---------------------------------------------------------------------------

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bridge-worker-permission-{tag}-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("temporary directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn mkdir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        std::fs::create_dir_all(&path).expect("temporary subdirectory");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn set_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("credential mode");
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

// ---------------------------------------------------------------------------
// Loopback mock OpenCode server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RecordedRequest {
    method: String,
    target: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

impl RecordedRequest {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    fn query(&self) -> Option<&str> {
        self.target.split_once('?').map(|(_, query)| query)
    }
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
}

struct ServerHandle {
    state: Arc<Mutex<MockState>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ServerHandle {
    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().expect("state").requests.clone()
    }

    fn request_count(&self) -> usize {
        self.state.lock().expect("state").requests.len()
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_server<F>(handler: F) -> (u16, ServerHandle)
where
    F: Fn(&RecordedRequest, &mut MockState) -> Vec<u8> + Send + 'static,
{
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    listener
        .set_nonblocking(true)
        .expect("non-blocking listener");
    let state = Arc::new(Mutex::new(MockState::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread_state = Arc::clone(&state);
    let thread_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !thread_stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).expect("blocking stream");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("read timeout");
                    let Some(request) = read_request(&mut stream) else {
                        continue;
                    };
                    let response = {
                        let mut guard = thread_state.lock().expect("state");
                        guard.requests.push(request.clone());
                        handler(&request, &mut guard)
                    };
                    let _ = stream.write_all(&response);
                    let _ = stream.flush();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break,
            }
        }
    });
    (
        port,
        ServerHandle {
            state,
            stop,
            handle: Some(handle),
        },
    )
}

fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut data = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                data.extend_from_slice(&chunk[..count]);
                if let Some(head_end) = find_subslice(&data, b"\r\n\r\n") {
                    let length = content_length(&data[..head_end]).unwrap_or(0);
                    if data.len() >= head_end + 4 + length {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    if data.is_empty() {
        return None;
    }
    let head_end = find_subslice(&data, b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&data[..head_end]);
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut authorization = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("authorization")
        {
            authorization = Some(value.trim().to_owned());
        }
    }
    Some(RecordedRequest {
        method,
        target,
        authorization,
        body: data[head_end + 4..].to_vec(),
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(head: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(head);
    for line in text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse().ok();
        }
    }
    None
}

fn ok_json(value: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(value).expect("serialize");
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    response
}

fn status(code: u16, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {code} Error\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn permission_json(id: &str, session: &str, name: &str, patterns: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "sessionID": session,
        "permission": name,
        "patterns": patterns,
        "metadata": {},
        "always": [],
    })
}

// ---------------------------------------------------------------------------
// Rust state + client helpers
// ---------------------------------------------------------------------------

fn project(id: &str) -> ProjectId {
    ProjectId::from_str(id).expect("project id")
}

fn task_id() -> TaskId {
    TaskId::from_str(TASK_UUID).expect("task id")
}

fn round_ref(task: TaskId, project: &ProjectId, number: u32) -> RoundRef {
    RoundRef {
        task_id: task,
        project_id: project.clone(),
        round_number: number,
    }
}

fn rust_layout(dir: &TempDir, id: &str) -> bridge_storage::RustStateLayout {
    bridge_storage::RustStateLayout::new(dir.path().join("state"), project(id)).expect("layout")
}

fn open_storage(dir: &TempDir, id: &str) -> (bridge_storage::RustStateLayout, StorageConnection) {
    let layout = rust_layout(dir, id);
    layout.initialize().expect("initialize rust state");
    let storage = layout.open().expect("open rust state");
    (layout, storage)
}

fn create_task(
    storage: &mut StorageConnection,
    task: TaskId,
    project: &ProjectId,
    workspace: &str,
) {
    storage
        .create_task(CreateTaskInput {
            task_id: task,
            project_id: project.clone(),
            workspace: workspace.to_owned(),
            task: "do work".to_owned(),
            request_id: format!("req-{task}"),
            payload_hash: "hash".to_owned(),
            base_head: None,
            allowed_paths: Vec::new(),
            test_commands: Vec::new(),
            snapshot: None,
        })
        .expect("create task");
}

/// Creates a task, binds the current round session and moves it to `observing`.
fn seed_observing(
    storage: &mut StorageConnection,
    task: TaskId,
    project: &ProjectId,
    workspace: &str,
    session: &str,
) {
    create_task(storage, task, project, workspace);
    storage
        .bind_round_session(round_ref(task, project, 1), session.to_owned())
        .expect("bind session");
    storage
        .mark_round_observing(round_ref(task, project, 1))
        .expect("observing");
}

fn round_status(storage: &StorageConnection, task: TaskId, number: u32) -> String {
    storage
        .connection()
        .query_row(
            "SELECT status FROM rounds WHERE task_id = ?1 AND round_number = ?2",
            rusqlite::params![task.to_string(), i64::from(number)],
            |row| row.get(0),
        )
        .expect("round status")
}

fn round_updated_at(storage: &StorageConnection, task: TaskId, number: u32) -> String {
    storage
        .connection()
        .query_row(
            "SELECT updated_at FROM rounds WHERE task_id = ?1 AND round_number = ?2",
            rusqlite::params![task.to_string(), i64::from(number)],
            |row| row.get(0),
        )
        .expect("round updated_at")
}

fn task_status(storage: &StorageConnection, task: TaskId) -> TaskStatus {
    storage
        .get_task(task)
        .expect("get task")
        .expect("task exists")
        .status
}

fn task_close_requested_at(storage: &StorageConnection, task: TaskId) -> Option<String> {
    storage
        .connection()
        .query_row(
            "SELECT close_requested_at FROM tasks WHERE task_id = ?1",
            rusqlite::params![task.to_string()],
            |row| row.get(0),
        )
        .expect("task close_requested_at")
}

fn round_error_code(storage: &StorageConnection, task: TaskId, number: u32) -> Option<String> {
    storage
        .connection()
        .query_row(
            "SELECT error_code FROM rounds WHERE task_id = ?1 AND round_number = ?2",
            rusqlite::params![task.to_string(), i64::from(number)],
            |row| row.get(0),
        )
        .expect("round error_code")
}

fn round_result_json(storage: &StorageConnection, task: TaskId, number: u32) -> Option<String> {
    storage
        .connection()
        .query_row(
            "SELECT result_json FROM rounds WHERE task_id = ?1 AND round_number = ?2",
            rusqlite::params![task.to_string(), i64::from(number)],
            |row| row.get(0),
        )
        .expect("round result_json")
}

fn event_count(storage: &StorageConnection, task: TaskId, kind: &str) -> i64 {
    storage
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE task_id = ?1 AND kind = ?2",
            rusqlite::params![task.to_string(), kind],
            |row| row.get(0),
        )
        .expect("event count")
}

fn build_client(port: u16, workspace: &Path) -> OpenCodeClient {
    let dir = TempDir::new("client");
    let password = dir.path().join("proj.password");
    std::fs::write(&password, format!("{PASSWORD}\n")).expect("password file");
    set_private(&password);
    let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
    let config = format!(
        "[projects.proj]\nworkspace = \"{}\"\nopencode_url = \"http://127.0.0.1:{port}\"\npassword_file = \"{}\"\nmax_rounds = 3\n",
        workspace.display(),
        password.display(),
    );
    let config_path = dir.path().join("projects.toml");
    std::fs::write(&config_path, config).expect("config file");
    let loaded = load_config(&config_path).expect("config must load");
    let entry = loaded.project("proj").expect("project must exist");
    OpenCodeClient::from_project(entry, Duration::from_secs(5)).expect("client")
}

fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn assert_scoped_auth(request: &RecordedRequest, workspace: &Path) {
    assert_eq!(
        request.authorization.as_deref(),
        Some(&format!("Basic {PASSWORD_BASE64}")[..])
    );
    let query = request.query().expect("scoped query");
    let directory = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("directory="))
        .expect("directory query parameter");
    let expected = std::fs::canonicalize(workspace).expect("canonical workspace");
    assert_eq!(form_decode(directory), expected.to_string_lossy());
}

fn canonical_workspace(dir: &TempDir) -> (PathBuf, String) {
    let workspace = dir.mkdir("ws");
    let canonical = std::fs::canonicalize(&workspace).expect("canonical workspace");
    let string = canonical.to_string_lossy().into_owned();
    (workspace, string)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn current_session_permission_persists_needs_user_and_user_action() {
    let dir = TempDir::new("block");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");
    let events_before = event_count(&storage, task, "needs_user");
    let updated_before = round_updated_at(&storage, task, 1);

    let (port, server) =
        spawn_server(
            |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/permission") => ok_json(&serde_json::json!([
                    permission_json("perm-1", "ses_cur", "bash", &["ls"]),
                    permission_json("perm-2", "ses_other", "edit", &["a.txt"]),
                ])),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let outcome = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect("the blocker must be persisted");
    let blocker = outcome.blocked().expect("a blocker is present");
    let title = round_session_title(task, 1);
    assert_eq!(blocker.session_id(), "ses_cur");
    assert_eq!(blocker.session_title(), title);
    assert_eq!(blocker.permissions().len(), 1);
    assert_eq!(blocker.permissions()[0].permission(), Some("bash"));
    assert_eq!(blocker.permissions()[0].patterns(), ["ls"]);

    let action = blocker.user_action();
    assert_eq!(action.kind(), UserActionKind::OpenProjectConsole);
    assert_eq!(action.session_id(), Some("ses_cur"));
    assert_eq!(action.session_title(), Some(title.as_str()));
    assert_eq!(action.command(), None);
    assert_eq!(action.fallback_command(), None);
    assert!(action.instructions().contains("permission"));
    assert!(action.message().contains("OpenCode"));

    assert_eq!(blocker.round().status, RoundStatus::NeedsUser);
    assert_eq!(blocker.task().status, TaskStatus::NeedsUser);
    assert_eq!(blocker.round().error_code.as_deref(), Some("needs_user"));
    let result = blocker
        .round()
        .result_json
        .clone()
        .expect("persisted result");
    let blockers = result["blockers"].as_array().expect("blockers array");
    assert_eq!(blockers.len(), 1, "only the current session blocks");
    assert_eq!(blockers[0]["type"], "permission");
    assert_eq!(blockers[0]["permission"], "bash");
    assert_eq!(blockers[0]["patterns"], serde_json::json!(["ls"]));
    assert_eq!(blockers[0]["reason"], "auto_approval_disabled");

    // No permission is ever answered: only one scoped GET, never a POST.
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path(), "/permission");
    assert_scoped_auth(&requests[0], &workspace);
    assert!(requests.iter().all(|request| request.method != "POST"));

    assert_eq!(round_status(&storage, task, 1), "needs_user");
    assert_eq!(task_status(&storage, task), TaskStatus::NeedsUser);
    assert_eq!(event_count(&storage, task, "needs_user"), events_before + 1);
    assert_ne!(round_updated_at(&storage, task, 1), updated_before);

    // The outcome rendering must not leak the session or permission content.
    let rendered = format!("{outcome} {outcome:?}");
    assert!(!rendered.contains("ses_cur"));
    assert!(!rendered.contains("ses_other"));
    assert!(!rendered.contains("bash"));
}

#[test]
fn empty_permission_list_is_a_no_op() {
    let dir = TempDir::new("empty");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");
    let events_before = event_count(&storage, task, "needs_user");
    let updated_before = round_updated_at(&storage, task, 1);

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let outcome = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect("an empty list is not an error");
    assert!(!outcome.is_blocked());
    assert_eq!(server.request_count(), 1);

    assert_eq!(round_status(&storage, task, 1), "observing");
    assert_eq!(task_status(&storage, task), TaskStatus::Implementing);
    assert_eq!(event_count(&storage, task, "needs_user"), events_before);
    assert_eq!(round_updated_at(&storage, task, 1), updated_before);
}

#[test]
fn foreign_session_permission_is_a_no_op() {
    let dir = TempDir::new("foreign");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");
    let events_before = event_count(&storage, task, "needs_user");

    let (port, server) = spawn_server(|_request, _state| {
        ok_json(&serde_json::json!([permission_json(
            "perm-2",
            "ses_other",
            "bash",
            &["rm -rf /"],
        )]))
    });
    let client = build_client(port, &workspace);

    let outcome = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect("a foreign session is not an error");
    assert!(!outcome.is_blocked());
    assert_eq!(server.request_count(), 1);
    assert_eq!(round_status(&storage, task, 1), "observing");
    assert_eq!(task_status(&storage, task), TaskStatus::Implementing);
    assert_eq!(event_count(&storage, task, "needs_user"), events_before);
}

#[test]
fn repeat_is_idempotent_without_duplicate_lifecycle_side_effects() {
    let dir = TempDir::new("repeat");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");

    let (port, server) = spawn_server(|_request, _state| {
        ok_json(&serde_json::json!([permission_json(
            "perm-1",
            "ses_cur",
            "bash",
            &["ls"],
        )]))
    });
    let client = build_client(port, &workspace);

    let first = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect("first call persists");
    assert!(first.is_blocked());
    let updated_after_first = round_updated_at(&storage, task, 1);
    let events_after_first = event_count(&storage, task, "needs_user");

    let second = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect("second call replays");
    let blocker = second.blocked().expect("still blocked");
    assert_eq!(blocker.round().status, RoundStatus::NeedsUser);
    assert_eq!(blocker.task().status, TaskStatus::NeedsUser);

    assert_eq!(
        round_updated_at(&storage, task, 1),
        updated_after_first,
        "the replay must not move updated_at"
    );
    assert_eq!(
        event_count(&storage, task, "needs_user"),
        events_after_first,
        "the replay must not append a second needs_user event"
    );
    assert_eq!(event_count(&storage, task, "needs_user"), 1);
    assert_eq!(server.request_count(), 2, "two scoped permission GETs");
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[test]
fn close_during_permission_get_is_not_a_false_blocker() {
    let dir = TempDir::new("close-during-get");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");
    let events_before = event_count(&storage, task, "needs_user");
    let updated_before = round_updated_at(&storage, task, 1);

    let storage = Arc::new(Mutex::new(storage));
    let handler_storage = Arc::clone(&storage);
    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/permission") => {
                    // A cooperative close writer does not take the WorkerLock, so
                    // the close can land while the permission GET is in flight.
                    handler_storage
                        .lock()
                        .expect("storage")
                        .request_task_close(task, "stop")
                        .expect("close request");
                    ok_json(&serde_json::json!([permission_json(
                        "perm-1",
                        "ses_cur",
                        "bash",
                        &["ls"],
                    )]))
                }
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a close during GET must not report a blocker");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::CloseRequested);
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains("ses_cur"));
    assert!(!rendered.contains("stop"));

    assert_eq!(server.request_count(), 1);
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.method == "GET"),
        "no permission POST is ever sent"
    );

    let storage = storage.lock().expect("storage");
    assert!(
        task_close_requested_at(&storage, task).is_some(),
        "the close request is persisted"
    );
    assert_eq!(round_status(&storage, task, 1), "observing");
    assert_eq!(task_status(&storage, task), TaskStatus::Implementing);
    assert_eq!(event_count(&storage, task, "needs_user"), events_before);
    assert_eq!(round_updated_at(&storage, task, 1), updated_before);
}

#[test]
fn close_during_permission_get_on_replay_is_not_a_false_blocker() {
    let dir = TempDir::new("close-during-replay");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");

    let storage = Arc::new(Mutex::new(storage));
    let handler_storage = Arc::clone(&storage);
    let (port, server) = spawn_server(move |_request, state| {
        if state.requests.len() == 2 {
            handler_storage
                .lock()
                .expect("storage")
                .request_task_close(task, "stop")
                .expect("close request");
        }
        ok_json(&serde_json::json!([permission_json(
            "perm-1",
            "ses_cur",
            "bash",
            &["ls"],
        )]))
    });
    let client = build_client(port, &workspace);

    let first = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect("the first call persists needs_user");
    assert!(first.is_blocked());
    let (updated_after_first, events_after_first) = {
        let storage = storage.lock().expect("storage");
        (
            round_updated_at(&storage, task, 1),
            event_count(&storage, task, "needs_user"),
        )
    };
    assert_eq!(events_after_first, 1);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a close during the replay GET must not report a blocker");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::CloseRequested);

    let storage = storage.lock().expect("storage");
    assert!(
        task_close_requested_at(&storage, task).is_some(),
        "the close request is persisted"
    );
    assert_eq!(
        event_count(&storage, task, "needs_user"),
        events_after_first,
        "the replay must not append a duplicate needs_user event"
    );
    assert_eq!(
        round_updated_at(&storage, task, 1),
        updated_after_first,
        "the replay must not move updated_at"
    );
    assert_eq!(server.request_count(), 2, "two scoped permission GETs");
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.method == "GET"),
        "no permission POST is ever sent"
    );
}

#[test]
fn close_landing_during_finish_round_is_not_a_false_blocker() {
    let dir = TempDir::new("close-during-finish");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");

    // Simulate the last close window: the cooperative close lands between the
    // post-HTTP revalidation and the atomic `finish_round` write, so only the
    // committed task returned by `finish_round` can reveal it.
    storage
        .connection()
        .execute_batch(&format!(
            "CREATE TRIGGER close_during_finish BEFORE INSERT ON events \
             WHEN NEW.kind = 'needs_user' AND NEW.task_id = '{}' \
             BEGIN UPDATE tasks SET status = 'closed', \
             close_requested_at = '2020-01-01T00:00:00.000Z' \
             WHERE task_id = NEW.task_id; END;",
            task
        ))
        .expect("close race trigger");

    let (port, server) = spawn_server(|_request, _state| {
        ok_json(&serde_json::json!([permission_json(
            "perm-1",
            "ses_cur",
            "bash",
            &["ls"],
        )]))
    });
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a task closed during the atomic write must not report Blocked");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::CloseRequested);
    assert_eq!(server.request_count(), 1);
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.method == "GET")
    );
    assert_eq!(
        task_status(&storage, task),
        TaskStatus::Closed,
        "the committed close is the authority"
    );
}

#[test]
fn storage_failure_rolls_back_needs_user_and_is_typed() {
    let dir = TempDir::new("storage-fail");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");
    let events_before = event_count(&storage, task, "needs_user");
    let updated_before = round_updated_at(&storage, task, 1);

    // Test-only trigger: reject the `needs_user` lifecycle/event write inside
    // the `finish_round` transaction. Production storage/schema is untouched.
    storage
        .connection()
        .execute_batch(&format!(
            "CREATE TRIGGER reject_needs_user BEFORE INSERT ON events \
             WHEN NEW.kind = 'needs_user' AND NEW.task_id = '{}' \
             BEGIN SELECT RAISE(ABORT, 'rejected for test'); END;",
            task
        ))
        .expect("reject trigger");

    let (port, server) = spawn_server(|_request, _state| {
        ok_json(&serde_json::json!([permission_json(
            "perm-1",
            "ses_cur",
            "bash",
            &["ls"],
        )]))
    });
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a storage failure must be typed, never a false success");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::Storage);
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains("ses_cur"));
    assert!(!rendered.contains("rejected for test"));

    // The single atomic write rolled back completely: round, task, error_code,
    // result_json and events all stay untouched.
    assert_eq!(round_status(&storage, task, 1), "observing");
    assert_eq!(task_status(&storage, task), TaskStatus::Implementing);
    assert_eq!(round_error_code(&storage, task, 1), None);
    assert_eq!(round_result_json(&storage, task, 1), None);
    assert_eq!(event_count(&storage, task, "needs_user"), events_before);
    assert_eq!(round_updated_at(&storage, task, 1), updated_before);

    assert_eq!(server.request_count(), 1);
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.method == "GET"),
        "no permission POST is ever sent"
    );
}

#[test]
fn permission_transport_error_is_typed_and_redacted() {
    let dir = TempDir::new("transport");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");
    let events_before = event_count(&storage, task, "needs_user");

    let (port, server) = spawn_server(|_request, _state| status(500, b"boom"));
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a failed permission list must fail closed");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::Permissions);
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains("ses_cur"));
    assert!(!rendered.contains("boom"));
    assert_eq!(server.request_count(), 1);
    assert_eq!(round_status(&storage, task, 1), "observing");
    assert_eq!(task_status(&storage, task), TaskStatus::Implementing);
    assert_eq!(event_count(&storage, task, "needs_user"), events_before);
}

#[test]
fn malformed_permission_list_is_typed() {
    let dir = TempDir::new("malformed");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");

    let (port, _server) =
        spawn_server(|_request, _state| ok_json(&serde_json::json!({ "not": "a list" })));
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a malformed list must fail closed");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::Permissions);
    assert_eq!(round_status(&storage, task, 1), "observing");
}

#[test]
fn unmarked_state_is_rejected_before_http() {
    let dir = TempDir::new("unmarked");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let project = project("proj-1");
    let task = task_id();
    let layout = rust_layout(&dir, "proj-1");
    // A schema-v6 database without the Rust sidecar marker must never be adopted.
    bridge_storage::initialize(layout.database()).expect("schema v6 database");
    {
        let mut storage = connect(layout.database()).expect("generic connection");
        seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");
    }
    let database_before = std::fs::read(layout.database()).expect("db bytes");
    assert!(
        !layout.marker().exists(),
        "the state is deliberately unmarked"
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect_err("an unmarked state must be rejected");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::StateOwnership);
    assert_eq!(
        server.request_count(),
        0,
        "no HTTP before the ownership guard"
    );
    assert!(!layout.marker().exists(), "no marker may be created");
    assert_eq!(
        std::fs::read(layout.database()).expect("db bytes"),
        database_before,
        "the foreign database must be unchanged"
    );
}

#[test]
fn missing_current_session_is_session_unknown() {
    let dir = TempDir::new("no-session");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(&mut storage, task, &project, &workspace_string);
    // Move to observing without binding a session.
    storage
        .mark_round_observing(round_ref(task, &project, 1))
        .expect("observing");

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a missing session must fail closed");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::SessionUnknown);
    assert_eq!(server.request_count(), 0, "no HTTP without a session");
    assert_eq!(round_status(&storage, task, 1), "observing");
}

#[test]
fn pending_close_request_is_refused() {
    let dir = TempDir::new("close");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &project, &workspace_string, "ses_cur");
    storage
        .request_task_close(task, "stop")
        .expect("close request");
    let events_before = event_count(&storage, task, "needs_user");

    let (port, server) = spawn_server(|_request, _state| {
        ok_json(&serde_json::json!([permission_json(
            "perm-1",
            "ses_cur",
            "bash",
            &["ls"],
        )]))
    });
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a pending close must fail closed");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::CloseRequested);
    assert_eq!(server.request_count(), 0, "no HTTP with a pending close");
    assert_eq!(round_status(&storage, task, 1), "observing");
    assert_eq!(event_count(&storage, task, "needs_user"), events_before);
}

#[test]
fn non_observing_round_is_rejected_before_http() {
    let dir = TempDir::new("pending-round");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(&mut storage, task, &project, &workspace_string);

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a pending round must be rejected");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::RoundNotObservable);
    assert_eq!(
        server.request_count(),
        0,
        "no HTTP for a non-observing round"
    );
}

#[test]
fn layout_project_mismatch_is_rejected_before_http() {
    let dir = TempDir::new("project-mismatch");
    let (workspace, workspace_string) = canonical_workspace(&dir);
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let proj1 = project("proj-1");
    let task = task_id();
    seed_observing(&mut storage, task, &proj1, &workspace_string, "ses_cur");
    let other = project("proj-2");

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &other, 1))
        .expect_err("a mismatched project must be rejected");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::TaskMismatch);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn zero_round_is_rejected_before_http() {
    let dir = TempDir::new("zero-round");
    let (workspace, _workspace_string) = canonical_workspace(&dir);
    let (layout, _storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = handle_permission_blocker(&client, &layout, round_ref(task, &project, 0))
        .expect_err("round zero must be rejected");
    assert_eq!(error.kind(), PermissionBlockerErrorKind::InvalidInput);
    assert_eq!(server.request_count(), 0);
}

fn approval_project(
    dir: &TempDir,
    workspace: &Path,
    port: u16,
    state: &Path,
    extra: &str,
) -> bridge_config::ProjectEntry {
    let config = dir.path().join("auto-project.toml");
    std::fs::write(&config,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:{port}\"\npassword_file=\"unused\"\nmax_rounds=3\n{extra}\n",serde_json::json!(workspace))).unwrap();
    bridge_config::load_config_with_state_root(&config, state)
        .unwrap()
        .project("proj")
        .unwrap()
        .clone()
}
use bridge_worker::permission::{PermissionReplies, handle_permission_blocker_with_auto_approval};

#[test]
fn auto_approval_replies_once_only_to_current_session_and_keeps_observing_when_clear() {
    let dir = TempDir::new("auto-once");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj");
    let task = task_id();
    let project_id = project("proj");
    seed_observing(
        &mut storage,
        task,
        &project_id,
        workspace.to_str().unwrap(),
        "current",
    );
    let pending = serde_json::json!([
        permission_json("approved", "current", "read", &[]),
        permission_json("approved", "current", "read", &[]),
        permission_json("foreign", "other", "read", &[]),
        permission_json(
            "state",
            "current",
            "external_directory",
            &[layout.project_dir().to_str().unwrap()]
        ),
    ]);
    let (port, server) = spawn_server(move |request, _| {
        if request.method == "GET" {
            ok_json(&pending)
        } else {
            ok_json(&serde_json::json!({}))
        }
    });
    let client = build_client(port, &workspace);
    let config = approval_project(
        &dir,
        &workspace,
        port,
        layout.state_root(),
        "auto_approve_permissions=[\"read\"]\nauto_approve_state_directory=true",
    );
    let roots = config.auto_approve_external_directories().to_vec();
    let mut replies = PermissionReplies::default();
    for _ in 0..2 {
        assert!(
            !handle_permission_blocker_with_auto_approval(
                &client,
                &layout,
                round_ref(task, &project_id, 1),
                &config,
                &mut replies
            )
            .unwrap()
            .is_blocked()
        );
    }
    let posts: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect();
    assert_eq!(posts.len(), 2);
    for request in &posts {
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
            serde_json::json!({"reply":"once"})
        );
        assert!(!request.path().contains("foreign"));
    }
    assert_eq!(replies.approvals().len(), 2);
    assert!(!format!("{replies:?}").contains("current"));
    assert_eq!(round_status(&storage, task, 1), "observing");
    assert_eq!(task_status(&storage, task), TaskStatus::Implementing);
    assert_eq!(event_count(&storage, task, "needs_user"), 0);
    assert_eq!(config.auto_approve_external_directories(), roots);
}

#[test]
fn auto_approval_defaults_and_denied_bash_preserve_blockers_and_success_evidence() {
    for configured in [false, true] {
        let dir = TempDir::new("auto-mixed");
        let workspace = dir.mkdir("ws");
        let (layout, mut storage) = open_storage(&dir, "proj");
        let task = task_id();
        let project_id = project("proj");
        seed_observing(
            &mut storage,
            task,
            &project_id,
            workspace.to_str().unwrap(),
            "current",
        );
        let pending = serde_json::json!([
            permission_json("read", "current", "read", &[]),
            permission_json("git", "current", "bash", &["git push"])
        ]);
        let (port, server) = spawn_server(move |request, _| {
            if request.method == "GET" {
                ok_json(&pending)
            } else {
                ok_json(&serde_json::json!({}))
            }
        });
        let client = build_client(port, &workspace);
        let config = approval_project(
            &dir,
            &workspace,
            port,
            layout.state_root(),
            if configured {
                "auto_approve_permissions=[\"read\",\"bash\"]"
            } else {
                ""
            },
        );
        let mut replies = PermissionReplies::default();
        let outcome = handle_permission_blocker_with_auto_approval(
            &client,
            &layout,
            round_ref(task, &project_id, 1),
            &config,
            &mut replies,
        )
        .unwrap();
        assert!(outcome.is_blocked());
        let blocked = outcome.blocked().unwrap();
        assert_eq!(blocked.permissions().len(), if configured { 1 } else { 2 });
        assert_eq!(
            blocked.permissions().last().unwrap().reason(),
            if configured {
                "git_write_blocked"
            } else {
                "not_configured"
            }
        );
        let result: serde_json::Value =
            serde_json::from_str(&round_result_json(&storage, task, 1).unwrap()).unwrap();
        if configured {
            assert_eq!(
                result["auto_approved"],
                serde_json::json!([{"id":"read","permission":"read"}])
            );
        } else {
            assert!(result.get("auto_approved").is_none());
        }
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|request| request.method == "POST")
                .count(),
            usize::from(configured)
        );
        assert_eq!(task_status(&storage, task), TaskStatus::NeedsUser);
        assert!(!format!("{outcome:?}").contains("git push"));
    }
}

#[test]
fn failed_auto_reply_is_redacted_blocker_and_never_marked_as_success() {
    let dir = TempDir::new("auto-failure");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj");
    let task = task_id();
    let project_id = project("proj");
    seed_observing(
        &mut storage,
        task,
        &project_id,
        workspace.to_str().unwrap(),
        "current",
    );
    let pending = serde_json::json!([permission_json("request", "current", "read", &[])]);
    let (port, server) = spawn_server(move |request, _| {
        if request.method == "GET" {
            ok_json(&pending)
        } else {
            status(500, b"secret-error-body")
        }
    });
    let client = build_client(port, &workspace);
    let config = approval_project(
        &dir,
        &workspace,
        port,
        layout.state_root(),
        "auto_approve_permissions=[\"read\"]",
    );
    let mut replies = PermissionReplies::default();
    for _ in 0..2 {
        let outcome = handle_permission_blocker_with_auto_approval(
            &client,
            &layout,
            round_ref(task, &project_id, 1),
            &config,
            &mut replies,
        )
        .unwrap();
        assert_eq!(
            outcome.blocked().unwrap().permissions()[0].reason(),
            "reply_failed"
        );
        assert!(replies.approvals().is_empty());
        assert!(
            !round_result_json(&storage, task, 1)
                .unwrap()
                .contains("secret-error-body")
        );
    }
    assert_eq!(event_count(&storage, task, "needs_user"), 1);
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        2
    );
}

#[test]
fn close_during_get_or_first_reply_stops_further_auto_replies() {
    for close_method in ["GET", "POST"] {
        let dir = TempDir::new("auto-close");
        let workspace = dir.mkdir("ws");
        let (layout, mut storage) = open_storage(&dir, "proj");
        let task = task_id();
        let project_id = project("proj");
        seed_observing(
            &mut storage,
            task,
            &project_id,
            workspace.to_str().unwrap(),
            "current",
        );
        let pending = serde_json::json!([
            permission_json("first", "current", "read", &[]),
            permission_json("second", "current", "read", &[])
        ]);
        let database = layout.database();
        let (port, server) = spawn_server(move |request, _| {
            if request.method == close_method {
                connect(&database)
                    .unwrap()
                    .request_task_close(task, "user-close")
                    .unwrap();
            }
            if request.method == "GET" {
                ok_json(&pending)
            } else {
                ok_json(&serde_json::json!({}))
            }
        });
        let client = build_client(port, &workspace);
        let config = approval_project(
            &dir,
            &workspace,
            port,
            layout.state_root(),
            "auto_approve_permissions=[\"read\"]",
        );
        let mut replies = PermissionReplies::default();
        let error = handle_permission_blocker_with_auto_approval(
            &client,
            &layout,
            round_ref(task, &project_id, 1),
            &config,
            &mut replies,
        )
        .unwrap_err();
        assert_eq!(error.kind(), PermissionBlockerErrorKind::CloseRequested);
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|request| request.method == "POST")
                .count(),
            usize::from(close_method == "POST")
        );
        assert_eq!(event_count(&storage, task, "needs_user"), 0);
    }
}

#[test]
fn successful_reply_survives_storage_rollback_without_a_second_post() {
    let dir = TempDir::new("auto-rollback");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj");
    let task = task_id();
    let project_id = project("proj");
    seed_observing(
        &mut storage,
        task,
        &project_id,
        workspace.to_str().unwrap(),
        "current",
    );
    storage.connection().execute_batch("CREATE TRIGGER reject_auto_blocker BEFORE INSERT ON events WHEN NEW.kind='needs_user' BEGIN SELECT RAISE(ABORT,'secret-trigger'); END").unwrap();
    let pending = serde_json::json!([
        permission_json("read", "current", "read", &[]),
        permission_json("git", "current", "bash", &["git push"])
    ]);
    let (port, server) = spawn_server(move |request, _| {
        if request.method == "GET" {
            ok_json(&pending)
        } else {
            ok_json(&serde_json::json!({}))
        }
    });
    let client = build_client(port, &workspace);
    let config = approval_project(
        &dir,
        &workspace,
        port,
        layout.state_root(),
        "auto_approve_permissions=[\"read\",\"bash\"]",
    );
    let mut replies = PermissionReplies::default();
    let error = handle_permission_blocker_with_auto_approval(
        &client,
        &layout,
        round_ref(task, &project_id, 1),
        &config,
        &mut replies,
    )
    .unwrap_err();
    assert_eq!(error.kind(), PermissionBlockerErrorKind::Storage);
    assert!(!format!("{error} {error:?}").contains("secret-trigger"));
    assert_eq!(round_status(&storage, task, 1), "observing");
    assert_eq!(replies.approvals().len(), 1);
    storage
        .connection()
        .execute_batch("DROP TRIGGER reject_auto_blocker")
        .unwrap();
    assert!(
        handle_permission_blocker_with_auto_approval(
            &client,
            &layout,
            round_ref(task, &project_id, 1),
            &config,
            &mut replies
        )
        .unwrap()
        .is_blocked()
    );
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
    assert_eq!(event_count(&storage, task, "needs_user"), 1);
}

#[test]
fn reply_buffer_cannot_cross_sessions_or_approve_a_changed_session_after_get() {
    for during_get in [false, true] {
        let dir = TempDir::new("auto-session");
        let workspace = dir.mkdir("ws");
        let (layout, mut storage) = open_storage(&dir, "proj");
        let task = task_id();
        let project_id = project("proj");
        seed_observing(
            &mut storage,
            task,
            &project_id,
            workspace.to_str().unwrap(),
            "current",
        );
        let database = layout.database();
        let pending = serde_json::json!([permission_json("read", "current", "read", &[])]);
        let (port, server) = spawn_server(move |request, _| {
            if during_get && request.method == "GET" {
                connect(&database)
                    .unwrap()
                    .connection()
                    .execute("UPDATE rounds SET session_id='new-session'", [])
                    .unwrap();
            }
            if request.method == "GET" {
                ok_json(&pending)
            } else {
                ok_json(&serde_json::json!({}))
            }
        });
        let client = build_client(port, &workspace);
        let config = approval_project(
            &dir,
            &workspace,
            port,
            layout.state_root(),
            "auto_approve_permissions=[\"read\"]",
        );
        let mut replies = PermissionReplies::default();
        let first = handle_permission_blocker_with_auto_approval(
            &client,
            &layout,
            round_ref(task, &project_id, 1),
            &config,
            &mut replies,
        );
        if during_get {
            assert_eq!(
                first.unwrap_err().kind(),
                PermissionBlockerErrorKind::StaleRound
            );
            assert_eq!(server.request_count(), 1);
        } else {
            assert!(!first.unwrap().is_blocked());
            storage
                .connection()
                .execute("UPDATE rounds SET session_id='new-session'", [])
                .unwrap();
            let count = server.request_count();
            assert_eq!(
                handle_permission_blocker_with_auto_approval(
                    &client,
                    &layout,
                    round_ref(task, &project_id, 1),
                    &config,
                    &mut replies
                )
                .unwrap_err()
                .kind(),
                PermissionBlockerErrorKind::InvalidInput
            );
            assert_eq!(server.request_count(), count);
        }
    }
}

#[test]
fn reply_buffer_rejects_another_namespace_or_endpoint_before_http() {
    let dir = TempDir::new("auto-namespace");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj");
    let task = task_id();
    let project_id = project("proj");
    seed_observing(
        &mut storage,
        task,
        &project_id,
        workspace.to_str().unwrap(),
        "current",
    );
    let pending = serde_json::json!([permission_json("read", "current", "read", &[])]);
    let (port, server) = spawn_server(move |request, _| {
        if request.method == "GET" {
            ok_json(&pending)
        } else {
            ok_json(&serde_json::json!({}))
        }
    });
    let client = build_client(port, &workspace);
    let config = approval_project(
        &dir,
        &workspace,
        port,
        layout.state_root(),
        "auto_approve_permissions=[\"read\"]",
    );
    let mut replies = PermissionReplies::default();
    assert!(
        !handle_permission_blocker_with_auto_approval(
            &client,
            &layout,
            round_ref(task, &project_id, 1),
            &config,
            &mut replies
        )
        .unwrap()
        .is_blocked()
    );
    let other =
        bridge_storage::RustStateLayout::new(dir.path().join("other-state"), project_id.clone())
            .unwrap();
    other.initialize().unwrap();
    let mut other_storage = other.open().unwrap();
    seed_observing(
        &mut other_storage,
        task,
        &project_id,
        workspace.to_str().unwrap(),
        "current",
    );
    let count = server.request_count();
    assert_eq!(
        handle_permission_blocker_with_auto_approval(
            &client,
            &other,
            round_ref(task, &project_id, 1),
            &config,
            &mut replies
        )
        .unwrap_err()
        .kind(),
        PermissionBlockerErrorKind::InvalidInput
    );
    let changed = approval_project(
        &dir,
        &workspace,
        if port == 65535 { port - 1 } else { port + 1 },
        layout.state_root(),
        "auto_approve_permissions=[\"read\"]",
    );
    assert_eq!(
        handle_permission_blocker_with_auto_approval(
            &client,
            &layout,
            round_ref(task, &project_id, 1),
            &changed,
            &mut replies
        )
        .unwrap_err()
        .kind(),
        PermissionBlockerErrorKind::InvalidInput
    );
    assert_eq!(server.request_count(), count);
}
