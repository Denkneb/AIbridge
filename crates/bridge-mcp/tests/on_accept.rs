#[path = "../../bridge-delivery/tests/common/mod.rs"]
mod common;
use bridge_domain::{RoundStatus, TaskStatus};
use bridge_mcp::{McpServer, protocol::Protocol};
use bridge_storage::{FinishRoundInput, RoundRef};
use serde_json::{Value, json};
use std::{fs, num::NonZeroU16, sync::Arc};
fn server(f: &common::Fixture) -> McpServer {
    McpServer::open(f.project.clone(), f.layout.clone())
        .unwrap()
        .with_workers(Arc::new(|_| Ok(())), vec![f.project.clone()])
        .unwrap()
}
fn call(server: &McpServer, name: &str, args: Value) -> Value {
    Protocol::stateless_http().handle(server,json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).unwrap()["result"]["structuredContent"].clone()
}
fn review(f: &common::Fixture) {
    let mut s = f.layout.open().unwrap();
    s.connection()
        .execute_batch("UPDATE tasks SET status='implementing',delivery_mode='on_accept'")
        .unwrap();
    let r = RoundRef {
        task_id: f.id,
        project_id: f.project.id().clone(),
        round_number: 1,
    };
    s.prepare_round(r.clone(), "outbound".into()).unwrap();
    s.mark_round_sent(r.clone()).unwrap();
    s.mark_round_observing(r.clone()).unwrap();
    s.finish_round(FinishRoundInput {
        round: r,
        round_status: RoundStatus::Complete,
        task_status: TaskStatus::AwaitingReview,
        response_message_id: None,
        response: None,
        error_code: None,
        result_json: None,
    })
    .unwrap();
    s.update_worktree_server(
        f.id,
        f.project.id(),
        "http://127.0.0.1:9000",
        NonZeroU16::new(9000).unwrap(),
        None,
    )
    .unwrap();
}
#[test]
fn first_on_accept_delivers_and_delivered_retry_needs_no_locks_or_server() {
    let f = common::Fixture::new();
    f.changes();
    review(&f);
    let server = server(&f);
    let result = call(&server, "accept_task", json!({"task_id":f.id.to_string()}));
    assert_eq!(result["status"], "accepted", "{result}");
    assert_eq!(result["delivery"]["state"], "delivered", "{result}");
    assert_eq!(
        fs::read(f.project.workspace().join("src/a")).unwrap(),
        b"result\xff\0"
    );
    let _admission = bridge_worker::WorkerLock::try_acquire_admission(&f.layout).unwrap();
    let repeat = call(&server, "accept_task", json!({"task_id":f.id.to_string()}));
    assert_eq!(repeat, result);
}
#[test]
fn accepted_refusal_is_visible_and_repeat_reuses_artifact_without_opencode() {
    let f = common::Fixture::new();
    f.changes();
    review(&f);
    fs::write(f.project.workspace().join("outside"), "dirty").unwrap();
    let server = server(&f);
    let result = call(&server, "accept_task", json!({"task_id":f.id.to_string()}));
    assert_eq!(result["status"], "accepted", "{result}");
    assert_eq!(result["delivery"]["state"], "refused");
    assert_eq!(result["delivery"]["code"], "dirty_main_workspace");
    assert_eq!(result["delivery"]["delivery_state"], "none");
    assert!(f.dest().join("manifest.json").is_file());
    let status = call(
        &server,
        "task_status",
        json!({"task_id":f.id.to_string(),"wait_seconds":0}),
    );
    assert_eq!(status["delivery"]["state"], "refused");
    assert_eq!(
        status["delivery"]["last_attempt"]["code"],
        "dirty_main_workspace"
    );
    assert!(!result.to_string().contains(f.root.to_str().unwrap()));
    fs::remove_file(f.project.workspace().join("outside")).unwrap();
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE worktrees SET server_port=NULL,server_endpoint=NULL",
            [],
        )
        .unwrap();
    let result = call(&server, "accept_task", json!({"task_id":f.id.to_string()}));
    assert_eq!(result["delivery"]["state"], "delivered", "{result}");
}
#[test]
fn first_accept_busy_keeps_review_and_pending_accepted_busy_reports_actual_state() {
    let f = common::Fixture::new();
    f.changes();
    review(&f);
    let server = server(&f);
    let guard = bridge_worker::WorkerLock::try_acquire_admission(&f.layout).unwrap();
    let result = call(&server, "accept_task", json!({"task_id":f.id.to_string()}));
    assert_eq!(result["error"], "admission_lock_busy");
    assert_eq!(
        f.layout
            .open()
            .unwrap()
            .get_task(f.id)
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::AwaitingReview
    );
    drop(guard);
}
#[test]
fn accepted_partial_delivery_resumes_the_existing_journal() {
    let f = common::Fixture::new();
    f.changes();
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute("UPDATE tasks SET delivery_mode='on_accept'", [])
        .unwrap();
    bridge_delivery::build(&f.layout, &f.project, f.id).unwrap();
    // Busy is a legitimate non-blocking refusal, and does not prove that the
    // injected crash boundary was reached. Require that exact failure.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let error = bridge_delivery::apply_with_fault(&f.layout, &f.project, f.id, |phase| {
            phase == "after_op:0"
        })
        .unwrap_err();
        if error.code == "project_busy" && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        assert_eq!(error.code, "simulated_crash");
        break;
    }
    let server = server(&f);
    let status = call(
        &server,
        "task_status",
        json!({"task_id":f.id.to_string(),"wait_seconds":0}),
    );
    assert_eq!(status["delivery"]["state"], "applying");
    let result = call(&server, "accept_task", json!({"task_id":f.id.to_string()}));
    assert_eq!(result["status"], "accepted");
    assert_eq!(result["delivery"]["state"], "delivered", "{result}");
}
#[test]
fn other_writer_blocks_delivery_after_accept_and_saved_policy_survives_manual_config() {
    let f = common::Fixture::new();
    f.changes();
    review(&f);
    let mut s = f.layout.open().unwrap();
    s.connection().execute("INSERT INTO active_writers(task_id,project_id,scopes_json,created_at,parallel) VALUES (?1,'proj','[\"src/\"]','stamp',1)",[f.id.to_string()]).unwrap();
    let other = uuid::Uuid::new_v4().to_string().parse().unwrap();
    s.connection()
        .execute("DELETE FROM active_writers", [])
        .unwrap();
    s.connection()
        .execute(
            "UPDATE tasks SET status='accepted' WHERE task_id=?1",
            [f.id.to_string()],
        )
        .unwrap();
    s.create_task(bridge_storage::CreateTaskInput {
        task_id: other,
        project_id: f.project.id().clone(),
        workspace: f.project.workspace().to_str().unwrap().into(),
        task: "other".into(),
        request_id: "other".into(),
        payload_hash: "hash".into(),
        base_head: None,
        allowed_paths: vec!["different/".into()],
        test_commands: vec![],
        snapshot: None,
    })
    .unwrap();
    s.connection()
        .execute(
            "UPDATE tasks SET status='awaiting_review' WHERE task_id=?1",
            [f.id.to_string()],
        )
        .unwrap();
    // A real task with a lost reservation still gates delivery.
    s.connection()
        .execute("DELETE FROM active_writers", [])
        .unwrap();
    let server = server(&f);
    let result = call(&server, "accept_task", json!({"task_id":f.id.to_string()}));
    assert_eq!(result["status"], "accepted", "{result}");
    assert_eq!(result["delivery"]["code"], "active_writers");
    assert!(!f.dest().exists());
    s.connection()
        .execute(
            "UPDATE tasks SET status='closed' WHERE task_id=?1",
            [other.to_string()],
        )
        .unwrap();
    let result = call(&server, "accept_task", json!({"task_id":f.id.to_string()}));
    assert_eq!(result["delivery"]["state"], "delivered", "{result}");
    // The fixture config is manual throughout; task-frozen on_accept controls behavior.
    assert_eq!(
        f.project.delivery_mode(),
        bridge_domain::DeliveryMode::Manual
    );
}
#[test]
fn accepted_io_failure_retains_state_and_reports_a_safe_current_refusal() {
    let f = common::Fixture::new();
    f.changes();
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute("UPDATE tasks SET delivery_mode='on_accept'", [])
        .unwrap();
    // A regular file at the artifact directory refuses before any main write.
    fs::write(f.dest(), "blocking artifact path").unwrap();
    let server = server(&f);
    let result = call(&server, "accept_task", json!({"task_id":f.id.to_string()}));
    assert_eq!(result["status"], "accepted");
    assert_eq!(result["delivery"]["state"], "refused");
    assert_eq!(result["delivery"]["code"], "artifact_unwritable");
    assert_eq!(result["delivery"]["delivery_state"], "none");
    assert!(!result.to_string().contains(f.root.to_str().unwrap()));
    assert_eq!(
        fs::read(f.project.workspace().join("src/a")).unwrap(),
        b"base\0binary"
    );
    fs::remove_file(f.dest()).unwrap();
    assert_eq!(
        call(&server, "accept_task", json!({"task_id":f.id.to_string()}))["delivery"]["state"],
        "delivered"
    );
}
