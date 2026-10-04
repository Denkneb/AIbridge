use bridge_config::{ProjectEntry, load_config_with_state_root};
use bridge_domain::{RoundStatus, TaskId, TaskStatus};
use bridge_mcp::{McpServer, protocol::Protocol};
use bridge_storage::{FinishRoundInput, RoundRef, RustStateLayout};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
struct Fixture {
    root: PathBuf,
    project: ProjectEntry,
    layout: RustStateLayout,
    activity: Arc<Mutex<Value>>,
    calls: Arc<Mutex<Vec<String>>>,
    spawns: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    mock: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(extra: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-mcp-tasks-{}-{}",
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
        fs::write(root.join("password"), "fixture-secret").unwrap();
        fs::set_permissions(root.join("password"), fs::Permissions::from_mode(0o600)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let activity = Arc::new(Mutex::new(json!({})));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (a, c, end, directory) = (
            activity.clone(),
            calls.clone(),
            stop.clone(),
            workspace.clone(),
        );
        let mock = thread::spawn(move || {
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
                        let first = text.lines().next().unwrap_or("");
                        c.lock().unwrap().push(first.into());
                        let path = first
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("")
                            .split('?')
                            .next()
                            .unwrap();
                        let value = match path {
                            "/global/health" => json!({"healthy":true}),
                            "/path" => json!({"directory":directory}),
                            "/session/status" => a.lock().unwrap().clone(),
                            "/permission" | "/question" => json!([]),
                            _ => json!({}),
                        };
                        let body = value.to_string();
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        let config = root.join("projects.toml");
        fs::write(&config,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:{port}\"\npassword_file={}\nmax_rounds=3\n{extra}\n",json!(workspace),json!(root.join("password")))).unwrap();
        let project = load_config_with_state_root(&config, &root.join("state"))
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
        let layout = RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
        Self {
            root,
            project,
            layout,
            activity,
            calls,
            spawns: Arc::new(AtomicUsize::new(0)),
            fail: Arc::new(AtomicBool::new(false)),
            stop,
            mock: Some(mock),
        }
    }
    fn server(&self) -> McpServer {
        let (spawns, fail) = (self.spawns.clone(), self.fail.clone());
        McpServer::open(self.project.clone(), self.layout.clone())
            .unwrap()
            .with_workers(
                Arc::new(move |_| {
                    spawns.fetch_add(1, Ordering::Relaxed);
                    if fail.load(Ordering::Relaxed) {
                        Err(())
                    } else {
                        Ok(())
                    }
                }),
                vec![self.project.clone()],
            )
            .unwrap()
    }
    fn submit(&self, server: &McpServer) -> Value {
        call(server, "submit_task", input())
    }
    fn review(&self, id: TaskId, session: bool) {
        let mut s = self.layout.open().unwrap();
        let r = RoundRef {
            task_id: id,
            project_id: self.project.id().clone(),
            round_number: 1,
        };
        s.prepare_round(r.clone(), "outbound".into()).unwrap();
        if session {
            s.bind_round_session(r.clone(), "session".into()).unwrap();
        }
        s.mark_round_sent(r.clone()).unwrap();
        s.mark_round_observing(r.clone()).unwrap();
        s.finish_round(FinishRoundInput {
            round: r,
            round_status: RoundStatus::Complete,
            task_status: TaskStatus::AwaitingReview,
            response_message_id: None,
            response: Some("review response".into()),
            error_code: None,
            result_json: Some(json!({"changed_paths":["src/a"],"verification":{"passed":false}})),
        })
        .unwrap();
    }
    fn count(&self) -> i64 {
        self.layout
            .open()
            .unwrap()
            .connection()
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.mock.take().unwrap().join().unwrap();
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn input() -> Value {
    json!({"request_id":"submit","task":"implement change","allowed_paths":["src/"],"test_commands":[]})
}
fn response(server: &McpServer, name: &str, args: Value) -> Value {
    Protocol::stateless_http().handle(server,json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).unwrap()
}
fn call(server: &McpServer, name: &str, args: Value) -> Value {
    response(server, name, args)["result"]["structuredContent"].clone()
}
fn task_id(value: &Value) -> TaskId {
    value["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{value}"))
        .parse()
        .unwrap()
}
#[test]
fn delegated_schema_and_notifications_are_strict_and_side_effect_free() {
    let f = Fixture::new("");
    let server = f.server();
    let mut p = Protocol::stateless_http();
    let list = p
        .handle(
            &server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .unwrap();
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 6);
    assert!(p.handle(&server,json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"submit_task","arguments":input()}})).is_none());
    assert_eq!(f.count(), 0);
    let mut args = input();
    args["unknown"] = json!(true);
    assert_eq!(
        response(&server, "submit_task", args)["error"]["code"],
        -32602
    );
    assert_eq!(f.count(), 0);
}
#[test]
fn submit_replay_uses_raw_hash_and_never_reprobes_or_respawns() {
    let f = Fixture::new("");
    let server = f.server();
    let first = f.submit(&server);
    let id = task_id(&first);
    assert_eq!(first["status"], "implementing");
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    f.calls.lock().unwrap().clear();
    fs::write(f.project.workspace().join("outside"), "dirty").unwrap();
    let replay = f.submit(&server);
    assert_eq!(task_id(&replay), id);
    assert!(f.calls.lock().unwrap().is_empty());
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    let mut args = input();
    args["task"] = json!("different");
    assert_eq!(
        call(&server, "submit_task", args)["error"],
        "request_conflict"
    );
    assert_eq!(f.count(), 1);
}
#[test]
fn validation_refusals_never_persist_or_spawn() {
    let f = Fixture::new("");
    let server = f.server();
    for (key, value, error) in [
        (
            "task",
            json!("-----BEGIN PRIVATE KEY-----\nsecret"),
            "suspected_secrets",
        ),
        (
            "workflow_id",
            json!("workflow"),
            "workflow_metadata_unavailable",
        ),
        (
            "test_commands",
            json!(["git push"]),
            "invalid_test_commands",
        ),
        ("budget", json!({"invalid":2}), "invalid_budget"),
        ("profile", json!("missing"), "unknown_profile"),
        ("allow_commit", json!("true"), "invalid_boolean"),
    ] {
        let mut args = input();
        args[key] = value;
        assert_eq!(call(&server, "submit_task", args)["error"], error, "{key}");
    }
    assert_eq!(f.count(), 0);
    assert_eq!(f.spawns.load(Ordering::Relaxed), 0);
}
#[test]
fn dirty_files_must_be_inside_scope_even_with_allow_dirty() {
    let f = Fixture::new("");
    let server = f.server();
    fs::write(f.project.workspace().join("outside"), "dirty").unwrap();
    let mut args = input();
    args["allow_dirty"] = json!(true);
    assert_eq!(
        call(&server, "submit_task", args)["error"],
        "dirty_paths_outside_scope"
    );
    assert_eq!(f.count(), 0);
    fs::remove_file(f.project.workspace().join("outside")).unwrap();
    fs::create_dir_all(f.project.workspace().join("src")).unwrap();
    fs::write(f.project.workspace().join("src/a"), "dirty").unwrap();
    let mut args = input();
    args["allow_dirty"] = json!(true);
    assert_eq!(call(&server, "submit_task", args)["status"], "implementing");
}
#[test]
fn failed_spawn_releases_only_its_lease_and_status_can_retry() {
    let f = Fixture::new("");
    let server = f.server();
    f.fail.store(true, Ordering::Relaxed);
    assert_eq!(f.submit(&server)["error"], "worker_spawn_failed");
    assert_eq!(f.count(), 1);
    let task = f
        .layout
        .open()
        .unwrap()
        .get_active_task(f.project.id())
        .unwrap()
        .unwrap();
    f.fail.store(false, Ordering::Relaxed);
    assert_eq!(
        call(
            &server,
            "task_status",
            json!({"task_id":task.task_id.to_string(),"wait_seconds":0})
        )["status"],
        "implementing"
    );
    assert_eq!(f.spawns.load(Ordering::Relaxed), 2);
    call(
        &server,
        "task_status",
        json!({"task_id":task.task_id.to_string(),"wait_seconds":0}),
    );
    assert_eq!(f.spawns.load(Ordering::Relaxed), 2);
}
#[test]
fn manual_accept_is_idempotent_and_checks_only_bound_session() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    f.review(id, true);
    *f.activity.lock().unwrap() = json!({"foreign":{"type":"busy"}});
    let args = json!({"task_id":id.to_string()});
    assert_eq!(
        call(&server, "accept_task", args.clone())["status"],
        "accepted"
    );
    f.calls.lock().unwrap().clear();
    assert_eq!(call(&server, "accept_task", args)["status"], "accepted");
    assert!(f.calls.lock().unwrap().is_empty());
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
}
#[test]
fn live_or_malformed_activity_refuses_accept_revision_and_close() {
    for activity in [json!({"session":{"type":"busy"}}), json!([])] {
        let f = Fixture::new("");
        let server = f.server();
        let id = task_id(&f.submit(&server));
        f.review(id, true);
        *f.activity.lock().unwrap() = activity;
        for (name, args) in [
            ("accept_task", json!({"task_id":id.to_string()})),
            (
                "request_changes",
                json!({"task_id":id.to_string(),"request_id":"revise","findings":"fix"}),
            ),
            (
                "close_task",
                json!({"task_id":id.to_string(),"reason":"stop"}),
            ),
        ] {
            assert!(
                response(&server, name, args)["result"]["isError"]
                    .as_bool()
                    .unwrap()
            );
        }
        let task = f.layout.open().unwrap().get_task(id).unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::AwaitingReview);
        assert!(task.close_requested_at.is_none());
        assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    }
}
#[test]
fn revision_replay_is_idempotent_and_review_status_keeps_verification() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    f.review(id, true);
    let status = call(
        &server,
        "task_status",
        json!({"task_id":id.to_string(),"wait_seconds":0,"verbose":true}),
    );
    assert_eq!(status["verification"]["passed"], false);
    assert_eq!(status["response"], "review response");
    let args = json!({"task_id":id.to_string(),"request_id":"revise","findings":"fix"});
    let revision = call(&server, "request_changes", args.clone());
    assert_eq!(revision["status"], "revising");
    assert_eq!(revision["round_number"], 2);
    assert_eq!(call(&server, "request_changes", args)["round_number"], 2);
    assert_eq!(f.spawns.load(Ordering::Relaxed), 2);
}
#[test]
fn close_while_worker_fenced_is_deferred_then_status_finishes() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    let fence = match bridge_worker::WorkerLock::try_acquire(&f.layout).unwrap() {
        bridge_worker::WorkerLockOutcome::Acquired(g) => g,
        _ => panic!("busy"),
    };
    assert_eq!(
        call(
            &server,
            "close_task",
            json!({"task_id":id.to_string(),"reason":"stop"})
        )["status"],
        "close_requested"
    );
    drop(fence);
    assert_eq!(
        call(
            &server,
            "task_status",
            json!({"task_id":id.to_string(),"wait_seconds":0})
        )["status"],
        "closed"
    );
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
}
#[test]
fn frozen_on_accept_delivery_is_explicitly_unavailable() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    f.review(id, false);
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute("UPDATE tasks SET delivery_mode='on_accept'", [])
        .unwrap();
    assert_eq!(
        call(&server, "accept_task", json!({"task_id":id.to_string()}))["error"],
        "on_accept_delivery_unavailable"
    );
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::AwaitingReview
    );
}

#[test]
fn explicit_needs_user_recovery_claims_once_without_creating_round_or_prompt() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    let mut s = f.layout.open().unwrap();
    let r = RoundRef {
        task_id: id,
        project_id: f.project.id().clone(),
        round_number: 1,
    };
    s.prepare_round(r.clone(), "outbound".into()).unwrap();
    s.bind_round_session(r.clone(), "session".into()).unwrap();
    s.mark_round_sent(r.clone()).unwrap();
    s.mark_round_observing(r.clone()).unwrap();
    s.finish_round(FinishRoundInput {
        round: r,
        round_status: RoundStatus::NeedsUser,
        task_status: TaskStatus::NeedsUser,
        response_message_id: None,
        response: None,
        error_code: Some("permission_required".into()),
        result_json: None,
    })
    .unwrap();
    f.calls.lock().unwrap().clear();
    assert_eq!(
        call(&server, "task_status", json!({"wait_seconds":0}))["status"],
        "needs_user"
    );
    assert!(f.calls.lock().unwrap().is_empty());
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    let args = json!({"task_id":id.to_string(),"wait_seconds":0});
    assert_eq!(
        call(&server, "task_status", args.clone())["status"],
        "implementing"
    );
    assert_eq!(call(&server, "task_status", args)["round_number"], 1);
    assert_eq!(f.spawns.load(Ordering::Relaxed), 2);
    assert!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.starts_with("GET "))
    );
}
#[test]
fn pending_worktree_submit_does_not_probe_static_server_and_close_cleans_row() {
    let f = Fixture::new("execution_mode=\"worktree\"");
    fs::write(f.project.workspace().join("seed"), "seed").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "seed"])
            .current_dir(f.project.workspace())
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-qm",
                "seed"
            ])
            .current_dir(f.project.workspace())
            .status()
            .unwrap()
            .success()
    );
    let server = f.server();
    let result = f.submit(&server);
    let id = task_id(&result);
    assert_eq!(result["execution_mode"], "worktree");
    assert_eq!(result["worktree"]["status"], "pending");
    assert!(f.calls.lock().unwrap().is_empty());
    assert_eq!(
        call(
            &server,
            "close_task",
            json!({"task_id":id.to_string(),"reason":"stop"})
        )["status"],
        "closed"
    );
    assert!(f.calls.lock().unwrap().is_empty());
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_worktree(id, f.project.id())
            .unwrap()
            .unwrap()
            .status
            .as_str(),
        "removed"
    );
}
#[test]
fn concurrent_status_calls_do_not_duplicate_spawn_lease() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE rounds SET worker_started_at=NULL,worker_deadline_at=NULL",
            [],
        )
        .unwrap();
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..8 {
            let server = &server;
            handles.push(scope.spawn(move || {
                call(
                    server,
                    "task_status",
                    json!({"task_id":id.to_string(),"wait_seconds":0}),
                )
            }));
        }
        for h in handles {
            assert_eq!(h.join().unwrap()["status"], "implementing");
        }
    });
    assert_eq!(f.spawns.load(Ordering::Relaxed), 2);
}
#[test]
fn raw_path_alias_replay_and_corrupt_binding_fail_closed() {
    let f = Fixture::new("");
    let server = f.server();
    let mut args = input();
    args["allowed_paths"] = json!(["./src/"]);
    let id = task_id(&call(&server, "submit_task", args.clone()));
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(id)
            .unwrap()
            .unwrap()
            .allowed_paths,
        vec!["src/"]
    );
    assert_eq!(task_id(&call(&server, "submit_task", args)), id);
    assert_eq!(
        call(&server, "submit_task", input())["error"],
        "request_conflict"
    );
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute("UPDATE tasks SET workspace='/different-root'", [])
        .unwrap();
    assert_eq!(
        call(
            &server,
            "task_status",
            json!({"task_id":id.to_string(),"wait_seconds":0})
        )["error"],
        "task_binding_mismatch"
    );
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
}

#[test]
fn revision_budget_override_does_not_bypass_structured_scope_validation() {
    let f = Fixture::new("");
    let server = f.server();
    let mut args = input();
    args["budget"] = json!({"limits":{"input":1}});
    let id = task_id(&call(&server, "submit_task", args));
    f.review(id, false);
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE rounds SET result_json=?1",
            [json!({"usage":{"input":2}}).to_string()],
        )
        .unwrap();
    let mut revision = json!({"task_id":id.to_string(),"request_id":"revise","findings":"fix"});
    assert_eq!(
        call(&server, "request_changes", revision.clone())["error"],
        "budget_exhausted"
    );
    revision["allow_budget_override"] = json!(true);
    revision["structured_findings"] =
        json!([{"severity":"error","path":"outside","code":"review","message":"fix"}]);
    assert_eq!(
        call(&server, "request_changes", revision.clone())["error"],
        "invalid_structured_findings"
    );
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(id)
            .unwrap()
            .unwrap()
            .revision_count,
        0
    );
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    revision["structured_findings"] = Value::Null;
    assert_eq!(
        call(&server, "request_changes", revision)["round_number"],
        2
    );
    assert_eq!(f.spawns.load(Ordering::Relaxed), 2);
}
