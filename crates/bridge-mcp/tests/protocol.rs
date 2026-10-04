mod common;
use bridge_domain::{ExecutionMode, TaskId, TaskStatus};
use bridge_mcp::{McpError, McpServer, protocol::Protocol, stdio};
use bridge_storage::{AdmissionSettings, CreateTaskInput, RustStateLayout};
use common::{Fixture, initialize};
use serde_json::{Value, json};
use std::{fs, io::Cursor, os::unix::fs::symlink, str::FromStr};
fn add_task(f: &Fixture, id: u32, parallel: bool) -> TaskId {
    let task_id = TaskId::from_str(&format!("00000000-0000-4000-8000-{id:012}")).unwrap();
    f.layout
        .open()
        .unwrap()
        .create_task_with_admission(
            CreateTaskInput {
                task_id,
                project_id: f.project.id().clone(),
                workspace: f.project.workspace().to_str().unwrap().to_owned(),
                task: "private-task-content".into(),
                request_id: format!("req-{id}"),
                payload_hash: format!("hash-{id}"),
                base_head: None,
                allowed_paths: vec![format!("src-{id}")],
                test_commands: vec![],
                snapshot: None,
            },
            &AdmissionSettings::new(
                if parallel { 3 } else { 1 },
                parallel,
                if parallel {
                    ExecutionMode::Worktree
                } else {
                    ExecutionMode::Direct
                },
            )
            .unwrap(),
            TaskStatus::Implementing,
        )
        .unwrap();
    task_id
}
#[test]
fn project_info_matches_five_frozen_binding_and_parallel_cases_without_recovery_or_secret_fields() {
    let corpus: Value =
        serde_json::from_str(include_str!("../../../docs/fixtures/mcp-cases.json")).unwrap();
    let cases = corpus["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["tool"] == "project_info");
    let mut count = 0;
    for case in cases {
        count += 1;
        let parallel = case["id"] == "project-info-parallel-writer-set";
        let f = Fixture::new(if parallel {
            "execution_mode=\"worktree\"\nmax_active_tasks=3\nallow_parallel_writers=true"
        } else {
            ""
        });
        let server = f.server();
        if !case["setup"]["task"].is_null() || parallel {
            add_task(&f, 1, parallel);
        }
        if parallel {
            add_task(&f, 2, true);
        } else {
            f.layout
                .open()
                .unwrap()
                .connection()
                .execute("DELETE FROM active_writers", [])
                .unwrap();
        }
        let before = f.layout.open().unwrap().active_set(f.project.id()).unwrap();
        let actual = server.project_info().unwrap();
        for (key, value) in case["expect"]["values"].as_object().unwrap() {
            let text = value
                .to_string()
                .replace("${PROJECT_ID}", "proj")
                .replace("${WORKSPACE}", f.project.workspace().to_str().unwrap())
                .replace("${TASK_ID}", "00000000-0000-4000-8000-000000000001")
                .replace("${OTHER_TASK_ID}", "00000000-0000-4000-8000-000000000002");
            assert_eq!(
                actual[key],
                serde_json::from_str::<Value>(&text).unwrap(),
                "{}: {key}",
                case["id"]
            );
        }
        assert_eq!(actual["delivery_mode"], "manual");
        let after = f.layout.open().unwrap().active_set(f.project.id()).unwrap();
        assert_eq!(before.tasks, after.tasks);
        assert_eq!(before.reservations, after.reservations);
        assert!(!actual.to_string().contains("private-task-content"));
        assert!(
            actual["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p.as_object().unwrap().len() == 2)
        );
    }
    assert_eq!(count, 5);
}
#[test]
fn stdio_handshake_lists_only_working_tools_and_emits_no_notifications_or_banners() {
    let f = Fixture::new("");
    let server = f.server();
    let messages = [
        initialize(),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":"list","method":"tools/list","params":{"_meta":{"progressToken":"list-progress"}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"project_info","arguments":{}}}),
        json!({"jsonrpc":"2.0","method":"notifications/unknown","params":{"private":"not-echoed"}}),
    ];
    let input = messages
        .iter()
        .map(|m| format!("{m}\n"))
        .collect::<String>();
    let mut output = Vec::new();
    stdio::run(&server, Cursor::new(input), &mut output).unwrap();
    let lines: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(lines[1]["id"], "list");
    let tools = lines[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "project_info");
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    assert_eq!(
        lines[2]["result"]["structuredContent"]["project_id"],
        "proj"
    );
    assert_eq!(
        serde_json::from_str::<Value>(lines[2]["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap(),
        lines[2]["result"]["structuredContent"]
    );
}
#[test]
fn malformed_requests_initialization_and_unavailable_mutations_are_redacted_and_side_effect_free() {
    let f = Fixture::new("");
    let server = f.server();
    let mut protocol = Protocol::default();
    assert_eq!(
        protocol
            .handle_bytes(&server, b"bad private-token")
            .unwrap()["error"]["code"],
        -32700
    );
    for request in [
        json!([]),
        json!({"jsonrpc":"1.0","id":1,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":null,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":1.5,"method":"ping"}),
    ] {
        assert_eq!(
            protocol.handle(&server, request).unwrap()["error"]["code"],
            -32600
        );
    }
    assert_eq!(
        protocol
            .handle(
                &server,
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
            )
            .unwrap()["error"]["code"],
        -32000
    );
    assert_eq!(
        protocol
            .handle(
                &server,
                json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{}})
            )
            .unwrap()["error"]["code"],
        -32602
    );
    assert!(
        protocol
            .handle(&server, initialize())
            .unwrap()
            .get("result")
            .is_some()
    );
    protocol.handle(
        &server,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    assert_eq!(
        protocol.handle(&server, initialize()).unwrap()["error"]["code"],
        -32600
    );
    assert_eq!(
        protocol
            .handle(
                &server,
                json!({"jsonrpc":"2.0","id":5,"method":"tools/list","params":{"_meta":[]}})
            )
            .unwrap()["error"]["code"],
        -32602
    );
    for name in [
        "submit_task",
        "task_status",
        "request_changes",
        "accept_task",
        "close_task",
    ] {
        let response=protocol.handle(&server,json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":name,"arguments":{"task":"private-request-content"}}})).unwrap();
        assert_eq!(response["error"]["code"], -32602);
        assert!(!response.to_string().contains("private-request-content"));
    }
    assert_eq!(protocol.handle(&server,json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"project_info","arguments":{"unexpected":true}}})).unwrap()["error"]["code"],-32602);
    assert_eq!(
        protocol
            .handle(
                &server,
                json!({"jsonrpc":"2.0","id":10,"method":"secret-method"})
            )
            .unwrap()["error"]["code"],
        -32601
    );
    assert!(server.project_info().unwrap()["active_task_id"].is_null());
}
#[test]
fn corrupt_storage_surfaces_as_tool_failure_without_partial_data() {
    let f = Fixture::new("");
    let server = f.server();
    let id = add_task(&f, 1, false);
    f.layout
        .open()
        .unwrap()
        .connection()
        .execute(
            "UPDATE tasks SET allowed_paths='private corrupt scope' WHERE task_id=?1",
            [id.to_string()],
        )
        .unwrap();
    let mut protocol = Protocol::stateless_http();
    let result = protocol
        .handle(
            &server,
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"project_info"}}),
        )
        .unwrap();
    assert_eq!(result["result"]["isError"], true);
    assert_eq!(
        result["result"]["content"][0]["text"],
        "mcp_state_unavailable"
    );
    assert!(!result.to_string().contains("private corrupt scope"));
}
#[test]
fn namespace_and_mcp_lock_fail_closed_without_adopting_foreign_files() {
    let f = Fixture::new("");
    let server = f.server();
    assert!(matches!(
        McpServer::open(f.project.clone(), f.layout.clone()),
        Err(McpError::Busy)
    ));
    drop(server);
    drop(f.server());
    fs::write(
        f.layout.marker(),
        r#"{"implementation":"python","format_version":1}"#,
    )
    .unwrap();
    let before = fs::read(f.layout.database()).unwrap();
    assert!(matches!(
        McpServer::open(f.project.clone(), f.layout.clone()),
        Err(McpError::State)
    ));
    assert_eq!(fs::read(f.layout.database()).unwrap(), before);
    let f = Fixture::new("");
    fs::create_dir(f.root.join("foreign")).unwrap();
    symlink(f.root.join("foreign"), f.layout.state_root()).unwrap();
    assert!(matches!(
        McpServer::open(f.project.clone(), f.layout.clone()),
        Err(McpError::Binding)
    ));
    assert_eq!(fs::read_dir(f.root.join("foreign")).unwrap().count(), 0);
    let layout =
        RustStateLayout::new(f.root.join("workspace/state"), f.project.id().clone()).unwrap();
    assert!(matches!(
        McpServer::open(f.project.clone(), layout),
        Err(McpError::Binding)
    ));
}
#[test]
fn oversized_stdio_input_is_bounded_and_never_echoed() {
    let f = Fixture::new("");
    let server = f.server();
    let mut output = Vec::new();
    let error = stdio::run(
        &server,
        Cursor::new(vec![b'x'; stdio::MAX_MESSAGE_BYTES + 1]),
        &mut output,
    )
    .unwrap_err();
    assert_eq!(error, McpError::FrameTooLarge);
    assert!(output.is_empty());
}
