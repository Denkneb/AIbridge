use crate::McpServer;
use serde_json::{Value, json};
fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: Value,
    read_only: bool,
) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read_only,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}})
}
pub(crate) fn definitions(server: &McpServer) -> Value {
    let mut tools = vec![tool(
        "project_info",
        "Return the immutable project binding and active task state.",
        json!({}),
        json!([]),
        true,
    )];
    if server.delegated_tools_enabled() {
        tools.extend([
            tool("submit_task","Create a task, idempotent by request_id. Dependencies wait for acceptance; explicit task_status activates ready tasks.",json!({
                "request_id":{"type":"string"},"task":{"type":"string"},"allowed_paths":{"type":"array","items":{"type":"string"},"minItems":1},"test_commands":{"type":"array","items":{"type":"string"}},
                "allow_dirty":{"type":"boolean","default":false},"allow_commit":{"type":"boolean","default":false},"allow_suspected_secrets":{"type":"boolean","default":false},
                "budget":{"type":["object","null"]},"profile":{"type":["string","null"]},"workflow_id":{"type":["string","null"]},"depends_on":{"type":["array","null"]}
            }),json!(["request_id","task","allowed_paths","test_commands"]),false),
            tool("task_status","Return compact status or verbose diagnostics; wait_seconds is bounded to 0..300. Explicit calls may recover needs_user.",json!({"task_id":{"type":["string","null"]},"wait_seconds":{"type":"integer","minimum":0,"maximum":300,"default":300},"verbose":{"type":"boolean","default":false}}),json!([]),false),
            tool("request_changes","Create a revision from review findings, idempotent by request_id.",json!({"task_id":{"type":"string"},"request_id":{"type":"string"},"findings":{"type":"string"},"structured_findings":{"type":["array","null"]},"allow_suspected_secrets":{"type":"boolean","default":false},"allow_budget_override":{"type":"boolean","default":false}}),json!(["task_id","request_id","findings"]),false),
            tool("accept_task","Accept an awaiting_review task using its frozen delivery policy; on_accept builds/applies or retries delivery.",json!({"task_id":{"type":"string"}}),json!(["task_id"]),false),
            tool("close_task","Request cooperative close; worktree task closes only after owned cleanup.",json!({"task_id":{"type":"string"},"reason":{"type":"string"}}),json!(["task_id","reason"]),false),
        ]);
    }
    json!({"tools":tools})
}
pub(crate) fn available(server: &McpServer, name: &str) -> bool {
    definitions(server)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == name)
}
pub(crate) fn valid_arguments(server: &McpServer, name: &str, args: &Value) -> bool {
    let Some(object) = args.as_object() else {
        return false;
    };
    let definitions = definitions(server);
    let Some(tool) = definitions["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
    else {
        return false;
    };
    let schema = &tool["inputSchema"];
    object.keys().all(|k| schema["properties"].get(k).is_some())
        && schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .all(|k| object.contains_key(k.as_str().unwrap()))
}
