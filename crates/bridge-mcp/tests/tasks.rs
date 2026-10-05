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
    blockers: Arc<Mutex<Value>>,
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
        let blockers = Arc::new(Mutex::new(json!({"permissions":[],"questions":[]})));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (a, c, end, directory) = (
            activity.clone(),
            calls.clone(),
            stop.clone(),
            workspace.clone(),
        );
        let b = blockers.clone();
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
                            "/global/health" => {
                                json!({"healthy":a.lock().unwrap().get("_health").and_then(Value::as_bool).unwrap_or(true)})
                            }
                            "/path" => json!({"directory":directory}),
                            "/session/status" => a.lock().unwrap().clone(),
                            "/permission" => b.lock().unwrap()["permissions"].clone(),
                            "/question" => b.lock().unwrap()["questions"].clone(),
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
            blockers,
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

fn expire_spawn(f: &Fixture) {
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE rounds SET worker_started_at='2000-01-01T00:00:00Z',worker_deadline_at=NULL",
            [],
        )
        .unwrap();
}
fn park(f: &Fixture, id: TaskId) {
    let mut s = f.layout.open().unwrap();
    let r = RoundRef {
        task_id: id,
        project_id: f.project.id().clone(),
        round_number: 1,
    };
    s.prepare_round(r.clone(), "saved outbound".into()).unwrap();
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
}
#[test]
fn startup_resumes_saved_attempt_once_and_preserves_outbound_and_baseline() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    let mut s = f.layout.open().unwrap();
    let r = RoundRef {
        task_id: id,
        project_id: f.project.id().clone(),
        round_number: 1,
    };
    s.prepare_round(r.clone(), "saved outbound".into()).unwrap();
    s.bind_round_session(r.clone(), "session".into()).unwrap();
    s.mark_round_sent(r.clone()).unwrap();
    let before = s.get_task(id).unwrap().unwrap().snapshot;
    expire_spawn(&f);
    drop(server);
    let server = f.server();
    thread::scope(|scope| {
        let a = scope.spawn(|| server.recover_startup());
        let b = scope.spawn(|| server.recover_startup());
        a.join().unwrap().unwrap();
        b.join().unwrap().unwrap();
    });
    server.recover_startup().unwrap();
    assert_eq!(f.spawns.load(Ordering::Relaxed), 2);
    assert_eq!(s.get_task(id).unwrap().unwrap().snapshot, before);
    let row = s.connection().query_row("SELECT attempted,outbound_message_id,session_id,round_number FROM rounds WHERE task_id=?1", [id.to_string()], |r| Ok((r.get::<_,bool>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,u32>(3)?))).unwrap();
    assert_eq!(row, (true, "saved outbound".into(), "session".into(), 1));
}
#[test]
fn startup_respects_live_worker_and_spawn_lease_then_retries_failed_spawn() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    server.recover_startup().unwrap();
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    expire_spawn(&f);
    let guard = match bridge_worker::WorkerLock::try_acquire_task(&f.layout, id).unwrap() {
        bridge_worker::WorkerLockOutcome::Acquired(g) => g,
        _ => panic!(),
    };
    server.recover_startup().unwrap();
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    drop(guard);
    f.fail.store(true, Ordering::Relaxed);
    assert!(server.recover_startup().is_err());
    let stamp: Option<String> = f
        .layout
        .open()
        .unwrap()
        .connection()
        .query_row("SELECT worker_started_at FROM rounds", [], |r| r.get(0))
        .unwrap();
    assert!(stamp.is_none());
    f.fail.store(false, Ordering::Relaxed);
    server.recover_startup().unwrap();
    assert_eq!(f.spawns.load(Ordering::Relaxed), 3);
}
#[test]
fn startup_background_needs_user_keeps_blocker_gate_and_rolls_back_failed_spawn() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    park(&f, id);
    // Both APIs reject malformed lists; neither may silently bypass a blocker.
    for (n, blockers) in [
        json!({"permissions":[{"id":"permission", "sessionID":"session", "permission":"bash", "patterns":[], "metadata":{}, "always":[]}],"questions":[]}),
        json!({"permissions":[],"questions":[{"id":"question","sessionID":"session","questions":[]}]}),
        json!({"permissions":{},"questions":[]}),
    ].into_iter().enumerate() {
        *f.blockers.lock().unwrap() = blockers;
        let outcome = server.recover_startup();
        if n < 2 { outcome.unwrap(); } else { assert!(outcome.is_err()); }
        assert_eq!(
            f.layout
                .open()
                .unwrap()
                .get_task(id)
                .unwrap()
                .unwrap()
                .status,
            TaskStatus::NeedsUser
        );
        assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    }
    *f.blockers.lock().unwrap() = json!({"permissions":[],"questions":[]});
    f.fail.store(true, Ordering::Relaxed);
    assert!(server.recover_startup().is_err());
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::NeedsUser
    );
    f.fail.store(false, Ordering::Relaxed);
    server.recover_startup().unwrap();
    server.recover_startup().unwrap();
    assert_eq!(f.spawns.load(Ordering::Relaxed), 3);
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Implementing
    );
    assert!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.starts_with("GET "))
    );
}
#[test]
fn startup_leaves_review_failed_and_waiting_tasks_parked_and_readonly_is_inert() {
    for status in ["awaiting_review", "failed", "waiting_dependencies"] {
        let f = Fixture::new("");
        let server = f.server();
        let id = task_id(&f.submit(&server));
        f.layout
            .open()
            .unwrap()
            .connection()
            .execute("UPDATE tasks SET status=?1", [status])
            .unwrap();
        expire_spawn(&f);
        f.calls.lock().unwrap().clear();
        server.recover_startup().unwrap();
        assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
        assert!(f.calls.lock().unwrap().is_empty());
        assert_eq!(
            f.layout
                .open()
                .unwrap()
                .get_task(id)
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            status
        );
        drop(server);
        let readonly = McpServer::open(f.project.clone(), f.layout.clone()).unwrap();
        readonly.recover_startup().unwrap();
        assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    }
}
#[test]
fn startup_completes_deferred_close_and_removes_stale_reservations() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    f.layout
        .open()
        .unwrap()
        .request_task_close(id, "saved close")
        .unwrap();
    server.recover_startup().unwrap();
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Closed
    );
    assert!(
        f.layout
            .open()
            .unwrap()
            .get_active_writers(f.project.id())
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    // Simulate the crash window where a terminal task still has a reservation.
    f.layout.open().unwrap().connection().execute("INSERT INTO active_writers(task_id,project_id,scopes_json,created_at,parallel) VALUES (?1,'proj','[\"src/\"]','old',0)", [id.to_string()]).unwrap();
    server.recover_startup().unwrap();
    assert!(
        f.layout
            .open()
            .unwrap()
            .get_active_writers(f.project.id())
            .unwrap()
            .is_empty()
    );
}
#[test]
fn stdio_startup_recovery_does_not_block_handshake_and_retains_mcp_ownership() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    let f = Fixture::new("");
    let server = f.server();
    let _id = task_id(&f.submit(&server));
    expire_spawn(&f);
    drop(server);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release = Mutex::new(release_rx);
    let server = McpServer::open(f.project.clone(), f.layout.clone())
        .unwrap()
        .with_workers(
            Arc::new(move |_| {
                entered_tx.send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                Ok(())
            }),
            vec![f.project.clone()],
        )
        .unwrap();
    let (transport, mut client) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    thread::scope(|scope| {
        let h = scope.spawn(|| {
            bridge_mcp::stdio::run(
                &server,
                BufReader::new(transport.try_clone().unwrap()),
                transport,
            )
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        writeln!(client,"{}",json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}})).unwrap();
        let mut line = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["id"], 1);
        assert!(matches!(
            McpServer::open(f.project.clone(), f.layout.clone()),
            Err(bridge_mcp::McpError::Busy)
        ));
        client.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(!h.is_finished());
        assert!(matches!(
            McpServer::open(f.project.clone(), f.layout.clone()),
            Err(bridge_mcp::McpError::Busy)
        ));
        release_tx.send(()).unwrap();
        h.join().unwrap().unwrap();
    });
}

#[test]
fn startup_recovers_all_parallel_writers_even_after_one_spawn_fails() {
    let f = Fixture::new(
        "execution_mode=\"worktree\"\nallow_parallel_writers=true\nmax_active_tasks=3",
    );
    fs::write(f.project.workspace().join("seed"), "seed").unwrap();
    for args in [
        vec!["add", "seed"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "seed",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(f.project.workspace())
                .status()
                .unwrap()
                .success()
        );
    }
    let server = f.server();
    let mut ids = Vec::new();
    for n in 0..3 {
        let mut args = input();
        args["request_id"] = json!(format!("submit-{n}"));
        args["allowed_paths"] = json!([format!("src{n}/")]);
        ids.push(task_id(&call(&server, "submit_task", args)));
    }
    assert_eq!(f.spawns.load(Ordering::Relaxed), 3);
    expire_spawn(&f);
    drop(server);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let server = McpServer::open(f.project.clone(), f.layout.clone())
        .unwrap()
        .with_workers(
            Arc::new(move |r| {
                let mut seen = captured.lock().unwrap();
                seen.push(r.task_id);
                if seen.len() == 1 { Err(()) } else { Ok(()) }
            }),
            vec![f.project.clone()],
        )
        .unwrap();
    assert!(server.recover_startup().is_err());
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert!(ids.iter().all(|id| seen.lock().unwrap().contains(id)));
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_active_writers(f.project.id())
            .unwrap()
            .len(),
        3
    );
    assert!(f.calls.lock().unwrap().is_empty());
}
#[test]
fn startup_quarantines_orphan_logically_without_changing_its_files() {
    let f = Fixture::new("");
    let server = f.server();
    let orphan = f.layout.project_dir().join("worktrees/orphan");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("keep"), "untouched").unwrap();
    fs::set_permissions(&orphan, fs::Permissions::from_mode(0o750)).unwrap();
    server.recover_startup().unwrap();
    server.recover_startup().unwrap();
    let entries = f.layout.open().unwrap().list_worktree_quarantine().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].original_path, orphan.to_str().unwrap());
    assert_eq!(
        fs::read_to_string(orphan.join("keep")).unwrap(),
        "untouched"
    );
    assert_eq!(
        fs::metadata(&orphan).unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert_eq!(f.spawns.load(Ordering::Relaxed), 0);
}
#[test]
fn http_transport_runs_startup_recovery_beside_authenticated_handshake() {
    use std::net::TcpStream;
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let f = Fixture::new(&format!(
        "mcp_url=\"http://127.0.0.1:{port}/mcp\"\nmcp_token_file=\"mcp.token\""
    ));
    fs::write(f.root.join("mcp.token"), "fixture-token").unwrap();
    fs::set_permissions(f.root.join("mcp.token"), fs::Permissions::from_mode(0o600)).unwrap();
    let server = f.server();
    let _id = task_id(&f.submit(&server));
    expire_spawn(&f);
    drop(server);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release = Mutex::new(release_rx);
    let server = bridge_mcp::http::HttpServer::bind_with_workers(
        f.project.clone(),
        f.layout.clone(),
        Arc::new(move |_| {
            entered_tx.send(()).unwrap();
            release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            Ok(())
        }),
        vec![f.project.clone()],
    )
    .unwrap();
    let stop = AtomicBool::new(false);
    thread::scope(|scope| {
        struct StopOnDrop<'a>(&'a AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let h = scope.spawn(|| server.run_until(&stop));
        let _stop_on_drop = StopOnDrop(&stop);
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let body=json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}).to_string();
        write!(client,"POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer fixture-token\r\nContent-Type: application/json\r\nAccept: application/json,text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert_eq!(
            serde_json::from_str::<Value>(response.split_once("\r\n\r\n").unwrap().1).unwrap()["id"],
            1
        );
        assert!(matches!(
            McpServer::open(f.project.clone(), f.layout.clone()),
            Err(bridge_mcp::McpError::Busy)
        ));
        release_tx.send(()).unwrap();
        stop.store(true, Ordering::Release);
        h.join().unwrap().unwrap();
    });
    assert!(McpServer::open(f.project.clone(), f.layout.clone()).is_ok());
}

#[test]
fn ambiguous_delivery_recovery_probes_health_claims_once_and_never_posts() {
    let f = Fixture::new("");
    let server = f.server();
    let id = task_id(&f.submit(&server));
    park(&f, id);
    f.layout.open().unwrap().connection().execute_batch("UPDATE tasks SET status='delivery_unknown'; UPDATE rounds SET status='delivery_unknown',error_code='delivery_unknown'").unwrap();
    *f.activity.lock().unwrap() = json!({"_health":false});
    server.recover_startup().unwrap();
    assert_eq!(f.spawns.load(Ordering::Relaxed), 1);
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::DeliveryUnknown
    );
    *f.activity.lock().unwrap() = json!({});
    f.fail.store(true, Ordering::Relaxed);
    assert!(server.recover_startup().is_err());
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::DeliveryUnknown
    );
    f.fail.store(false, Ordering::Relaxed);
    thread::scope(|scope| {
        let a = scope.spawn(|| server.recover_startup());
        let b = scope.spawn(|| {
            call(
                &server,
                "task_status",
                json!({"task_id":id.to_string(),"wait_seconds":0}),
            )
        });
        a.join().unwrap().unwrap();
        b.join().unwrap();
    });
    assert_eq!(f.spawns.load(Ordering::Relaxed), 3);
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Implementing
    );
    assert!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.starts_with("GET "))
    );
}
