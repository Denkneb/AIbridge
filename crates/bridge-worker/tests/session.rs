//! Integration tests for round session resolution (task 7.3).
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
use bridge_storage::{
    CreateRevisionRoundInput, CreateTaskInput, FinishRoundInput, RoundRef, StorageConnection,
    connect,
};
use bridge_worker::{
    SessionResolutionErrorKind, SessionResolutionSource, resolve_round_session, round_session_title,
};
use rusqlite::OptionalExtension;

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
            "bridge-worker-session-{tag}-{}-{nanos}-{sequence}",
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
    body: Vec<u8>,
    authorization: Option<String>,
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
    sessions: Vec<serde_json::Value>,
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

    fn sessions(&self) -> Vec<serde_json::Value> {
        self.state.lock().expect("state").sessions.clone()
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
    let body = data[head_end + 4..].to_vec();
    Some(RecordedRequest {
        method,
        target,
        body,
        authorization,
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
    ok_json_bytes(&serde_json::to_vec(value).expect("serialize"))
}

fn ok_json_bytes(body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
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

fn session_json(
    id: Option<&str>,
    title: Option<&str>,
    directory: Option<&str>,
) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    if let Some(id) = id {
        object.insert("id".to_owned(), serde_json::json!(id));
    }
    if let Some(title) = title {
        object.insert("title".to_owned(), serde_json::json!(title));
    }
    if let Some(directory) = directory {
        object.insert("directory".to_owned(), serde_json::json!(directory));
    }
    serde_json::Value::Object(object)
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

fn round_session(storage: &StorageConnection, task: TaskId, number: u32) -> Option<String> {
    storage
        .connection()
        .query_row(
            "SELECT session_id FROM rounds WHERE task_id = ?1 AND round_number = ?2",
            rusqlite::params![task.to_string(), i64::from(number)],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .expect("round query")
        .flatten()
}

fn task_session(storage: &StorageConnection, task: TaskId) -> Option<String> {
    storage
        .get_task(task)
        .expect("get task")
        .and_then(|task| task.session_id)
}

/// Builds a client bound to the canonical `workspace` and the mock `port`.
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

/// Decodes a `application/x-www-form-urlencoded` value enough for assertions.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn persisted_round_session_wins_without_http_or_write() {
    let dir = TempDir::new("existing");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    storage
        .bind_round_session(round_ref(task, &project, 1), "ses-existing".to_owned())
        .expect("seed bind");
    let before = storage.get_task(task).expect("task").expect("row");

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let resolved = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect("persisted session must win");
    assert_eq!(resolved.id(), "ses-existing");
    assert_eq!(resolved.source(), SessionResolutionSource::Existing);
    assert_eq!(server.request_count(), 0, "no HTTP request is allowed");
    let after = storage.get_task(task).expect("task").expect("row");
    assert_eq!(before.updated_at, after.updated_at, "no write is allowed");
}

#[test]
fn exact_title_adoption_binds_without_post() {
    let dir = TempDir::new("adopt");
    let workspace = dir.mkdir("ws");
    let workspace_canonical = std::fs::canonicalize(&workspace).expect("canonical");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);

    let (port, server) = spawn_server(move |request, _state| {
        if request.method == "GET" && request.path() == "/session" {
            ok_json(&serde_json::json!([session_json(
                Some("ses_adopt"),
                Some(&title),
                Some(&workspace_canonical.to_string_lossy()),
            )]))
        } else {
            status(500, b"unexpected")
        }
    });
    let client = build_client(port, &workspace);

    let resolved = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect("adoption must succeed");
    assert_eq!(resolved.id(), "ses_adopt");
    assert_eq!(resolved.source(), SessionResolutionSource::Adopted);

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "exactly one list request");
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path(), "/session");
    assert_scoped_auth(&requests[0], &workspace);

    assert_eq!(
        round_session(&storage, task, 1).as_deref(),
        Some("ses_adopt")
    );
    assert_eq!(task_session(&storage, task).as_deref(), Some("ses_adopt"));
}

#[test]
fn multiple_title_matches_are_ambiguous_without_binding() {
    let dir = TempDir::new("ambiguous");
    let workspace = dir.mkdir("ws");
    let workspace_canonical = std::fs::canonicalize(&workspace).expect("canonical");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);

    let (port, server) = spawn_server(move |request, _state| {
        if request.method == "GET" && request.path() == "/session" {
            ok_json(&serde_json::json!([
                session_json(
                    Some("ses-one"),
                    Some(&title),
                    Some(&workspace_canonical.to_string_lossy())
                ),
                session_json(Some("ses-two"), Some(&title), None),
            ]))
        } else {
            status(500, b"unexpected")
        }
    });
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect_err("two matches must be ambiguous");
    assert_eq!(error.kind(), SessionResolutionErrorKind::SessionAmbiguous);
    assert_eq!(server.request_count(), 1, "no POST is allowed");
    assert_eq!(round_session(&storage, task, 1), None);
    assert_eq!(task_session(&storage, task), None);
}

#[test]
fn unusable_matched_session_id_is_unknown() {
    let dir = TempDir::new("bad-id");
    let workspace = dir.mkdir("ws");
    let workspace_canonical = std::fs::canonicalize(&workspace).expect("canonical");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);
    let directory = workspace_canonical.to_string_lossy().into_owned();

    let cases = [
        session_json(None, Some(&title), Some(&directory)),
        session_json(Some(""), Some(&title), Some(&directory)),
        session_json(Some("other-id"), Some(&title), Some(&directory)),
        serde_json::json!({"id": 123, "title": title, "directory": directory}),
    ];

    for case in cases {
        let case = case.clone();
        let (port, server) = spawn_server(move |request, _state| {
            if request.method == "GET" && request.path() == "/session" {
                ok_json(&serde_json::json!([case.clone()]))
            } else {
                status(500, b"unexpected")
            }
        });
        let client = build_client(port, &workspace);

        let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
            .expect_err("an unusable matched id must be unknown");
        assert_eq!(error.kind(), SessionResolutionErrorKind::SessionUnknown);
        assert_eq!(server.request_count(), 1, "exactly one GET and no POST");
        assert_eq!(round_session(&storage, task, 1), None);
        assert_eq!(task_session(&storage, task), None);
    }
}

#[test]
fn adoption_requires_matching_directory() {
    for directory in [None, Some("/definitely/not/the/workspace")] {
        let dir = TempDir::new("adopt-dir");
        let workspace = dir.mkdir("ws");
        let (layout, mut storage) = open_storage(&dir, "proj-1");
        let project = project("proj-1");
        let task = task_id();
        create_task(
            &mut storage,
            task,
            &project,
            workspace.to_str().expect("utf-8"),
        );
        let title = round_session_title(task, 1);
        let directory = directory.map(str::to_owned);

        let (port, server) = spawn_server(move |request, _state| {
            if request.method == "GET" && request.path() == "/session" {
                ok_json(&serde_json::json!([session_json(
                    Some("ses_adopt"),
                    Some(&title),
                    directory.as_deref(),
                )]))
            } else {
                status(500, b"unexpected")
            }
        });
        let client = build_client(port, &workspace);

        let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
            .expect_err("adoption without the workspace directory must fail");
        assert_eq!(
            error.kind(),
            SessionResolutionErrorKind::SessionDirectoryMismatch
        );
        assert_eq!(server.request_count(), 1, "no POST is allowed");
        assert_eq!(task_session(&storage, task), None);
    }
}

#[test]
fn create_posts_once_and_binds_without_parent_id() {
    let dir = TempDir::new("create");
    let workspace = dir.mkdir("ws");
    let workspace_canonical = std::fs::canonicalize(&workspace).expect("canonical");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);
    let expected_title = title.clone();

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_created"),
                    Some(&title),
                    Some(&workspace_canonical.to_string_lossy()),
                )),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let resolved = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect("create must succeed");
    assert_eq!(resolved.id(), "ses_created");
    assert_eq!(resolved.source(), SessionResolutionSource::Created);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "POST");
    assert_scoped_auth(&requests[0], &workspace);
    assert_scoped_auth(&requests[1], &workspace);
    let body = String::from_utf8(requests[1].body.clone()).expect("utf-8 body");
    assert_eq!(body, format!(r#"{{"title":"{expected_title}"}}"#));
    assert!(!body.contains("parentID"), "no parent session may be set");
    assert!(!body.contains("fork"), "no fork may be requested");

    assert_eq!(
        round_session(&storage, task, 1).as_deref(),
        Some("ses_created")
    );
    assert_eq!(task_session(&storage, task).as_deref(), Some("ses_created"));
}

#[test]
fn create_with_missing_directory_is_allowed() {
    let dir = TempDir::new("create-no-dir");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);

    let (port, _server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => {
                    ok_json(&session_json(Some("ses_created"), Some(&title), None))
                }
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let resolved = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect("missing directory is allowed for create");
    assert_eq!(resolved.id(), "ses_created");
    assert_eq!(resolved.source(), SessionResolutionSource::Created);
    assert_eq!(task_session(&storage, task).as_deref(), Some("ses_created"));
}

#[test]
fn create_with_mismatched_directory_fails_without_binding() {
    let dir = TempDir::new("create-bad-dir");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_created"),
                    Some(&title),
                    Some("/definitely/not/the/workspace"),
                )),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a mismatched create directory must fail");
    assert_eq!(
        error.kind(),
        SessionResolutionErrorKind::SessionDirectoryMismatch
    );
    assert_eq!(server.request_count(), 2, "exactly one GET and one POST");
    assert_eq!(task_session(&storage, task), None, "no binding is allowed");
}

#[cfg(unix)]
#[test]
fn symlink_workspace_alias_is_accepted() {
    let dir = TempDir::new("symlink");
    let workspace = dir.mkdir("ws");
    let workspace_canonical = std::fs::canonicalize(&workspace).expect("canonical");
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&workspace_canonical, &alias).expect("symlink");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);
    let alias_string = alias.to_string_lossy().into_owned();

    let (port, _server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                    Some("ses_alias"),
                    Some(&title),
                    Some(&alias_string),
                )])),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let resolved = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect("a symlink alias of the workspace must match");
    assert_eq!(resolved.id(), "ses_alias");
    assert_eq!(resolved.source(), SessionResolutionSource::Adopted);
}

#[test]
fn list_transport_and_malformed_fail_closed_without_create() {
    for response in [
        status(500, b"do-not-leak-body!"),
        ok_json_bytes(b"not-json"),
        ok_json(&serde_json::json!({"not": "an array"})),
    ] {
        let dir = TempDir::new("list-fail");
        let workspace = dir.mkdir("ws");
        let (layout, mut storage) = open_storage(&dir, "proj-1");
        let project = project("proj-1");
        let task = task_id();
        create_task(
            &mut storage,
            task,
            &project,
            workspace.to_str().expect("utf-8"),
        );
        let response = response.clone();

        let (port, server) = spawn_server(move |_request, _state| response.clone());
        let client = build_client(port, &workspace);

        let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
            .expect_err("a failed list must fail closed");
        assert_eq!(error.kind(), SessionResolutionErrorKind::SessionUnknown);
        assert_eq!(server.request_count(), 1, "no POST after a failed list");
        assert_eq!(task_session(&storage, task), None);
    }
}

#[test]
fn create_transport_failure_fails_closed_without_retry() {
    let dir = TempDir::new("create-fail");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );

    let (port, server) = spawn_server(|request, _state| {
        if request.method == "GET" {
            ok_json(&serde_json::json!([]))
        } else {
            status(500, b"do-not-leak-body!")
        }
    });
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a failed create must fail closed");
    assert_eq!(error.kind(), SessionResolutionErrorKind::SessionUnknown);
    assert_eq!(server.request_count(), 2, "exactly one GET and one POST");
    assert_eq!(task_session(&storage, task), None);
}

#[test]
fn create_invalid_session_response_is_unknown_without_retry() {
    let dir = TempDir::new("create-bad-body");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);

    let cases = [
        status(500, b"do-not-leak-body!"),
        ok_json(&session_json(Some("other-id"), Some(&title), None)),
        ok_json(&session_json(None, Some(&title), None)),
        ok_json(&session_json(Some(""), Some(&title), None)),
        ok_json(&serde_json::json!({"id": 123, "title": title})),
        ok_json(&serde_json::json!({})),
        ok_json_bytes(b"not-json"),
        ok_json(&serde_json::json!([session_json(
            Some("ses_list"),
            Some(&title),
            None
        )])),
        ok_json(&serde_json::json!("scalar")),
    ];

    for case in cases {
        let case = case.clone();
        let (port, server) =
            spawn_server(
                move |request, _state| match (request.method.as_str(), request.path()) {
                    ("GET", "/session") => ok_json(&serde_json::json!([])),
                    ("POST", "/session") => case.clone(),
                    _ => status(500, b"unexpected"),
                },
            );
        let client = build_client(port, &workspace);

        let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
            .expect_err("an invalid created session must be unknown");
        assert_eq!(error.kind(), SessionResolutionErrorKind::SessionUnknown);
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "exactly one GET and one POST");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "POST")
                .count(),
            1,
            "no automatic retry is allowed"
        );
        assert_eq!(round_session(&storage, task, 1), None);
        assert_eq!(task_session(&storage, task), None);
    }
}

#[test]
fn create_with_non_string_directory_is_allowed() {
    let dir = TempDir::new("create-non-string-dir");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);

    let (port, _server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&serde_json::json!({
                    "id": "ses_created",
                    "title": title,
                    "directory": 123,
                })),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let resolved = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect("a non-string directory folds to absent and is allowed for create");
    assert_eq!(resolved.id(), "ses_created");
    assert_eq!(resolved.source(), SessionResolutionSource::Created);
    assert_eq!(task_session(&storage, task).as_deref(), Some("ses_created"));
}

#[test]
fn second_resolution_reuses_session_without_duplicate() {
    let dir = TempDir::new("reuse");
    let workspace = dir.mkdir("ws");
    let workspace_canonical = std::fs::canonicalize(&workspace).expect("canonical");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_created"),
                    Some(&title),
                    Some(&workspace_canonical.to_string_lossy()),
                )),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let first = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect("first create");
    assert_eq!(first.source(), SessionResolutionSource::Created);
    assert_eq!(server.request_count(), 2);

    let second = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect("second reuse");
    assert_eq!(second.id(), "ses_created");
    assert_eq!(second.source(), SessionResolutionSource::Existing);
    assert_eq!(server.request_count(), 2, "no duplicate HTTP request");
}

#[test]
fn orphan_created_but_not_bound_is_adopted_on_retry() {
    let dir = TempDir::new("orphan");
    let workspace = dir.mkdir("ws");
    let workspace_canonical = std::fs::canonicalize(&workspace).expect("canonical");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);
    let expected_title = title.clone();

    let (port, server) = spawn_server(move |request, state| {
        match (request.method.as_str(), request.path()) {
            ("GET", "/session") => ok_json(&serde_json::Value::Array(state.sessions.clone())),
            ("POST", "/session") => {
                // The server creates the session but the client never sees a
                // success, so the binding is skipped and an orphan remains.
                state.sessions.push(session_json(
                    Some("ses_orphan"),
                    Some(&expected_title),
                    Some(&workspace_canonical.to_string_lossy()),
                ));
                status(500, b"do-not-leak-body!")
            }
            _ => status(500, b"unexpected"),
        }
    });
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect_err("the first call must fail");
    assert_eq!(error.kind(), SessionResolutionErrorKind::SessionUnknown);
    assert_eq!(server.sessions().len(), 1, "the orphan must exist");
    assert_eq!(task_session(&storage, task), None);

    let resolved = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect("the retry must adopt the orphan");
    assert_eq!(resolved.id(), "ses_orphan");
    assert_eq!(resolved.source(), SessionResolutionSource::Adopted);
    let posts = server
        .requests()
        .into_iter()
        .filter(|request| request.method == "POST")
        .count();
    assert_eq!(posts, 1, "no duplicate create is allowed");
    assert_eq!(
        round_session(&storage, task, 1).as_deref(),
        Some("ses_orphan")
    );
}

#[test]
fn storage_rejection_is_typed_and_never_a_success() {
    let dir = TempDir::new("storage-fail");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    // Drive round one to awaiting_review so a revision round can be created
    // concurrently while the resolver is between create and bind.
    storage
        .mark_round_observing(round_ref(task, &project, 1))
        .expect("observing");
    storage
        .finish_round(FinishRoundInput {
            round: round_ref(task, &project, 1),
            round_status: RoundStatus::Complete,
            task_status: TaskStatus::AwaitingReview,
            response_message_id: None,
            response: None,
            error_code: None,
            result_json: None,
        })
        .expect("finish round one");

    let mutator = Arc::new(Mutex::new(
        connect(layout.database()).expect("second connection"),
    ));
    let mutator_for_handler = Arc::clone(&mutator);
    let title = round_session_title(task, 1);
    let handler_project = project.clone();

    let (port, server) = spawn_server(move |request, _state| {
        match (request.method.as_str(), request.path()) {
            ("GET", "/session") => ok_json(&serde_json::json!([])),
            ("POST", "/session") => {
                // Make round one stale before the resolver binds it.
                let mut guard = mutator_for_handler.lock().expect("mutator");
                guard
                    .create_revision_round(CreateRevisionRoundInput {
                        task_id: task,
                        project_id: handler_project.clone(),
                        round_number: 2,
                        request_id: "concurrent-revision".to_owned(),
                        payload_hash: "hash".to_owned(),
                        findings: None,
                    })
                    .expect("concurrent revision");
                ok_json(&session_json(Some("ses_created"), Some(&title), None))
            }
            _ => status(500, b"unexpected"),
        }
    });
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a failed binding must not be a success");
    assert_eq!(error.kind(), SessionResolutionErrorKind::Storage);
    assert_eq!(server.request_count(), 2);
    assert_eq!(round_session(&storage, task, 1), None, "no round binding");
    assert_eq!(task_session(&storage, task), None, "no task binding");
}

#[test]
fn task_workspace_mismatch_fails_before_http() {
    let dir = TempDir::new("ws-mismatch");
    let workspace = dir.mkdir("ws");
    let other = dir.mkdir("other");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(&mut storage, task, &project, other.to_str().expect("utf-8"));

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a task workspace mismatch must fail");
    assert_eq!(error.kind(), SessionResolutionErrorKind::WorkspaceMismatch);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn foreign_project_round_and_missing_task_fail_before_http() {
    let dir = TempDir::new("foreign");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let primary_project = project("proj-1");
    let other_project = project("proj-2");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &primary_project,
        workspace.to_str().expect("utf-8"),
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let mismatch = resolve_round_session(&client, &layout, round_ref(task, &other_project, 1))
        .expect_err("a foreign project must fail");
    assert_eq!(mismatch.kind(), SessionResolutionErrorKind::TaskMismatch);

    let missing = TaskId::from_str("11111111-1111-1111-1111-111111111111").expect("task id");
    let unknown = resolve_round_session(&client, &layout, round_ref(missing, &primary_project, 1))
        .expect_err("a missing task must fail");
    assert_eq!(unknown.kind(), SessionResolutionErrorKind::UnknownTask);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn stale_round_fails_before_http() {
    let dir = TempDir::new("stale");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    storage
        .mark_round_observing(round_ref(task, &project, 1))
        .expect("observing");
    storage
        .finish_round(FinishRoundInput {
            round: round_ref(task, &project, 1),
            round_status: RoundStatus::Complete,
            task_status: TaskStatus::AwaitingReview,
            response_message_id: None,
            response: None,
            error_code: None,
            result_json: None,
        })
        .expect("finish");
    storage
        .create_revision_round(CreateRevisionRoundInput {
            task_id: task,
            project_id: project.clone(),
            round_number: 2,
            request_id: "rev-1".to_owned(),
            payload_hash: "hash".to_owned(),
            findings: None,
        })
        .expect("revision");

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a stale round must fail");
    assert_eq!(error.kind(), SessionResolutionErrorKind::StaleRound);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn zero_round_is_invalid_input() {
    let dir = TempDir::new("zero");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &project, 0))
        .expect_err("round zero must fail");
    assert_eq!(error.kind(), SessionResolutionErrorKind::InvalidInput);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn task_session_id_is_never_used_as_a_fallback() {
    let dir = TempDir::new("fallback");
    let workspace = dir.mkdir("ws");
    let workspace_canonical = std::fs::canonicalize(&workspace).expect("canonical");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    // Simulate an inconsistent persisted state that production never writes:
    // the task has a session while the current round has none. A raw fixture
    // write is used only to build this state; the resolver must ignore it.
    storage
        .connection()
        .execute(
            "UPDATE tasks SET session_id = 'ses-task-pointer' WHERE task_id = ?1",
            rusqlite::params![task.to_string()],
        )
        .expect("fixture write");
    let title = round_session_title(task, 1);

    let (port, _server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_fresh"),
                    Some(&title),
                    Some(&workspace_canonical.to_string_lossy()),
                )),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let resolved = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect("the fresh session must be created");
    assert_eq!(resolved.id(), "ses_fresh");
    assert_ne!(resolved.id(), "ses-task-pointer");
    assert_eq!(
        round_session(&storage, task, 1).as_deref(),
        Some("ses_fresh")
    );
}

#[test]
fn error_and_outcome_rendering_are_redacted() {
    let dir = TempDir::new("redaction");
    let workspace = dir.mkdir("secret-workspace");
    let workspace_canonical = std::fs::canonicalize(&workspace).expect("canonical");
    let (layout, mut storage) = open_storage(&dir, "secret-project");
    let project = project("secret-project");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &project,
        workspace.to_str().expect("utf-8"),
    );
    let title = round_session_title(task, 1);

    let (port, _server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                    Some("ses_secret"),
                    Some(&title),
                    Some(&workspace_canonical.to_string_lossy()),
                )])),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let resolved =
        resolve_round_session(&client, &layout, round_ref(task, &project, 1)).expect("adoption");
    let rendered = format!("{resolved} {resolved:?}");
    assert!(!rendered.contains("ses_secret"));
    assert!(!rendered.contains("secret-workspace"));
    assert!(!rendered.contains(TASK_UUID));

    let error = resolve_round_session(
        &client,
        &layout,
        round_ref(
            TaskId::from_str("22222222-2222-2222-2222-222222222222").expect("task id"),
            &project,
            1,
        ),
    )
    .expect_err("missing task");
    let rendered_error = format!("{error} {error:?}");
    assert!(!rendered_error.contains("secret-project"));
    assert!(!rendered_error.contains("22222222"));
    assert!(!rendered_error.contains("ses_secret"));
}

// ---------------------------------------------------------------------------
// Ownership guard: only a verified Rust-owned state may be resolved or bound
// ---------------------------------------------------------------------------

/// Seeds a task (and its first round) into an arbitrary generic database.
fn seed_generic_task(database: &Path, task: TaskId, project: &ProjectId, workspace: &Path) {
    let mut storage = connect(database).expect("generic connection");
    create_task(
        &mut storage,
        task,
        project,
        workspace.to_str().expect("utf-8"),
    );
}

#[test]
fn unmarked_schema_v6_state_is_rejected_before_http() {
    let dir = TempDir::new("unmarked");
    let workspace = dir.mkdir("ws");
    let project = project("proj-1");
    let task = task_id();
    let layout = rust_layout(&dir, "proj-1");
    // A schema-v6 database without the Rust sidecar marker (a Python-owned or
    // legacy database) must never be adopted.
    bridge_storage::initialize(layout.database()).expect("schema v6 database");
    seed_generic_task(&layout.database(), task, &project, &workspace);
    let database_before = std::fs::read(layout.database()).expect("db bytes");
    assert!(
        !layout.marker().exists(),
        "the state is deliberately unmarked"
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect_err("an unmarked state must be rejected");
    assert_eq!(error.kind(), SessionResolutionErrorKind::StateOwnership);
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
    let foreign = connect(layout.database()).expect("foreign connection");
    assert_eq!(round_session(&foreign, task, 1), None);
    assert_eq!(task_session(&foreign, task), None);
}

#[test]
fn foreign_marker_is_rejected_before_http() {
    let dir = TempDir::new("foreign-marker");
    let workspace = dir.mkdir("ws");
    let project = project("proj-1");
    let task = task_id();
    let layout = rust_layout(&dir, "proj-1");
    std::fs::create_dir_all(layout.project_dir()).expect("project dir");
    bridge_storage::initialize(layout.database()).expect("schema v6 database");
    seed_generic_task(&layout.database(), task, &project, &workspace);
    std::fs::write(
        layout.marker(),
        r#"{"implementation":"python","format_version":1,"project_id":"proj-1","state_root":"00"}"#,
    )
    .expect("foreign marker");
    let marker_before = std::fs::read(layout.marker()).expect("marker bytes");
    let database_before = std::fs::read(layout.database()).expect("db bytes");

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a foreign marker must be rejected");
    assert_eq!(error.kind(), SessionResolutionErrorKind::StateOwnership);
    assert_eq!(
        server.request_count(),
        0,
        "no HTTP before the ownership guard"
    );
    assert_eq!(
        std::fs::read(layout.marker()).expect("marker bytes"),
        marker_before,
        "the foreign marker must be unchanged"
    );
    assert_eq!(
        std::fs::read(layout.database()).expect("db bytes"),
        database_before,
        "the foreign database must be unchanged"
    );
    let foreign = connect(layout.database()).expect("foreign connection");
    assert_eq!(round_session(&foreign, task, 1), None);
    assert_eq!(task_session(&foreign, task), None);
}

#[test]
fn layout_project_mismatch_with_round_is_rejected_before_http() {
    let dir = TempDir::new("layout-mismatch");
    let workspace = dir.mkdir("ws");
    let primary = project("proj-1");
    let foreign = project("proj-2");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let task = task_id();
    create_task(
        &mut storage,
        task,
        &primary,
        workspace.to_str().expect("utf-8"),
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &layout, round_ref(task, &foreign, 1))
        .expect_err("a layout/round project mismatch must be rejected");
    assert_eq!(error.kind(), SessionResolutionErrorKind::TaskMismatch);
    assert_eq!(
        server.request_count(),
        0,
        "no HTTP before the ownership guard"
    );
    assert_eq!(round_session(&storage, task, 1), None);
    assert_eq!(task_session(&storage, task), None);
}

#[test]
fn rust_state_copied_to_another_root_is_rejected_before_http() {
    let dir = TempDir::new("copied");
    let workspace = dir.mkdir("ws");
    let project = project("proj-1");
    let task = task_id();
    let source = rust_layout(&dir, "proj-1");
    source.initialize().expect("initialize source state");
    seed_generic_task(&source.database(), task, &project, &workspace);

    let other_root = dir.mkdir("other-state");
    let other = bridge_storage::RustStateLayout::new(other_root.clone(), project.clone())
        .expect("other layout");
    std::fs::create_dir_all(other.project_dir()).expect("other project dir");
    std::fs::copy(source.marker(), other.marker()).expect("copy marker");
    std::fs::copy(source.database(), other.database()).expect("copy database");
    let marker_before = std::fs::read(other.marker()).expect("marker bytes");
    let database_before = std::fs::read(other.database()).expect("db bytes");

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = resolve_round_session(&client, &other, round_ref(task, &project, 1))
        .expect_err("state copied under another root must be rejected");
    assert_eq!(error.kind(), SessionResolutionErrorKind::StateOwnership);
    assert_eq!(
        server.request_count(),
        0,
        "no HTTP before the ownership guard"
    );
    assert_eq!(
        std::fs::read(other.marker()).expect("marker bytes"),
        marker_before,
        "the copied marker must be unchanged"
    );
    assert_eq!(
        std::fs::read(other.database()).expect("db bytes"),
        database_before,
        "the copied database must be unchanged"
    );
    let foreign = connect(other.database()).expect("foreign connection");
    assert_eq!(round_session(&foreign, task, 1), None);
    assert_eq!(task_session(&foreign, task), None);
}
