//! Integration tests for the initial prompt happy path (task 7.4).
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
use bridge_domain::{ProjectId, RoundKind, RoundStatus, TaskId, TaskStatus};
use bridge_opencode::OpenCodeClient;
use bridge_storage::{
    CreateRevisionRoundInput, CreateTaskInput, FinishRoundInput, RoundRef, StorageConnection,
    connect,
};
use bridge_worker::{
    DispatchErrorKind, SessionResolutionSource, dispatch_initial_round, dispatch_revision_round,
    initial_prompt, new_message_id, revision_prompt, round_session_title,
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
            "bridge-worker-dispatch-{tag}-{}-{nanos}-{sequence}",
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

    fn body_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("json body")
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

    fn prompt_posts(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.path().ends_with("/prompt_async"))
            .collect()
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

fn no_content() -> Vec<u8> {
    b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n".to_vec()
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

#[allow(clippy::too_many_arguments)]
fn create_task_full(
    storage: &mut StorageConnection,
    task: TaskId,
    project: &ProjectId,
    workspace: &str,
    text: &str,
    allowed_paths: Vec<String>,
    test_commands: Vec<String>,
    snapshot: Option<serde_json::Value>,
) {
    storage
        .create_task(CreateTaskInput {
            task_id: task,
            project_id: project.clone(),
            workspace: workspace.to_owned(),
            task: text.to_owned(),
            request_id: format!("req-{task}"),
            payload_hash: "hash".to_owned(),
            base_head: None,
            allowed_paths,
            test_commands,
            snapshot,
        })
        .expect("create task");
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoundState {
    status: String,
    attempted: i64,
    outbound: Option<String>,
    session: Option<String>,
}

fn round_state(storage: &StorageConnection, task: TaskId, number: u32) -> RoundState {
    storage
        .connection()
        .query_row(
            "SELECT status, attempted, outbound_message_id, session_id FROM rounds \
             WHERE task_id = ?1 AND round_number = ?2",
            rusqlite::params![task.to_string(), i64::from(number)],
            |row| {
                Ok(RoundState {
                    status: row.get(0)?,
                    attempted: row.get(1)?,
                    outbound: row.get(2)?,
                    session: row.get(3)?,
                })
            },
        )
        .expect("round state")
}

fn task_session(storage: &StorageConnection, task: TaskId) -> Option<String> {
    storage
        .get_task(task)
        .expect("get task")
        .and_then(|task| task.session_id)
}

/// Installs a temporary, test-only SQLite trigger on the fresh Rust-owned
/// database.
///
/// This is fault injection at the storage boundary: the production schema,
/// migration and `bridge-storage` code are untouched. The trigger makes exactly
/// one production lifecycle write abort so the dispatch-level failure handling
/// can be observed without adding any test-only branch to production code.
fn install_trigger(storage: &StorageConnection, sql: &str) {
    storage
        .connection()
        .execute_batch(sql)
        .expect("install test-only trigger");
}

fn build_client_with(port: u16, workspace: &Path, model: Option<&str>) -> OpenCodeClient {
    let dir = TempDir::new("client");
    let password = dir.path().join("proj.password");
    std::fs::write(&password, format!("{PASSWORD}\n")).expect("password file");
    set_private(&password);
    let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
    let model_line = model
        .map(|model| format!("opencode_model = \"{model}\"\n"))
        .unwrap_or_default();
    let config = format!(
        "[projects.proj]\nworkspace = \"{}\"\nopencode_url = \"http://127.0.0.1:{port}\"\npassword_file = \"{}\"\nmax_rounds = 3\n{model_line}",
        workspace.display(),
        password.display(),
    );
    let config_path = dir.path().join("projects.toml");
    std::fs::write(&config_path, config).expect("config file");
    let loaded = load_config(&config_path).expect("config must load");
    let entry = loaded.project("proj").expect("project must exist");
    OpenCodeClient::from_project(entry, Duration::from_secs(5)).expect("client")
}

fn build_client(port: u16, workspace: &Path) -> OpenCodeClient {
    build_client_with(port, workspace, None)
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

/// Independently reconstructs the exact reference `implementation_prompt` for
/// the focused cases below.
#[allow(clippy::too_many_arguments)]
fn expected_prompt(
    task_id: &str,
    workspace: &str,
    allowed: &str,
    baseline_rule: &str,
    git_rule: &str,
    test_commands: &str,
    text: &str,
) -> String {
    format!(
        concat!(
            "Работай только над задачей {task_id} в workspace {workspace}.\n",
            "Прочитай применимые AGENTS.md и проверь Git status.\n",
            "Меняй только согласованные allowed_paths: {allowed}.\n",
            "Относительные пути лежат в основном workspace; абсолютные — в доверенных\n",
            "внешних Git-репозиториях: у каждого затронутого репозитория свой HEAD, index\n",
            "и статус, проверь и отрази в отчёте git status/diff, index и HEAD каждого из них.\n",
            "{baseline_rule}\n",
            "{git_rule}\n",
            "Запусти согласованные test_commands и сохрани реальные результаты: {test_commands}.\n",
            "Во время разработки ты можешь выполнять любые целевые проверки, но после твоего\n",
            "финального ответа мост авторитетно повторит ровно согласованные test_commands\n",
            "в указанном порядке; их результат не зависит от твоего отчёта.\n",
            "Не выполняй push, deploy или реальные внешние вызовы.\n",
            "Не меняй соседние проекты и конфигурацию моста.\n",
            "По завершении верни: изменения, список файлов, команды проверок,\n",
            "коды завершения, результаты, что не проверено, оставшиеся проблемы.\n",
            "После отчёта прекрати изменения и жди ревью.\n",
            "\n",
            "Задача:\n",
            "{text}\n",
        ),
        task_id = task_id,
        workspace = workspace,
        allowed = allowed,
        baseline_rule = baseline_rule,
        git_rule = git_rule,
        test_commands = test_commands,
        text = text,
    )
}

const DIRTY_BASELINE_RULE: &str = "До задачи уже существовали изменения: Cargo.lock. Считай их \
     согласованным baseline; не выполняй reset, clean, stash и не теряй исходное содержимое.";
const STRICT_GIT_RULE: &str = "Не выполняй git add, commit и любые изменения index или HEAD ни в \
     одном затронутом репозитории.";

fn focused_snapshot() -> serde_json::Value {
    serde_json::json!({
        "dirty_paths": ["Cargo.lock"],
        "allow_commit": false,
    })
}

/// Completes the initial `implement` round and creates the revision round 2.
///
/// The production lifecycle is used end-to-end (no raw row edits), so the test
/// proves the 7.5 dispatch consumes a round produced by the accepted 3.9a
/// `create_revision_round`.
fn seed_revision_round(
    storage: &mut StorageConnection,
    task: TaskId,
    project: &ProjectId,
    findings: Option<&str>,
) {
    storage
        .mark_round_observing(round_ref(task, project, 1))
        .expect("observing");
    storage
        .finish_round(FinishRoundInput {
            round: round_ref(task, project, 1),
            round_status: RoundStatus::Complete,
            task_status: TaskStatus::AwaitingReview,
            response_message_id: None,
            response: None,
            error_code: None,
            result_json: None,
        })
        .expect("finish round one");
    storage
        .create_revision_round(CreateRevisionRoundInput {
            task_id: task,
            project_id: project.clone(),
            round_number: 2,
            request_id: "rev-1".to_owned(),
            payload_hash: "hash".to_owned(),
            findings: findings.map(str::to_owned),
        })
        .expect("revision round");
}

/// Independently reconstructs the exact reference `revision_prompt` for the
/// focused cases below (a second implementation of the template, not the
/// production builder).
#[allow(clippy::too_many_arguments)]
fn expected_revision_prompt(
    task_id: &str,
    workspace: &str,
    round_number: u32,
    allowed: &str,
    baseline_rule: &str,
    git_rule: &str,
    test_commands: &str,
    text: &str,
    findings: &str,
) -> String {
    format!(
        concat!(
            "Доработай задачу {task_id} в workspace {workspace}, раунд {round_number}.\n",
            "Это новая независимая сессия: не полагайся на контекст предыдущих раундов,\n",
            "прочитай исходную задачу и замечания ниже и сначала проверь текущее состояние\n",
            "Git и файлов командой git status и просмотром файлов.\n",
            "Меняй только согласованные allowed_paths: {allowed}.\n",
            "Относительные пути лежат в основном workspace; абсолютные — в доверенных\n",
            "внешних Git-репозиториях: у каждого затронутого репозитория свой HEAD, index\n",
            "и статус, проверь и отрази в отчёте git status/diff, index и HEAD каждого из них.\n",
            "{baseline_rule}\n",
            "{git_rule}\n",
            "Исправь только перечисленные замечания ревью. Не откатывай и не переделывай уже\n",
            "корректные изменения, сделанные в предыдущих раундах.\n",
            "Снова запусти согласованные test_commands: {test_commands} и сохрани реальные результаты.\n",
            "Во время доработки ты можешь выполнять любые целевые проверки, но после твоего\n",
            "финального ответа мост авторитетно повторит ровно согласованные test_commands;\n",
            "их результат не зависит от твоего отчёта.\n",
            "Не выполняй push или deploy.\n",
            "Верни: что изменено, результаты проверок, что не проверено и какие ограничения остались.\n",
            "\n",
            "Исходная задача:\n",
            "{text}\n",
            "\n",
            "Замечания ревью:\n",
            "{findings}\n",
        ),
        task_id = task_id,
        workspace = workspace,
        round_number = round_number,
        allowed = allowed,
        baseline_rule = baseline_rule,
        git_rule = git_rule,
        test_commands = test_commands,
        text = text,
        findings = findings,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn initial_dispatch_sends_exact_prompt_and_records_observing() {
    let dir = TempDir::new("happy");
    let workspace = dir.mkdir("ws");
    let workspace_canonical = std::fs::canonicalize(&workspace).expect("canonical");
    let workspace_string = workspace_canonical.to_string_lossy().into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        vec!["Cargo.toml".to_owned(), "src".to_owned()],
        vec!["cargo test".to_owned(), "cargo clippy".to_owned()],
        Some(focused_snapshot()),
    );
    let title = round_session_title(task, 1);
    let directory = workspace_string.clone();

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_created"),
                    Some(&title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let outcome = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect("dispatch must succeed");
    assert_eq!(outcome.session_source(), SessionResolutionSource::Created);
    assert_eq!(outcome.session().id(), "ses_created");
    assert_eq!(outcome.round().status, RoundStatus::Observing);
    assert_eq!(outcome.task().status, TaskStatus::Implementing);

    let outbound = outcome.outbound_message_id();
    assert!(outbound.starts_with("msg_"));
    assert_eq!(outbound.len(), "msg_".len() + 32);
    assert!(
        outbound["msg_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "GET list + POST create + POST prompt");
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path(), "/session");
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].path(), "/session");
    assert_eq!(requests[2].method, "POST");
    assert_eq!(requests[2].path(), "/session/ses_created/prompt_async");
    for request in &requests {
        assert_scoped_auth(request, &workspace);
    }

    let body = requests[2].body_json();
    assert_eq!(body["messageID"], serde_json::json!(outbound));
    let expected_text = expected_prompt(
        TASK_UUID,
        &workspace_string,
        "Cargo.toml, src",
        DIRTY_BASELINE_RULE,
        STRICT_GIT_RULE,
        "cargo test; cargo clippy",
        "do work",
    );
    assert_eq!(
        body["parts"],
        serde_json::json!([{ "type": "text", "text": expected_text }])
    );
    assert_eq!(
        expected_text,
        initial_prompt(outcome.task(), client.workspace()),
        "the dispatched text is exactly the production prompt builder output"
    );

    let state = round_state(&storage, task, 1);
    assert_eq!(state.status, "observing");
    assert_eq!(state.attempted, 1);
    assert_eq!(state.outbound.as_deref(), Some(outbound));
    assert_eq!(state.session.as_deref(), Some("ses_created"));
    assert_eq!(task_session(&storage, task).as_deref(), Some("ses_created"));
}

#[test]
fn dispatch_reuses_persisted_session_without_create() {
    let dir = TempDir::new("existing-session");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );
    storage
        .bind_round_session(round_ref(task, &project, 1), "ses-existing".to_owned())
        .expect("seed bind");

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("POST", "/session/ses-existing/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let outcome = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect("dispatch must succeed");
    assert_eq!(outcome.session_source(), SessionResolutionSource::Existing);
    assert_eq!(server.request_count(), 1, "only the prompt POST is sent");
    assert_eq!(server.prompt_posts().len(), 1);
    assert_eq!(round_state(&storage, task, 1).status, "observing");
}

#[test]
fn outbound_id_and_attempt_are_persisted_before_prompt_post() {
    let dir = TempDir::new("persist-before-send");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );
    let title = round_session_title(task, 1);
    let directory = workspace_string.clone();
    let database = layout.database();
    let observed: Arc<Mutex<Option<RoundState>>> = Arc::new(Mutex::new(None));
    let observed_for_handler = Arc::clone(&observed);

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_created"),
                    Some(&title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => {
                    let snapshot = {
                        let connection = connect(&database).expect("observer connection");
                        round_state(&connection, task, 1)
                    };
                    *observed_for_handler.lock().expect("observed") = Some(snapshot);
                    no_content()
                }
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let outcome = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect("dispatch must succeed");
    let at_post = observed
        .lock()
        .expect("observed")
        .clone()
        .expect("the prompt POST was observed");
    assert_eq!(
        at_post.status, "sent",
        "the attempt is persisted before the POST"
    );
    assert_eq!(at_post.attempted, 1);
    assert_eq!(
        at_post.outbound.as_deref(),
        Some(outcome.outbound_message_id())
    );
    assert_eq!(at_post.session.as_deref(), Some("ses_created"));
    let body = server.prompt_posts()[0].body_json();
    assert_eq!(
        body["messageID"],
        serde_json::json!(outcome.outbound_message_id())
    );
}

#[test]
fn prepared_outbound_id_is_reused_without_second_prepare() {
    let dir = TempDir::new("reuse-outbound");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );
    storage
        .prepare_round(round_ref(task, &project, 1), "msg_prepared".to_owned())
        .expect("prepare");
    storage
        .bind_round_session(round_ref(task, &project, 1), "ses-existing".to_owned())
        .expect("seed bind");

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("POST", "/session/ses-existing/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let outcome = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect("a prepared but unsent round must dispatch");
    assert_eq!(outcome.outbound_message_id(), "msg_prepared");
    assert_eq!(
        server.prompt_posts()[0].body_json()["messageID"],
        serde_json::json!("msg_prepared")
    );
    assert_eq!(round_state(&storage, task, 1).status, "observing");
}

#[test]
fn repeated_dispatch_is_rejected_before_send() {
    let dir = TempDir::new("repeated");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );
    let title = round_session_title(task, 1);
    let directory = workspace_string.clone();

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_created"),
                    Some(&title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    dispatch_initial_round(&client, &layout, round_ref(task, &project, 1)).expect("first dispatch");
    assert_eq!(server.prompt_posts().len(), 1);

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a repeated dispatch must fail");
    assert_eq!(error.kind(), DispatchErrorKind::RoundNotDispatchable);
    assert_eq!(server.prompt_posts().len(), 1, "no second prompt POST");
}

#[test]
fn revision_round_is_rejected_before_http() {
    let dir = TempDir::new("revision");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
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
        .expect("finish round one");
    storage
        .create_revision_round(CreateRevisionRoundInput {
            task_id: task,
            project_id: project.clone(),
            round_number: 2,
            request_id: "rev-1".to_owned(),
            payload_hash: "hash".to_owned(),
            findings: Some("fix it".to_owned()),
        })
        .expect("revision round");
    let revision = storage
        .connection()
        .query_row(
            "SELECT kind FROM rounds WHERE task_id = ?1 AND round_number = 2",
            rusqlite::params![task.to_string()],
            |row| row.get::<_, String>(0),
        )
        .expect("revision kind");
    assert_eq!(revision, RoundKind::Revise.as_str());

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("a revision round must not be dispatched as initial");
    assert_eq!(error.kind(), DispatchErrorKind::RevisionNotSupported);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn stale_foreign_and_invalid_inputs_fail_before_http() {
    let dir = TempDir::new("inputs");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let primary = project("proj-1");
    let other = project("proj-2");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &primary,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let zero = dispatch_initial_round(&client, &layout, round_ref(task, &primary, 0))
        .expect_err("round zero");
    assert_eq!(zero.kind(), DispatchErrorKind::InvalidInput);

    let foreign = dispatch_initial_round(&client, &layout, round_ref(task, &other, 1))
        .expect_err("foreign project");
    assert_eq!(foreign.kind(), DispatchErrorKind::TaskMismatch);

    let missing = TaskId::from_str("11111111-1111-1111-1111-111111111111").expect("task id");
    let unknown = dispatch_initial_round(&client, &layout, round_ref(missing, &primary, 1))
        .expect_err("missing task");
    assert_eq!(unknown.kind(), DispatchErrorKind::UnknownTask);

    assert_eq!(server.request_count(), 0, "no HTTP for invalid inputs");
}

#[test]
fn stale_round_and_non_pending_round_fail_before_http() {
    let dir = TempDir::new("stale");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
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

    let stale = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a stale round must fail");
    assert_eq!(stale.kind(), DispatchErrorKind::StaleRound);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn non_pending_round_is_not_dispatchable_before_http() {
    let dir = TempDir::new("observing");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );
    storage
        .mark_round_observing(round_ref(task, &project, 1))
        .expect("observing");

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a non-pending round must fail");
    assert_eq!(error.kind(), DispatchErrorKind::RoundNotDispatchable);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn closed_task_with_pending_implement_round_fails_before_http() {
    let dir = TempDir::new("closed-task");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );
    storage
        .request_task_close(task, "stop")
        .expect("request close");
    storage
        .complete_requested_close(task)
        .expect("complete close");
    let persisted = storage.get_task(task).expect("get task").expect("task");
    assert_eq!(persisted.status, TaskStatus::Closed);
    assert!(
        persisted.close_requested_at.is_some(),
        "the close request stays persisted"
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a closed task must not dispatch");
    assert_eq!(error.kind(), DispatchErrorKind::TaskNotDispatchable);
    assert_eq!(server.request_count(), 0, "no HTTP for a closed task");

    let state = round_state(&storage, task, 1);
    assert_eq!(state.status, "pending", "the round is untouched");
    assert_eq!(state.attempted, 0);
    assert_eq!(state.outbound, None);
    assert_eq!(state.session, None);
    assert_eq!(task_session(&storage, task), None);
}

#[test]
fn pending_close_request_fails_before_http() {
    let dir = TempDir::new("pending-close");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );
    storage
        .request_task_close(task, "stop")
        .expect("request close");

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a close-requested task must not dispatch");
    assert_eq!(error.kind(), DispatchErrorKind::TaskNotDispatchable);
    assert_eq!(server.request_count(), 0, "no HTTP for a close request");

    let state = round_state(&storage, task, 1);
    assert_eq!(state.status, "pending");
    assert_eq!(state.attempted, 0);
    assert_eq!(state.outbound, None);
    assert_eq!(state.session, None);
    assert_eq!(task_session(&storage, task), None);
}

#[test]
fn non_implementing_task_statuses_fail_before_http() {
    let dir = TempDir::new("task-statuses");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    for status in TaskStatus::ALL {
        if status == TaskStatus::Implementing {
            continue;
        }
        storage
            .connection()
            .execute(
                "UPDATE tasks SET status = ?1 WHERE task_id = ?2",
                rusqlite::params![status.as_str(), task.to_string()],
            )
            .expect("set task status");
        let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
            .expect_err("a non-implementing task must not dispatch");
        assert_eq!(
            error.kind(),
            DispatchErrorKind::TaskNotDispatchable,
            "{status:?}"
        );
        assert_eq!(server.request_count(), 0, "{status:?}");
    }
}

#[test]
fn close_request_during_session_resolution_blocks_prompt() {
    let dir = TempDir::new("close-during-resolution");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );
    let title = round_session_title(task, 1);
    let directory = workspace_string.clone();
    let database = layout.database();

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => {
                    let mut connection = connect(&database).expect("close connection");
                    connection
                        .request_task_close(task, "close during resolution")
                        .expect("request close");
                    ok_json(&serde_json::json!([]))
                }
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_created"),
                    Some(&title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a close request during resolution must fail closed");
    assert_eq!(error.kind(), DispatchErrorKind::TaskNotDispatchable);
    assert_eq!(server.prompt_posts().len(), 0, "no prompt POST");
    assert_eq!(server.request_count(), 2, "only list + create");

    let state = round_state(&storage, task, 1);
    assert_eq!(state.status, "pending", "no prepare/mark_sent happened");
    assert_eq!(state.attempted, 0);
    assert_eq!(state.outbound, None);
    assert_eq!(state.session.as_deref(), Some("ses_created"));
}

#[test]
fn workspace_mismatch_fails_before_http() {
    let dir = TempDir::new("ws-mismatch");
    let workspace = dir.mkdir("ws");
    let other = dir.mkdir("other");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        other.to_str().expect("utf-8"),
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a workspace mismatch must fail");
    assert_eq!(error.kind(), DispatchErrorKind::WorkspaceMismatch);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn delivery_failure_is_typed_and_not_retried() {
    let dir = TempDir::new("delivery-fail");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );
    let title = round_session_title(task, 1);
    let directory = workspace_string.clone();

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_created"),
                    Some(&title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => {
                    status(500, b"do-not-leak-body!")
                }
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a failed delivery must be a typed error");
    assert_eq!(error.kind(), DispatchErrorKind::Delivery);
    assert_eq!(server.prompt_posts().len(), 1, "exactly one prompt POST");

    let state = round_state(&storage, task, 1);
    assert_eq!(state.status, "sent", "the attempt stays persisted");
    assert_eq!(state.attempted, 1);
    assert!(state.outbound.is_some());
    assert_eq!(state.session.as_deref(), Some("ses_created"));

    let repeated = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("no automatic retry");
    assert_eq!(repeated.kind(), DispatchErrorKind::RoundNotDispatchable);
    assert_eq!(
        server.prompt_posts().len(),
        1,
        "still exactly one prompt POST"
    );
}

#[test]
fn session_resolution_failure_never_sends_prompt() {
    let dir = TempDir::new("session-fail");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );

    let (port, server) = spawn_server(|_request, _state| status(500, b"do-not-leak-body!"));
    let client = build_client(port, &workspace);

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a failed session resolution must fail closed");
    assert_eq!(error.kind(), DispatchErrorKind::Session);
    assert_eq!(server.request_count(), 1, "only the list attempt");
    assert_eq!(server.prompt_posts().len(), 0);

    let state = round_state(&storage, task, 1);
    assert_eq!(state.status, "pending");
    assert_eq!(state.attempted, 0);
    assert_eq!(state.outbound, None);
    assert_eq!(state.session, None);
}

#[test]
fn unmarked_state_is_rejected_before_http() {
    let dir = TempDir::new("unmarked");
    let workspace = dir.mkdir("ws");
    let project = project("proj-1");
    let task = task_id();
    let layout = rust_layout(&dir, "proj-1");
    // A schema-v6 database without the Rust sidecar marker must never be adopted.
    bridge_storage::initialize(layout.database()).expect("schema v6 database");
    {
        let mut storage = connect(layout.database()).expect("generic connection");
        create_task_full(
            &mut storage,
            task,
            &project,
            workspace.to_str().expect("utf-8"),
            "do work",
            Vec::new(),
            Vec::new(),
            None,
        );
    }
    let database_before = std::fs::read(layout.database()).expect("db bytes");
    assert!(
        !layout.marker().exists(),
        "the state is deliberately unmarked"
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("an unmarked state must be rejected");
    assert_eq!(error.kind(), DispatchErrorKind::StateOwnership);
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
fn model_is_included_last_when_the_project_selects_one() {
    let dir = TempDir::new("model");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "do work",
        Vec::new(),
        Vec::new(),
        None,
    );
    let title = round_session_title(task, 1);
    let directory = workspace_string.clone();

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_created"),
                    Some(&title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client_with(port, &workspace, Some("prov/model-x"));

    dispatch_initial_round(&client, &layout, round_ref(task, &project, 1)).expect("dispatch");
    let body = server.prompt_posts()[0].body_json();
    assert_eq!(
        body["model"],
        serde_json::json!({"providerID": "prov", "modelID": "model-x"})
    );
    let rendered = server.prompt_posts()[0]
        .body
        .windows(b"\"model\"".len())
        .position(|window| window == b"\"model\"");
    let message_id = server.prompt_posts()[0]
        .body
        .windows(b"\"messageID\"".len())
        .position(|window| window == b"\"messageID\"");
    assert!(
        rendered > message_id,
        "the model field is inserted after messageID/parts"
    );
}

#[test]
fn redaction_hides_ids_prompt_and_workspace() {
    let dir = TempDir::new("redaction");
    let workspace = dir.mkdir("secret-workspace");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "secret-project");
    let project = project("secret-project");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "SECRET-TASK-TEXT",
        Vec::new(),
        Vec::new(),
        None,
    );
    let title = round_session_title(task, 1);
    let directory = workspace_string.clone();

    let (port, _server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_secret"),
                    Some(&title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let outcome =
        dispatch_initial_round(&client, &layout, round_ref(task, &project, 1)).expect("dispatch");
    let rendered = format!("{outcome} {outcome:?}");
    assert!(!rendered.contains("ses_secret"));
    assert!(!rendered.contains(outcome.outbound_message_id()));
    assert!(!rendered.contains(TASK_UUID));
    assert!(!rendered.contains("secret-workspace"));
    assert!(!rendered.contains("SECRET-TASK-TEXT"));

    let error = dispatch_initial_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("repeated dispatch");
    let rendered_error = format!("{error} {error:?}");
    assert!(!rendered_error.contains("secret-project"));
    assert!(!rendered_error.contains(TASK_UUID));
    assert!(!rendered_error.contains("ses_secret"));
    assert!(!rendered_error.contains("SECRET-TASK-TEXT"));
}

#[test]
fn new_message_id_has_the_reference_shape() {
    let id = new_message_id();
    assert!(id.starts_with("msg_"));
    assert_eq!(id.len(), "msg_".len() + 32);
    assert!(
        id["msg_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
}

// ---------------------------------------------------------------------------
// 7.5 revision round tests
// ---------------------------------------------------------------------------

#[test]
fn revision_dispatch_sends_exact_prompt_and_records_observing() {
    let dir = TempDir::new("rev-happy");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        vec!["Cargo.toml".to_owned(), "src".to_owned()],
        vec!["cargo test".to_owned(), "cargo clippy".to_owned()],
        Some(focused_snapshot()),
    );
    // Seed the previous round's session to prove it is not reused.
    storage
        .bind_round_session(round_ref(task, &project, 1), "ses_round1".to_owned())
        .expect("seed round one session");
    seed_revision_round(&mut storage, task, &project, Some("fix the bug"));
    assert_eq!(
        task_session(&storage, task),
        None,
        "create_revision_round clears the task session"
    );

    let round1_title = round_session_title(task, 1);
    let round2_title = round_session_title(task, 2);
    let round2_title_server = round2_title.clone();
    let directory = workspace_string.clone();
    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                    Some("ses_round1"),
                    Some(&round1_title),
                    Some(&directory),
                )])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_round2"),
                    Some(&round2_title_server),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let outcome = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect("revision dispatch must succeed");
    assert_eq!(outcome.session_source(), SessionResolutionSource::Created);
    assert_eq!(outcome.session().id(), "ses_round2");
    assert_eq!(outcome.round().status, RoundStatus::Observing);
    assert_eq!(outcome.task().status, TaskStatus::Revising);

    let outbound = outcome.outbound_message_id();
    assert!(outbound.starts_with("msg_"));
    assert_eq!(outbound.len(), "msg_".len() + 32);

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "GET list + POST create + POST prompt");
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path(), "/session");
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].path(), "/session");
    assert_eq!(
        requests[1].body_json()["title"],
        serde_json::json!(round2_title),
        "the new round gets its own deterministic title"
    );
    assert_eq!(requests[2].method, "POST");
    assert_eq!(requests[2].path(), "/session/ses_round2/prompt_async");
    for request in &requests {
        assert_scoped_auth(request, &workspace);
    }

    let body = requests[2].body_json();
    assert_eq!(body["messageID"], serde_json::json!(outbound));
    let expected_text = expected_revision_prompt(
        TASK_UUID,
        &workspace_string,
        2,
        "Cargo.toml, src",
        DIRTY_BASELINE_RULE,
        STRICT_GIT_RULE,
        "cargo test; cargo clippy",
        "original task",
        "fix the bug",
    );
    assert_eq!(
        body["parts"],
        serde_json::json!([{ "type": "text", "text": expected_text }])
    );
    assert_eq!(
        expected_text,
        revision_prompt(outcome.task(), client.workspace(), "fix the bug", 2),
        "the dispatched text is exactly the production revision prompt builder output"
    );

    let state = round_state(&storage, task, 2);
    assert_eq!(state.status, "observing");
    assert_eq!(state.attempted, 1);
    assert_eq!(state.outbound.as_deref(), Some(outbound));
    assert_eq!(state.session.as_deref(), Some("ses_round2"));
    assert_eq!(task_session(&storage, task).as_deref(), Some("ses_round2"));

    let persisted_findings: Option<String> = storage
        .connection()
        .query_row(
            "SELECT findings FROM rounds WHERE task_id = ?1 AND round_number = 2",
            rusqlite::params![task.to_string()],
            |row| row.get(0),
        )
        .expect("findings");
    assert_eq!(persisted_findings.as_deref(), Some("fix the bug"));
}

#[test]
fn revision_dispatch_rejects_missing_text_before_http() {
    let dir = TempDir::new("rev-missing-text");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        workspace.to_str().unwrap(),
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, None);
    let (port, server) = spawn_server(|_, _| status(500, b"unexpected"));
    let client = build_client(port, &workspace);
    let error =
        dispatch_revision_round(&client, &layout, round_ref(task, &project, 2)).unwrap_err();
    assert_eq!(error.kind(), DispatchErrorKind::StructuredFindingsInvariant);
    assert!(server.requests().is_empty());
    assert_eq!(round_state(&storage, task, 2).status, "failed");
    assert_eq!(round_state(&storage, task, 2).attempted, 0);
    assert_eq!(
        storage.get_task(task).unwrap().unwrap().status,
        TaskStatus::Failed
    );
}

#[test]
fn revision_attempt_and_outbound_are_persisted_before_prompt_post() {
    let dir = TempDir::new("rev-persist-before-send");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));

    let round1_title = round_session_title(task, 1);
    let round2_title = round_session_title(task, 2);
    let directory = workspace_string.clone();
    let database = layout.database();
    let observed: Arc<Mutex<Option<RoundState>>> = Arc::new(Mutex::new(None));
    let observed_for_handler = Arc::clone(&observed);
    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                    Some("ses_round1"),
                    Some(&round1_title),
                    Some(&directory),
                )])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_round2"),
                    Some(&round2_title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => {
                    let snapshot = {
                        let connection = connect(&database).expect("observer connection");
                        round_state(&connection, task, 2)
                    };
                    *observed_for_handler.lock().expect("observed") = Some(snapshot);
                    no_content()
                }
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let outcome = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect("revision dispatch must succeed");
    let at_post = observed
        .lock()
        .expect("observed")
        .clone()
        .expect("the prompt POST was observed");
    assert_eq!(
        at_post.status, "sent",
        "the attempt is persisted before the POST"
    );
    assert_eq!(at_post.attempted, 1);
    assert_eq!(
        at_post.outbound.as_deref(),
        Some(outcome.outbound_message_id())
    );
    assert_eq!(at_post.session.as_deref(), Some("ses_round2"));
    assert_eq!(
        server.prompt_posts()[0].body_json()["messageID"],
        serde_json::json!(outcome.outbound_message_id())
    );
}

#[test]
fn revision_prepared_outbound_id_is_reused_without_second_prepare() {
    let dir = TempDir::new("rev-reuse-outbound");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));
    storage
        .prepare_round(round_ref(task, &project, 2), "msg_prepared".to_owned())
        .expect("prepare");
    storage
        .bind_round_session(round_ref(task, &project, 2), "ses_existing".to_owned())
        .expect("seed bind");

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("POST", "/session/ses_existing/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let outcome = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect("a prepared but unsent revision must dispatch");
    assert_eq!(outcome.outbound_message_id(), "msg_prepared");
    assert_eq!(server.request_count(), 1, "only the prompt POST is sent");
    assert_eq!(
        server.prompt_posts()[0].body_json()["messageID"],
        serde_json::json!("msg_prepared")
    );
    assert_eq!(round_state(&storage, task, 2).status, "observing");
}

#[test]
fn repeated_revision_dispatch_is_rejected_before_send() {
    let dir = TempDir::new("rev-repeated");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));
    let round1_title = round_session_title(task, 1);
    let round2_title = round_session_title(task, 2);
    let directory = workspace_string.clone();
    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                    Some("ses_round1"),
                    Some(&round1_title),
                    Some(&directory),
                )])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_round2"),
                    Some(&round2_title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect("first dispatch");
    assert_eq!(server.prompt_posts().len(), 1);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("a repeated revision dispatch must fail");
    assert_eq!(error.kind(), DispatchErrorKind::RoundNotDispatchable);
    assert_eq!(server.prompt_posts().len(), 1, "no second prompt POST");
}

#[test]
fn revision_dispatch_rejects_an_implement_round_before_http() {
    let dir = TempDir::new("rev-cross-kind");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 1))
        .expect_err("a revision dispatch must reject an implement round");
    assert_eq!(error.kind(), DispatchErrorKind::ImplementNotSupported);
    assert_eq!(server.request_count(), 0, "no HTTP for a cross-kind round");
    assert_eq!(round_state(&storage, task, 1).status, "pending");
}

#[test]
fn revision_dispatch_rejects_non_revising_task_status_before_http() {
    let dir = TempDir::new("rev-task-statuses");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    for status in TaskStatus::ALL {
        if status == TaskStatus::Revising {
            continue;
        }
        storage
            .connection()
            .execute(
                "UPDATE tasks SET status = ?1 WHERE task_id = ?2",
                rusqlite::params![status.as_str(), task.to_string()],
            )
            .expect("set task status");
        let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
            .expect_err("a non-revising task must not dispatch a revision");
        assert_eq!(
            error.kind(),
            DispatchErrorKind::TaskNotDispatchable,
            "{status:?}"
        );
        assert_eq!(server.request_count(), 0, "{status:?}");
    }
}

#[test]
fn revision_pending_close_request_fails_before_http() {
    let dir = TempDir::new("rev-pending-close");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));
    storage
        .request_task_close(task, "stop")
        .expect("request close");

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("a close-requested task must not dispatch a revision");
    assert_eq!(error.kind(), DispatchErrorKind::TaskNotDispatchable);
    assert_eq!(server.request_count(), 0, "no HTTP for a close request");

    let state = round_state(&storage, task, 2);
    assert_eq!(state.status, "pending");
    assert_eq!(state.attempted, 0);
    assert_eq!(state.outbound, None);
    assert_eq!(state.session, None);
}

#[test]
fn revision_close_request_during_session_resolution_blocks_prompt() {
    let dir = TempDir::new("rev-close-during-resolution");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));
    let round2_title = round_session_title(task, 2);
    let directory = workspace_string.clone();
    let database = layout.database();

    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => {
                    let mut connection = connect(&database).expect("close connection");
                    connection
                        .request_task_close(task, "close during resolution")
                        .expect("request close");
                    ok_json(&serde_json::json!([]))
                }
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_round2"),
                    Some(&round2_title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("a close request during resolution must fail closed");
    assert_eq!(error.kind(), DispatchErrorKind::TaskNotDispatchable);
    assert_eq!(server.prompt_posts().len(), 0, "no prompt POST");
    assert_eq!(server.request_count(), 2, "only list + create");

    let state = round_state(&storage, task, 2);
    assert_eq!(state.status, "pending", "no prepare/mark_sent happened");
    assert_eq!(state.attempted, 0);
    assert_eq!(state.outbound, None);
    assert_eq!(state.session.as_deref(), Some("ses_round2"));
}

#[test]
fn revision_workspace_mismatch_fails_before_http() {
    let dir = TempDir::new("rev-ws-mismatch");
    let workspace = dir.mkdir("ws");
    let other = dir.mkdir("other");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        other.to_str().expect("utf-8"),
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("a workspace mismatch must fail");
    assert_eq!(error.kind(), DispatchErrorKind::WorkspaceMismatch);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn revision_stale_foreign_and_invalid_inputs_fail_before_http() {
    let dir = TempDir::new("rev-inputs");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let primary = project("proj-1");
    let other = project("proj-2");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &primary,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &primary, Some("note"));

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let zero = dispatch_revision_round(&client, &layout, round_ref(task, &primary, 0))
        .expect_err("round zero");
    assert_eq!(zero.kind(), DispatchErrorKind::InvalidInput);

    let stale = dispatch_revision_round(&client, &layout, round_ref(task, &primary, 1))
        .expect_err("a stale implement round must fail");
    assert_eq!(stale.kind(), DispatchErrorKind::StaleRound);

    let foreign = dispatch_revision_round(&client, &layout, round_ref(task, &other, 2))
        .expect_err("foreign project");
    assert_eq!(foreign.kind(), DispatchErrorKind::TaskMismatch);

    let missing = TaskId::from_str("11111111-1111-1111-1111-111111111111").expect("task id");
    let unknown = dispatch_revision_round(&client, &layout, round_ref(missing, &primary, 2))
        .expect_err("missing task");
    assert_eq!(unknown.kind(), DispatchErrorKind::UnknownTask);

    assert_eq!(server.request_count(), 0, "no HTTP for invalid inputs");
}

#[test]
fn revision_delivery_failure_is_typed_and_not_retried() {
    let dir = TempDir::new("rev-delivery-fail");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));
    let round1_title = round_session_title(task, 1);
    let round2_title = round_session_title(task, 2);
    let directory = workspace_string.clone();
    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                    Some("ses_round1"),
                    Some(&round1_title),
                    Some(&directory),
                )])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_round2"),
                    Some(&round2_title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => {
                    status(500, b"do-not-leak-body!")
                }
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("a failed delivery must be a typed error");
    assert_eq!(error.kind(), DispatchErrorKind::Delivery);
    assert_eq!(server.prompt_posts().len(), 1, "exactly one prompt POST");

    let state = round_state(&storage, task, 2);
    assert_eq!(state.status, "sent", "the attempt stays persisted");
    assert_eq!(state.attempted, 1);
    assert!(state.outbound.is_some());
    assert_eq!(state.session.as_deref(), Some("ses_round2"));

    let repeated = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("no automatic retry");
    assert_eq!(repeated.kind(), DispatchErrorKind::RoundNotDispatchable);
    assert_eq!(
        server.prompt_posts().len(),
        1,
        "still exactly one prompt POST"
    );
}

#[test]
fn revision_session_resolution_failure_never_sends_prompt() {
    let dir = TempDir::new("rev-session-fail");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));

    let (port, server) = spawn_server(|_request, _state| status(500, b"do-not-leak-body!"));
    let client = build_client(port, &workspace);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("a failed session resolution must fail closed");
    assert_eq!(error.kind(), DispatchErrorKind::Session);
    assert_eq!(server.request_count(), 1, "only the list attempt");
    assert_eq!(server.prompt_posts().len(), 0);

    let state = round_state(&storage, task, 2);
    assert_eq!(state.status, "pending");
    assert_eq!(state.attempted, 0);
    assert_eq!(state.outbound, None);
    assert_eq!(state.session, None);
}

#[test]
fn revision_prepare_storage_failure_is_typed_and_leaves_round_unprepared() {
    let dir = TempDir::new("rev-prepare-fail");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));

    // The production `prepare_round` write aborts on this fresh Rust-owned
    // database, so the dispatch-level `Storage` handling is exercised without
    // touching the production schema or code.
    install_trigger(
        &storage,
        "CREATE TRIGGER fail_prepare BEFORE UPDATE OF outbound_message_id ON rounds \
         BEGIN SELECT RAISE(ABORT, 'test prepare failure'); END;",
    );

    let round1_title = round_session_title(task, 1);
    let round2_title = round_session_title(task, 2);
    let directory = workspace_string.clone();
    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                    Some("ses_round1"),
                    Some(&round1_title),
                    Some(&directory),
                )])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_round2"),
                    Some(&round2_title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("a failed prepare must be a typed storage error");
    assert_eq!(error.kind(), DispatchErrorKind::Storage);
    assert_eq!(server.prompt_posts().len(), 0, "no prompt POST");

    let state = round_state(&storage, task, 2);
    assert_eq!(state.status, "pending");
    assert_eq!(state.attempted, 0);
    assert_eq!(state.outbound, None, "the outbound id is never persisted");
    assert_eq!(state.session.as_deref(), Some("ses_round2"));
}

#[test]
fn revision_sent_storage_failure_is_typed_and_keeps_prepared_round_pending() {
    let dir = TempDir::new("rev-sent-fail");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));

    // `prepare_round` succeeds and persists the outbound id; the following
    // `mark_round_sent` write aborts, so the attempt must never reach the POST.
    install_trigger(
        &storage,
        "CREATE TRIGGER fail_sent BEFORE UPDATE OF status ON rounds \
         WHEN NEW.status = 'sent' \
         BEGIN SELECT RAISE(ABORT, 'test sent failure'); END;",
    );

    let round1_title = round_session_title(task, 1);
    let round2_title = round_session_title(task, 2);
    let directory = workspace_string.clone();
    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                    Some("ses_round1"),
                    Some(&round1_title),
                    Some(&directory),
                )])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_round2"),
                    Some(&round2_title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("a failed sent write must be a typed storage error");
    assert_eq!(error.kind(), DispatchErrorKind::Storage);
    assert_eq!(server.prompt_posts().len(), 0, "no prompt POST");

    let state = round_state(&storage, task, 2);
    assert_eq!(state.status, "pending", "the round was never marked sent");
    assert_eq!(state.attempted, 0);
    assert!(
        state.outbound.is_some(),
        "the prepared outbound id is preserved"
    );
    assert_eq!(state.session.as_deref(), Some("ses_round2"));
}

#[test]
fn revision_observing_storage_failure_is_typed_after_single_post_and_not_retried() {
    let dir = TempDir::new("rev-observing-fail");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "original task",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("note"));

    // The POST succeeds but the final `mark_round_observing` write aborts: the
    // dispatch must report a typed storage error instead of a false success and
    // must not resend.
    install_trigger(
        &storage,
        "CREATE TRIGGER fail_observing BEFORE UPDATE OF status ON rounds \
         WHEN NEW.status = 'observing' \
         BEGIN SELECT RAISE(ABORT, 'test observing failure'); END;",
    );

    let round1_title = round_session_title(task, 1);
    let round2_title = round_session_title(task, 2);
    let directory = workspace_string.clone();
    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                    Some("ses_round1"),
                    Some(&round1_title),
                    Some(&directory),
                )])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_round2"),
                    Some(&round2_title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("a failed observing write must not be a false success");
    assert_eq!(error.kind(), DispatchErrorKind::Storage);
    assert_eq!(server.prompt_posts().len(), 1, "exactly one prompt POST");

    let state = round_state(&storage, task, 2);
    assert_eq!(state.status, "sent", "the attempt stays persisted");
    assert_eq!(state.attempted, 1);
    assert!(state.outbound.is_some());

    let repeated = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("no automatic retry");
    assert_eq!(repeated.kind(), DispatchErrorKind::RoundNotDispatchable);
    assert_eq!(
        server.prompt_posts().len(),
        1,
        "still exactly one prompt POST"
    );
}

#[test]
fn revision_unmarked_state_is_rejected_before_http() {
    let dir = TempDir::new("rev-unmarked");
    let workspace = dir.mkdir("ws");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let project = project("proj-1");
    let task = task_id();
    let layout = rust_layout(&dir, "proj-1");
    // A schema-v6 database without the Rust sidecar marker must never be adopted.
    bridge_storage::initialize(layout.database()).expect("schema v6 database");
    {
        let mut storage = connect(layout.database()).expect("generic connection");
        create_task_full(
            &mut storage,
            task,
            &project,
            &workspace_string,
            "original task",
            Vec::new(),
            Vec::new(),
            None,
        );
        seed_revision_round(&mut storage, task, &project, Some("note"));
    }
    let database_before = std::fs::read(layout.database()).expect("db bytes");
    assert!(
        !layout.marker().exists(),
        "the state is deliberately unmarked"
    );

    let (port, server) = spawn_server(|_request, _state| ok_json(&serde_json::json!([])));
    let client = build_client(port, &workspace);

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("an unmarked state must be rejected");
    assert_eq!(error.kind(), DispatchErrorKind::StateOwnership);
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
fn revision_redaction_hides_ids_prompt_findings_and_workspace() {
    let dir = TempDir::new("rev-redaction");
    let workspace = dir.mkdir("secret-workspace");
    let workspace_string = std::fs::canonicalize(&workspace)
        .expect("canonical")
        .to_string_lossy()
        .into_owned();
    let (layout, mut storage) = open_storage(&dir, "secret-project");
    let project = project("secret-project");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        &workspace_string,
        "SECRET-TASK-TEXT",
        Vec::new(),
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("SECRET-FINDINGS"));
    let round1_title = round_session_title(task, 1);
    let round2_title = round_session_title(task, 2);
    let directory = workspace_string.clone();
    let (port, server) =
        spawn_server(
            move |request, _state| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                    Some("ses_round1"),
                    Some(&round1_title),
                    Some(&directory),
                )])),
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_secret"),
                    Some(&round2_title),
                    Some(&directory),
                )),
                ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);

    let outcome = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect("revision dispatch");
    let rendered = format!("{outcome} {outcome:?}");
    assert!(!rendered.contains("ses_secret"));
    assert!(!rendered.contains(outcome.outbound_message_id()));
    assert!(!rendered.contains(TASK_UUID));
    assert!(!rendered.contains("secret-workspace"));
    assert!(!rendered.contains("SECRET-TASK-TEXT"));
    assert!(!rendered.contains("SECRET-FINDINGS"));

    let error = dispatch_revision_round(&client, &layout, round_ref(task, &project, 2))
        .expect_err("repeated dispatch");
    let rendered_error = format!("{error} {error:?}");
    assert!(!rendered_error.contains("secret-project"));
    assert!(!rendered_error.contains(TASK_UUID));
    assert!(!rendered_error.contains("ses_secret"));
    assert!(!rendered_error.contains("SECRET-TASK-TEXT"));
    assert!(!rendered_error.contains("SECRET-FINDINGS"));

    let _ = server;
}

// 7.13: persisted validation must stop delivery before any session/prompt HTTP.
fn structured_item(path: &str) -> serde_json::Value {
    serde_json::json!({"severity":"error","path":path,"line":2,"code":"BUG-1","message":"fix\n the operator"})
}

fn seed_structured_revision(
    dir: &TempDir,
) -> (
    PathBuf,
    bridge_storage::RustStateLayout,
    StorageConnection,
    TaskId,
    ProjectId,
) {
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        workspace.to_str().unwrap(),
        "original",
        vec!["src/".to_owned()],
        Vec::new(),
        None,
    );
    seed_revision_round(&mut storage, task, &project, Some("fix"));
    (workspace, layout, storage, task, project)
}

fn set_structured(storage: &StorageConnection, task: TaskId, raw: &str) {
    storage
        .connection()
        .execute(
            "UPDATE rounds SET structured_findings=?1 WHERE task_id=?2 AND round_number=2",
            rusqlite::params![raw, task.to_string()],
        )
        .unwrap();
}

#[test]
fn persisted_structured_corruption_fails_terminally_before_any_http() {
    let mut extra = structured_item("src/a.rs");
    extra["extra"] = serde_json::json!(true);
    let mut severity = structured_item("src/a.rs");
    severity["severity"] = serde_json::json!("fatal");
    let mut line = structured_item("src/a.rs");
    line["line"] = serde_json::json!(true);
    let invalid = vec![
        "BLOB".to_owned(),
        "{".to_owned(),
        "".to_owned(),
        "null".to_owned(),
        "{}".to_owned(),
        "42".to_owned(),
        "[42]".to_owned(),
        serde_json::json!([extra]).to_string(),
        serde_json::json!([severity]).to_string(),
        serde_json::json!([line]).to_string(),
        serde_json::json!([structured_item("outside.rs")]).to_string(),
        serde_json::json!([structured_item("../src/a.rs")]).to_string(),
        serde_json::Value::Array(vec![structured_item("src/a.rs"); 201]).to_string(),
    ];
    for raw in invalid {
        let dir = TempDir::new("structured-corrupt");
        let (workspace, layout, storage, task, project) = seed_structured_revision(&dir);
        if raw == "BLOB" {
            storage
                .connection()
                .execute(
                    "UPDATE rounds SET structured_findings=?1 WHERE task_id=?2 AND round_number=2",
                    rusqlite::params![vec![0xff_u8], task.to_string()],
                )
                .unwrap();
        } else {
            set_structured(&storage, task, &raw);
        }
        let (port, server) = spawn_server(|_, _| status(500, b"unexpected"));
        let client = build_client(port, &workspace);
        let error =
            dispatch_revision_round(&client, &layout, round_ref(task, &project, 2)).unwrap_err();
        assert_eq!(error.kind(), DispatchErrorKind::StructuredFindingsInvariant);
        assert_eq!(server.request_count(), 0);
        assert_eq!(round_state(&storage, task, 2).status, "failed");
        assert_eq!(round_state(&storage, task, 2).attempted, 0);
        assert_eq!(round_state(&storage, task, 2).outbound, None);
        assert_eq!(
            storage.get_task(task).unwrap().unwrap().status,
            TaskStatus::Failed
        );
        let code: String = storage
            .connection()
            .query_row(
                "SELECT error_code FROM rounds WHERE task_id=?1 AND round_number=2",
                [task.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(code, "structured_findings_invariant");
        assert!(!format!("{error:?} {error}").contains("outside.rs"));
        let again =
            dispatch_revision_round(&client, &layout, round_ref(task, &project, 2)).unwrap_err();
        assert_eq!(again.kind(), DispatchErrorKind::TaskNotDispatchable);
        assert_eq!(server.request_count(), 0);
    }
}

#[test]
fn structured_revision_renders_normalized_persisted_prompt() {
    for raw in [
        "[]".to_owned(),
        serde_json::json!([structured_item("src/./a.rs")]).to_string(),
    ] {
        let dir = TempDir::new("structured-prompt");
        let (workspace, layout, storage, task, project) = seed_structured_revision(&dir);
        set_structured(&storage, task, &raw);
        let title = round_session_title(task, 2);
        let directory = workspace.to_str().unwrap().to_owned();
        let (port, server) =
            spawn_server(
                move |request, _| match (request.method.as_str(), request.path()) {
                    ("GET", "/session") => ok_json(&serde_json::json!([])),
                    ("POST", "/session") => ok_json(&session_json(
                        Some("ses_revision"),
                        Some(&title),
                        Some(&directory),
                    )),
                    ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                    _ => status(500, b"unexpected"),
                },
            );
        let client = build_client(port, &workspace);
        let outcome =
            dispatch_revision_round(&client, &layout, round_ref(task, &project, 2)).unwrap();
        let expected = if raw == "[]" {
            "fix"
        } else {
            "fix\n\nСтруктурированные замечания ревью:\n1. [error] src/a.rs:2 (BUG-1) fix the operator"
        };
        assert_eq!(
            server.prompt_posts()[0].body_json()["parts"][0]["text"],
            revision_prompt(outcome.task(), &workspace, expected, 2)
        );
        assert_eq!(server.prompt_posts().len(), 1);
        assert_eq!(round_state(&storage, task, 2).status, "observing");
    }
}

#[test]
fn structured_findings_are_revalidated_after_session_resolution() {
    let dir = TempDir::new("structured-session-race");
    let (workspace, layout, storage, task, project) = seed_structured_revision(&dir);
    set_structured(
        &storage,
        task,
        &serde_json::json!([structured_item("src/a.rs")]).to_string(),
    );
    let database = layout.database();
    let title = round_session_title(task, 2);
    let directory = workspace.to_str().unwrap().to_owned();
    let (port, server) =
        spawn_server(
            move |request, _| match (request.method.as_str(), request.path()) {
                ("GET", "/session") => {
                    let storage = connect(&database).unwrap();
                    set_structured(&storage, task, "null");
                    ok_json(&serde_json::json!([]))
                }
                ("POST", "/session") => ok_json(&session_json(
                    Some("ses_revision"),
                    Some(&title),
                    Some(&directory),
                )),
                _ => status(500, b"unexpected"),
            },
        );
    let client = build_client(port, &workspace);
    let error =
        dispatch_revision_round(&client, &layout, round_ref(task, &project, 2)).unwrap_err();
    assert_eq!(error.kind(), DispatchErrorKind::StructuredFindingsInvariant);
    assert_eq!(server.request_count(), 2);
    assert_eq!(server.prompt_posts().len(), 0);
    assert_eq!(round_state(&storage, task, 2).status, "failed");
    assert_eq!(round_state(&storage, task, 2).attempted, 0);
}

#[cfg(unix)]
#[test]
fn persisted_symlink_escape_stops_revision_delivery() {
    let dir = TempDir::new("structured-symlink");
    let (workspace, layout, storage, task, project) = seed_structured_revision(&dir);
    let outside = dir.mkdir("outside");
    std::os::unix::fs::symlink(outside.join("missing"), workspace.join("src")).unwrap();
    set_structured(
        &storage,
        task,
        &serde_json::json!([structured_item("src/a.rs")]).to_string(),
    );
    let (port, server) = spawn_server(|_, _| status(500, b"unexpected"));
    let error = dispatch_revision_round(
        &build_client(port, &workspace),
        &layout,
        round_ref(task, &project, 2),
    )
    .unwrap_err();
    assert_eq!(error.kind(), DispatchErrorKind::StructuredFindingsInvariant);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn invariant_finish_storage_failure_rolls_back_without_http() {
    let dir = TempDir::new("structured-finish-failure");
    let (workspace, layout, storage, task, project) = seed_structured_revision(&dir);
    set_structured(&storage, task, "{}");
    install_trigger(
        &storage,
        "CREATE TRIGGER reject_failed BEFORE UPDATE OF status ON rounds WHEN NEW.status='failed' BEGIN SELECT RAISE(ABORT,'test'); END;",
    );
    let (port, server) = spawn_server(|_, _| status(500, b"unexpected"));
    let error = dispatch_revision_round(
        &build_client(port, &workspace),
        &layout,
        round_ref(task, &project, 2),
    )
    .unwrap_err();
    assert_eq!(error.kind(), DispatchErrorKind::Storage);
    assert_eq!(server.request_count(), 0);
    assert_eq!(round_state(&storage, task, 2).status, "pending");
    assert_eq!(
        storage.get_task(task).unwrap().unwrap().status,
        TaskStatus::Revising
    );
}

#[test]
fn request_revision_persistence_replay_conflict_and_rollback() {
    let dir = TempDir::new("structured-create");
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        workspace.to_str().unwrap(),
        "original",
        vec!["src/".to_owned()],
        Vec::new(),
        None,
    );
    storage
        .mark_round_observing(round_ref(task, &project, 1))
        .unwrap();
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
        .unwrap();
    let value = serde_json::json!([structured_item("src/./a.rs")]);
    let findings = bridge_worker::validate_revision_findings(
        "fix",
        Some(&value),
        &workspace,
        &[],
        &["src/".to_owned()],
    )
    .unwrap();
    let round = round_ref(task, &project, 2);
    install_trigger(
        &storage,
        "CREATE TRIGGER reject_structured BEFORE UPDATE OF structured_findings ON rounds BEGIN SELECT RAISE(ABORT,'test'); END;",
    );
    assert!(
        findings
            .create_round(&mut storage, round.clone(), "review".to_owned())
            .is_err()
    );
    assert_eq!(
        storage.get_task(task).unwrap().unwrap().status,
        TaskStatus::AwaitingReview
    );
    let count: i64 = storage
        .connection()
        .query_row("SELECT count(*) FROM rounds", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    storage
        .connection()
        .execute_batch("DROP TRIGGER reject_structured")
        .unwrap();
    let first = findings
        .create_round(&mut storage, round.clone(), "review".to_owned())
        .unwrap();
    assert_eq!(first.state.task.revision_count, 1);
    assert!(!first.replayed);
    let debug = format!("{first:?}");
    for sensitive in ["original", "fix", TASK_UUID, workspace.to_str().unwrap()] {
        assert!(!debug.contains(sensitive));
    }
    let persisted = storage
        .get_round_structured_findings(&round)
        .unwrap()
        .unwrap();
    assert_eq!(persisted.as_slice()[0].path, "src/a.rs");
    let events: i64 = storage
        .connection()
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let canonical = bridge_worker::validate_revision_findings(
        "fix",
        Some(&serde_json::json!([structured_item("src/a.rs")])),
        &workspace,
        &[],
        &["src/".to_owned()],
    )
    .unwrap();
    let replay = canonical
        .create_round(&mut storage, round.clone(), "review".to_owned())
        .unwrap();
    assert_eq!(first.state, replay.state);
    assert!(replay.replayed);
    assert_eq!(
        events,
        storage
            .connection()
            .query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap()
    );
    let changed = bridge_worker::validate_revision_findings(
        "different",
        Some(&value),
        &workspace,
        &[],
        &["src/".to_owned()],
    )
    .unwrap();
    assert!(matches!(
        changed.create_round(&mut storage, round.clone(), "review".to_owned()),
        Err(bridge_storage::RoundUpdateError::RequestConflict)
    ));
    let foreign = round_ref(task, &crate::project("foreign"), 2);
    assert!(matches!(
        findings.create_round(&mut storage, foreign, "review".to_owned()),
        Err(bridge_storage::RoundUpdateError::ProjectMismatch)
    ));

    // Concurrent None/[] requests share the historical hash and create once.
    storage.mark_round_observing(round.clone()).unwrap();
    storage
        .finish_round(FinishRoundInput {
            round,
            round_status: RoundStatus::Complete,
            task_status: TaskStatus::AwaitingReview,
            response_message_id: None,
            response: None,
            error_code: None,
            result_json: None,
        })
        .unwrap();
    let text = bridge_worker::validate_revision_findings(
        "text",
        None,
        &workspace,
        &[],
        &["src/".to_owned()],
    )
    .unwrap();
    let empty = bridge_worker::validate_revision_findings(
        "text",
        Some(&serde_json::json!([])),
        &workspace,
        &[],
        &["src/".to_owned()],
    )
    .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut threads = Vec::new();
    for request in [text, empty] {
        let barrier = barrier.clone();
        let database = layout.database();
        let round = round_ref(task, &project, 3);
        threads.push(std::thread::spawn(move || {
            let mut storage = connect(&database).unwrap();
            barrier.wait();
            request
                .create_round(&mut storage, round, "text-review".to_owned())
                .unwrap()
        }));
    }
    let outcomes: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(outcomes.iter().filter(|o| o.replayed).count(), 1);
    assert_eq!(outcomes[0].state, outcomes[1].state);
    assert_eq!(storage.get_task(task).unwrap().unwrap().revision_count, 2);
    let count: i64 = storage
        .connection()
        .query_row(
            "SELECT count(*) FROM rounds WHERE request_id='text-review'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn pre_send_invariant_failure_is_fenced_and_preserves_close_priority() {
    let dir = TempDir::new("findings-failure-guards");
    let (_workspace, _layout, mut storage, task, project) = seed_structured_revision(&dir);
    let current = round_ref(task, &project, 2);
    assert!(
        storage
            .fail_revision_findings(round_ref(task, &project, 1))
            .is_err()
    );
    assert!(
        storage
            .fail_revision_findings(round_ref(task, &crate::project("foreign"), 2))
            .is_err()
    );
    assert_eq!(round_state(&storage, task, 2).status, "pending");
    storage
        .request_task_close(task, "close before failure")
        .unwrap();
    let outcome = storage.fail_revision_findings(current.clone()).unwrap();
    assert_eq!(outcome.task.status, TaskStatus::Closed);
    assert_eq!(outcome.round.status, RoundStatus::Failed);
    assert!(storage.fail_revision_findings(current).is_err());
    let count: i64 = storage
        .connection()
        .query_row("SELECT count(*) FROM events WHERE kind='closed'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);

    let dir = TempDir::new("findings-failure-attempted");
    let (_workspace, _layout, mut storage, task, project) = seed_structured_revision(&dir);
    let current = round_ref(task, &project, 2);
    storage
        .prepare_round(current.clone(), "msg_test".to_owned())
        .unwrap();
    storage.mark_round_sent(current.clone()).unwrap();
    assert!(storage.fail_revision_findings(current).is_err());
    assert_eq!(round_state(&storage, task, 2).status, "sent");
}

#[test]
fn accounting_history_persistence_and_budget_snapshot() {
    let dir = TempDir::new("usage-history");
    let (workspace, _layout, mut storage, task, project) = seed_structured_revision(&dir);
    let round = round_ref(task, &project, 2);
    storage
        .prepare_round(round.clone(), "msg_out".to_owned())
        .unwrap();
    storage.mark_round_sent(round.clone()).unwrap();
    storage.mark_round_observing(round.clone()).unwrap();
    let messages = serde_json::json!([
        {"info":{"role":"user","id":"old"},"parts":[]},
        {"info":{"role":"assistant","parentID":"old","tokens":{"input":1000}},"parts":[]},
        {"info":{"role":"user","id":"msg_out"},"parts":[]},
        {"info":{"role":"assistant","parentID":"msg_out","providerID":"provider","modelID":"model","tokens":{"input":10,"output":2},"cost":0.25},"parts":[]},
        {"info":{"role":"user","id":"continued"},"parts":[]},
        {"info":{"role":"assistant","parentID":"continued","tokens":{"input":5,"cache":{"read":3}},"cost":0.5},"parts":[]}
    ]);
    let (port, _server) = spawn_server(move |_, _| ok_json(&messages));
    let messages = build_client(port, &workspace)
        .list_messages("session")
        .unwrap();
    let observed = bridge_worker::usage::observed_accounting(&messages, "msg_out");
    assert_eq!(
        observed["usage"],
        serde_json::json!({"input":15.0,"output":2.0,"reasoning":0.0,"cache_read":3.0,"cache_write":0.0,"cost":0.75})
    );
    assert_eq!(
        observed,
        bridge_worker::usage::observed_accounting(&messages, "msg_out")
    );
    assert_eq!(
        observed["model"],
        serde_json::json!({"provider_id":"provider","model_id":"model"})
    );
    let input = FinishRoundInput {
        round: round.clone(),
        round_status: RoundStatus::Complete,
        task_status: TaskStatus::AwaitingReview,
        response_message_id: None,
        response: None,
        error_code: None,
        result_json: Some(serde_json::json!({"tool_errors":[]})),
    };
    install_trigger(
        &storage,
        "CREATE TRIGGER reject_usage BEFORE INSERT ON events WHEN NEW.kind='complete' BEGIN SELECT RAISE(ABORT,'test'); END;",
    );
    assert!(
        bridge_worker::usage::finish_round_with_accounting(&mut storage, input.clone(), &messages)
            .is_err()
    );
    assert_eq!(round_state(&storage, task, 2).status, "observing");
    storage
        .connection()
        .execute_batch("DROP TRIGGER reject_usage")
        .unwrap();
    let outcome =
        bridge_worker::usage::finish_round_with_accounting(&mut storage, input, &messages).unwrap();
    assert_eq!(
        outcome.round.result_json.as_ref().unwrap()["usage"],
        observed["usage"]
    );
    assert_eq!(
        outcome.round.result_json.unwrap()["tool_errors"],
        serde_json::json!([])
    );
    storage
        .connection()
        .execute(
            "UPDATE tasks SET budget_json=?1 WHERE task_id=?2",
            rusqlite::params![r#"{"limits":{"input":15,"cost":1}}"#, task.to_string()],
        )
        .unwrap();
    let decision = storage
        .revision_budget_decision(task, &project, false)
        .unwrap();
    assert_eq!(decision.error, Some("budget_exhausted"));
    assert!(
        storage
            .revision_budget_decision(task, &project, true)
            .unwrap()
            .overridden
    );
    assert!(
        storage
            .revision_budget_decision(task, &crate::project("foreign"), false)
            .is_err()
    );
    storage
        .connection()
        .execute(
            "UPDATE tasks SET budget_json='{}' WHERE task_id=?1",
            [task.to_string()],
        )
        .unwrap();
    assert_eq!(
        storage
            .revision_budget_decision(task, &project, false)
            .unwrap()
            .error,
        Some("budget_corrupt")
    );
    assert!(
        storage
            .revision_budget_decision(task, &project, true)
            .unwrap()
            .overridden
    );
    assert_eq!(round_state(&storage, task, 2).status, "complete");
}

fn checkpoint_git(root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}
fn checkpoint_repo(dir: &TempDir) -> PathBuf {
    let root = dir.mkdir("repo");
    checkpoint_git(&root, &["init", "-q"]);
    std::fs::write(root.join("a"), "before").unwrap();
    std::fs::write(root.join("b"), "rename me").unwrap();
    std::fs::write(root.join(".gitignore"), "ignored\n").unwrap();
    checkpoint_git(&root, &["add", "."]);
    checkpoint_git(&root, &["commit", "-qm", "baseline"]);
    root
}
fn checkpoint_baseline(root: &Path) -> serde_json::Value {
    let manifest = bridge_git::worktree_manifest(root)
        .unwrap()
        .entries()
        .iter()
        .map(|entry| {
            (
                entry.path().to_str().unwrap().to_owned(),
                serde_json::json!(entry.digest_hex()),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({"manifest":manifest})
}

#[test]
fn checkpoint_rounds_are_predecessor_only_and_finish_atomically() {
    let dir = TempDir::new("checkpoint-rounds");
    let root = checkpoint_repo(&dir);
    let baseline = checkpoint_baseline(&root);
    let (_layout, mut storage) = open_storage(&dir, "proj-1");
    let project = project("proj-1");
    let task = task_id();
    create_task_full(
        &mut storage,
        task,
        &project,
        root.to_str().unwrap(),
        "task",
        vec!["a".to_owned()],
        Vec::new(),
        Some(baseline),
    );
    std::fs::write(root.join("a"), "round one").unwrap();
    std::fs::rename(root.join("b"), root.join("c")).unwrap();
    std::fs::write(root.join("binary"), [0, 1, 2]).unwrap();
    std::fs::write(root.join("ignored"), "never read").unwrap();
    let first = bridge_worker::checkpoint::build_round_checkpoint(
        &storage,
        &round_ref(task, &project, 1),
        &root,
        &[],
        None,
    )
    .unwrap();
    let value = serde_json::to_value(&first).unwrap();
    assert_eq!(
        value["repositories"][0]["diff_stat"]["counts"],
        serde_json::json!({"added":1,"modified":1,"deleted":0,"renamed":1})
    );
    assert_eq!(
        value["repositories"][0]["changed"]["binary"]["kind"],
        "binary"
    );
    assert!(value["repositories"][0]["changed"].get("ignored").is_none());
    assert!(!value.to_string().contains(root.to_str().unwrap()));
    storage
        .mark_round_observing(round_ref(task, &project, 1))
        .unwrap();
    let input = FinishRoundInput {
        round: round_ref(task, &project, 1),
        round_status: RoundStatus::Complete,
        task_status: TaskStatus::AwaitingReview,
        response_message_id: None,
        response: None,
        error_code: None,
        result_json: None,
    };
    install_trigger(
        &storage,
        "CREATE TRIGGER reject_checkpoint BEFORE UPDATE OF checkpoint_json ON rounds BEGIN SELECT RAISE(ABORT,'test'); END;",
    );
    assert!(
        storage
            .finish_round_with_checkpoint(input.clone(), Some(&first))
            .is_err()
    );
    assert_eq!(round_state(&storage, task, 1).status, "observing");
    assert!(
        storage
            .get_round_checkpoint(&round_ref(task, &project, 1))
            .unwrap()
            .is_none()
    );
    storage
        .connection()
        .execute_batch("DROP TRIGGER reject_checkpoint")
        .unwrap();
    storage
        .finish_round_with_checkpoint(input, Some(&first))
        .unwrap();
    assert_eq!(
        storage
            .get_round_checkpoint(&round_ref(task, &project, 1))
            .unwrap()
            .unwrap(),
        first
    );
    let revision = bridge_worker::validate_revision_findings("fix", None, &root, &[], &[]).unwrap();
    revision
        .create_round(&mut storage, round_ref(task, &project, 2), "rev".to_owned())
        .unwrap();
    std::fs::write(root.join("a"), "round two").unwrap();
    let second = bridge_worker::checkpoint::build_round_checkpoint(
        &storage,
        &round_ref(task, &project, 2),
        &root,
        &[],
        None,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&second).unwrap()["repositories"][0]["diff_stat"]["counts"],
        serde_json::json!({"added":0,"modified":1,"deleted":0,"renamed":0})
    );
    assert!(
        bridge_worker::checkpoint::build_round_checkpoint(
            &storage,
            &round_ref(task, &project, 3),
            &root,
            &[],
            None
        )
        .is_none()
    );
    storage
        .prepare_round(round_ref(task, &project, 2), "msg_checkpoint".to_owned())
        .unwrap();
    storage
        .mark_round_sent(round_ref(task, &project, 2))
        .unwrap();
    storage
        .mark_round_observing(round_ref(task, &project, 2))
        .unwrap();
    let input = FinishRoundInput {
        round: round_ref(task, &project, 2),
        round_status: RoundStatus::Complete,
        task_status: TaskStatus::AwaitingReview,
        response_message_id: None,
        response: None,
        error_code: None,
        result_json: None,
    };
    let outcome = bridge_worker::checkpoint::finish_round_with_diagnostics(
        &mut storage,
        input,
        &[],
        &root,
        &[],
        None,
    )
    .unwrap();
    assert_eq!(outcome.round.result_json.unwrap()["usage"]["input"], 0);
    assert_eq!(
        storage
            .get_round_checkpoint(&round_ref(task, &project, 2))
            .unwrap()
            .unwrap(),
        second
    );
    storage
        .connection()
        .execute(
            "UPDATE rounds SET checkpoint_json='{}' WHERE round_number=1",
            [],
        )
        .unwrap();
    assert!(
        bridge_worker::checkpoint::build_round_checkpoint(
            &storage,
            &round_ref(task, &project, 2),
            &root,
            &[],
            None
        )
        .is_none()
    );
    assert!(
        storage
            .get_round_checkpoint(&round_ref(task, &crate::project("foreign"), 1))
            .unwrap()
            .is_none()
    );
}

#[test]
fn checkpoint_bounds_topology_and_unavailable_repository() {
    let dir = TempDir::new("checkpoint-bounds");
    let root = checkpoint_repo(&dir);
    let baseline = serde_json::from_value(checkpoint_baseline(&root)["manifest"].clone()).unwrap();
    let specs = vec![(root.clone(), baseline)];
    let previous = bridge_worker::checkpoint::capture_checkpoint(&specs, None).unwrap();
    assert!(
        bridge_worker::checkpoint::capture_checkpoint(
            &[
                (root.clone(), std::collections::BTreeMap::new()),
                (root.clone(), std::collections::BTreeMap::new())
            ],
            Some(&previous)
        )
        .is_none()
    );
    let missing = dir.path().join("missing");
    let unavailable = bridge_worker::checkpoint::capture_checkpoint(
        &[(missing, std::collections::BTreeMap::new())],
        None,
    )
    .unwrap();
    assert!(!unavailable.repositories[0].available);
    assert!(bridge_worker::checkpoint::capture_checkpoint(&specs, Some(&unavailable)).is_none());
    for index in 0..2001 {
        std::fs::write(root.join(format!("new-{index}")), "x").unwrap();
    }
    assert!(bridge_worker::checkpoint::capture_checkpoint(&specs, Some(&previous)).is_none());
}

#[cfg(unix)]
#[test]
fn checkpoint_symlinks_and_exec_bit_are_content_free() {
    let dir = TempDir::new("checkpoint-symlinks");
    let root = checkpoint_repo(&dir);
    std::os::unix::fs::symlink("missing", root.join("link")).unwrap();
    let state = bridge_git::checkpoint::worktree_state(&root).unwrap();
    assert_eq!(state["files"]["link"]["kind"], "symlink");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let changed = bridge_git::checkpoint::worktree_state(&root).unwrap();
    assert_ne!(
        state["files"]["a"]["digest"],
        changed["files"]["a"]["digest"]
    );
    assert!(!changed.to_string().contains("before"));
}

// 7.18: exercise submission -> frozen storage -> both worker dispatch modes.
fn seed_profile_task(
    dir: &TempDir,
    selected: Option<serde_json::Value>,
    extra_config: &str,
) -> (
    PathBuf,
    bridge_storage::RustStateLayout,
    StorageConnection,
    TaskId,
    ProjectId,
) {
    let workspace = dir.mkdir("ws");
    let (layout, mut storage) = open_storage(dir, "proj-1");
    let path = dir.path().join("submit-projects.toml");
    std::fs::write(&path, format!(
        "[projects.proj-1]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4999\"\npassword_file=\"unused.password\"\nmax_rounds=3\n{extra_config}",
        serde_json::json!(workspace.to_str().unwrap())
    )).unwrap();
    let config = load_config(&path).unwrap();
    let project = config.project("proj-1").unwrap();
    let task = task_id();
    bridge_submission::submit_task_with_profile(
        &mut storage,
        project,
        bridge_submission::ProfileSubmissionInput {
            task: CreateTaskInput {
                task_id: task,
                project_id: project.id().clone(),
                workspace: workspace.to_str().unwrap().into(),
                task: "original {profile_block}".into(),
                request_id: "profile-request".into(),
                payload_hash: "replaced".into(),
                base_head: None,
                allowed_paths: vec!["src/".into()],
                test_commands: vec!["cargo test".into()],
                snapshot: None,
            },
            profile: selected,
            allow_dirty: false,
            allow_commit: false,
            budget: None,
            initial_status: TaskStatus::Implementing,
        },
    )
    .unwrap();
    (workspace, layout, storage, task, project.id().clone())
}

#[test]
fn frozen_profile_model_and_overlay_survive_config_changes_in_both_modes() {
    for revision in [false, true] {
        for (extra, selected, expected, overlay) in [
            (
                "opencode_model=\"old/project/model\"\n",
                None,
                Some(("old", "project/model")),
                "",
            ),
            ("", None, None, ""),
            (
                "opencode_model=\"old/project\"\n[projects.proj-1.profiles.custom]\npurpose=\"Custom\"\ninstructions=\"Проверь {task_id}.\"\nmodel=\"frozen/model/sub\"\n",
                Some(serde_json::json!("custom")),
                Some(("frozen", "model/sub")),
                "Проверь {task_id}.",
            ),
            (
                "opencode_model=\"old/project\"\n",
                Some(serde_json::json!("test-writer")),
                Some(("old", "project")),
                "",
            ),
        ] {
            let dir = TempDir::new("pinned-profile-model");
            let (workspace, layout, mut storage, task, project) =
                seed_profile_task(&dir, selected, extra);
            let saved = storage.get_task_profile(task, &project).unwrap().unwrap();
            let number = if revision {
                seed_revision_round(&mut storage, task, &project, Some("fix"));
                set_structured(
                    &storage,
                    task,
                    &serde_json::json!([structured_item("src/a.rs")]).to_string(),
                );
                2
            } else {
                1
            };
            let title = round_session_title(task, number);
            let directory = workspace.to_str().unwrap().to_owned();
            let (port, server) =
                spawn_server(
                    move |request, _| match (request.method.as_str(), request.path()) {
                        ("GET", "/session") => ok_json(&serde_json::json!([session_json(
                            Some("ses_frozen"),
                            Some(&title),
                            Some(&directory)
                        )])),
                        ("POST", path) if path.ends_with("/prompt_async") => no_content(),
                        _ => status(500, b"unexpected"),
                    },
                );
            let client = build_client_with(port, &workspace, Some("current/changed"));
            let reference = round_ref(task, &project, number);
            if revision {
                dispatch_revision_round(&client, &layout, reference).unwrap();
            } else {
                dispatch_initial_round(&client, &layout, reference).unwrap();
            }
            let body = server.prompt_posts()[0].body_json();
            if let Some((provider, model)) = expected {
                assert_eq!(
                    body["model"],
                    serde_json::json!({"providerID":provider,"modelID":model})
                );
            } else {
                assert!(body.get("model").is_none());
            }
            let text = body["parts"][0]["text"].as_str().unwrap();
            let persisted = storage.get_task(task).unwrap().unwrap();
            let expected_text = if revision {
                bridge_worker::revision_prompt_with_profile(
                    &persisted,
                    &workspace,
                    "fix\n\nСтруктурированные замечания ревью:\n1. [error] src/a.rs:2 (BUG-1) fix the operator",
                    2,
                    Some(&saved),
                )
            } else {
                bridge_worker::initial_prompt_with_profile(&persisted, &workspace, Some(&saved))
            };
            assert_eq!(text, expected_text);
            assert!(text.contains(overlay));
            assert!(text.contains("Меняй только согласованные allowed_paths: src/."));
            assert!(text.contains("Не выполняй git add, commit"));
            assert!(text.contains("авторитетно повторит ровно согласованные test_commands"));
            assert_eq!(
                storage.get_task_profile(task, &project).unwrap(),
                Some(saved)
            );
            assert_eq!(round_state(&storage, task, number).status, "observing");
            assert_eq!(server.prompt_posts().len(), 1);
        }
    }
}

#[test]
fn profile_corruption_fails_terminally_before_session_or_prompt_http() {
    for revision in [false, true] {
        for (sql, kind) in [
            (
                "UPDATE tasks SET profile_json=NULL",
                DispatchErrorKind::ProfileSnapshotMissing,
            ),
            (
                "UPDATE tasks SET profile_json=''",
                DispatchErrorKind::ProfileSnapshotMissing,
            ),
            (
                "UPDATE tasks SET profile_json='{}'",
                DispatchErrorKind::ProfileSnapshotCorrupt,
            ),
            (
                "UPDATE tasks SET profile_json=x'ff'",
                DispatchErrorKind::ProfileSnapshotCorrupt,
            ),
            (
                "UPDATE tasks SET profile_json='null'",
                DispatchErrorKind::ProfileSnapshotCorrupt,
            ),
            (
                "UPDATE tasks SET profile_json=json_set(profile_json,'$.model','invalid-model')",
                DispatchErrorKind::ProfileSnapshotCorrupt,
            ),
            (
                "UPDATE tasks SET profile_json=json_set(profile_json,'$.source','unknown')",
                DispatchErrorKind::ProfileSnapshotCorrupt,
            ),
            (
                "UPDATE tasks SET profile='test-writer'",
                DispatchErrorKind::ProfileSnapshotCorrupt,
            ),
            (
                "UPDATE tasks SET profile_hash=NULL",
                DispatchErrorKind::ProfileSnapshotCorrupt,
            ),
            (
                "UPDATE tasks SET profile_hash='bad'",
                DispatchErrorKind::ProfileSnapshotCorrupt,
            ),
            (
                "UPDATE tasks SET profile_source='argument'",
                DispatchErrorKind::ProfileSnapshotCorrupt,
            ),
            (
                "UPDATE tasks SET profile_source=NULL",
                DispatchErrorKind::ProfileSnapshotCorrupt,
            ),
        ] {
            let dir = TempDir::new("profile-corrupt");
            let (workspace, layout, mut storage, task, project) = seed_profile_task(&dir, None, "");
            let number = if revision {
                seed_revision_round(&mut storage, task, &project, Some("fix"));
                2
            } else {
                1
            };
            storage.connection().execute_batch(sql).unwrap();
            let (port, server) = spawn_server(|_, _| status(500, b"unexpected"));
            let client = build_client(port, &workspace);
            let reference = round_ref(task, &project, number);
            let error = if revision {
                dispatch_revision_round(&client, &layout, reference)
            } else {
                dispatch_initial_round(&client, &layout, reference)
            }
            .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(server.request_count(), 0);
            assert_eq!(round_state(&storage, task, number).status, "failed");
            assert_eq!(round_state(&storage, task, number).attempted, 0);
            assert_eq!(round_state(&storage, task, number).outbound, None);
            assert_eq!(
                storage.get_task(task).unwrap().unwrap().status,
                TaskStatus::Failed
            );
            let (code, result): (String, String) = storage
                .connection()
                .query_row(
                    "SELECT error_code,result_json FROM rounds WHERE round_number=?1",
                    [number],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(code, kind.as_str());
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&result).unwrap(),
                serde_json::json!({"error":kind.as_str()})
            );
            assert!(!format!("{error:?} {error}").contains("original"));
        }
    }
}

#[test]
fn profile_is_revalidated_after_session_resolution() {
    for revision in [false, true] {
        let dir = TempDir::new("profile-session-race");
        let (workspace, layout, mut storage, task, project) = seed_profile_task(&dir, None, "");
        let number = if revision {
            seed_revision_round(&mut storage, task, &project, Some("fix"));
            2
        } else {
            1
        };
        let database = layout.database();
        let title = round_session_title(task, number);
        let directory = workspace.to_str().unwrap().to_owned();
        let (port, server) =
            spawn_server(
                move |request, _| match (request.method.as_str(), request.path()) {
                    ("GET", "/session") => {
                        connect(&database)
                            .unwrap()
                            .connection()
                            .execute_batch("UPDATE tasks SET profile_hash='bad'")
                            .unwrap();
                        ok_json(&serde_json::json!([]))
                    }
                    ("POST", "/session") => ok_json(&session_json(
                        Some("ses_profile"),
                        Some(&title),
                        Some(&directory),
                    )),
                    _ => status(500, b"unexpected"),
                },
            );
        let client = build_client(port, &workspace);
        let reference = round_ref(task, &project, number);
        let error = if revision {
            dispatch_revision_round(&client, &layout, reference)
        } else {
            dispatch_initial_round(&client, &layout, reference)
        }
        .unwrap_err();
        assert_eq!(error.kind(), DispatchErrorKind::ProfileSnapshotCorrupt);
        assert_eq!(server.prompt_posts().len(), 0);
        assert_eq!(server.request_count(), 2);
        assert_eq!(round_state(&storage, task, number).status, "failed");
        assert_eq!(round_state(&storage, task, number).attempted, 0);
    }
}

#[test]
fn profile_failure_is_atomic_and_preserves_close_priority() {
    use bridge_storage::profiles::ProfileReadError;
    let dir = TempDir::new("profile-finish-atomic");
    let (_, _, mut storage, task, project) = seed_profile_task(&dir, None, "");
    let reference = round_ref(task, &project, 1);
    install_trigger(
        &storage,
        "CREATE TRIGGER reject_profile_finish BEFORE INSERT ON events WHEN NEW.kind='failed' BEGIN SELECT RAISE(ABORT,'fixture'); END;",
    );
    assert!(
        storage
            .fail_profile_snapshot(reference.clone(), ProfileReadError::CorruptSnapshot)
            .is_err()
    );
    assert_eq!(round_state(&storage, task, 1).status, "pending");
    assert_eq!(
        storage.get_task(task).unwrap().unwrap().status,
        TaskStatus::Implementing
    );
    storage
        .connection()
        .execute_batch("DROP TRIGGER reject_profile_finish")
        .unwrap();
    storage.request_task_close(task, "test close").unwrap();
    let outcome = storage
        .fail_profile_snapshot(reference.clone(), ProfileReadError::CorruptSnapshot)
        .unwrap();
    assert_eq!(outcome.task.status, TaskStatus::Closed);
    assert_eq!(outcome.round.status, RoundStatus::Failed);
    assert!(
        storage
            .fail_profile_snapshot(reference, ProfileReadError::CorruptSnapshot)
            .is_err()
    );
}

#[test]
fn profile_failure_cannot_terminate_an_attempted_or_cross_project_round() {
    use bridge_storage::profiles::ProfileReadError;
    let dir = TempDir::new("profile-failure-fences");
    let (_, _, mut storage, task, project) = seed_profile_task(&dir, None, "");
    let reference = round_ref(task, &project, 1);
    assert!(
        storage
            .fail_profile_snapshot(
                round_ref(task, &self::project("other"), 1),
                ProfileReadError::CorruptSnapshot
            )
            .is_err()
    );
    assert!(
        storage
            .fail_profile_snapshot(reference.clone(), ProfileReadError::Database)
            .is_err()
    );
    assert_eq!(round_state(&storage, task, 1).status, "pending");
    storage
        .prepare_round(reference.clone(), "msg_attempted".into())
        .unwrap();
    storage.mark_round_sent(reference.clone()).unwrap();
    assert!(
        storage
            .fail_profile_snapshot(reference, ProfileReadError::MissingSnapshot)
            .is_err()
    );
    assert_eq!(round_state(&storage, task, 1).status, "sent");
    assert_eq!(round_state(&storage, task, 1).attempted, 1);
    assert_eq!(
        storage.get_task(task).unwrap().unwrap().status,
        TaskStatus::Implementing
    );
}
