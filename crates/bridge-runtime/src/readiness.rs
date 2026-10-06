//! Read-only bounded endpoint probes and ownership diagnostics.
use crate::{
    RuntimeError,
    process::{boot_id, identity, read_record},
};
use bridge_config::ProjectEntry;
use bridge_opencode::{HttpRequest, HttpTransport, OpenCodeClient};
use bridge_storage::RustStateLayout;
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
pub fn opencode(project: &ProjectEntry, timeout: Duration) -> bool {
    OpenCodeClient::from_project(project, timeout).is_ok_and(|c| {
        c.health().is_ok_and(|h| h.healthy())
            && c.verify_workspace().is_ok()
            && c.check_compatibility().is_ok_and(|d| d.is_compatible())
    })
}
pub fn mcp(project: &ProjectEntry, timeout: Duration) -> bool {
    let Some(endpoint) = project.mcp_endpoint() else {
        return false;
    };
    let Ok(Some(token)) = project.read_mcp_token() else {
        return false;
    };
    let Ok(client) = HttpTransport::bearer(*endpoint.base(), token, timeout) else {
        return false;
    };
    let call = |method: &str, params: Value| -> Option<Value> {
        let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
        let response = client
            .request(&HttpRequest::post("/mcp", body.to_string().into_bytes()))
            .ok()?;
        let value: Value = serde_json::from_slice(response.body()).ok()?;
        if value["jsonrpc"] != "2.0" || value["id"] != 1 || value.get("error").is_some() {
            return None;
        }
        value.get("result").cloned()
    };
    if call("initialize",json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"agent-bridge-readiness","version":"1"}})).is_none(){return false;}
    let Some(tools) = call("tools/list", json!({})) else {
        return false;
    };
    let Some(list) = tools["tools"].as_array() else {
        return false;
    };
    let names = list
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect::<BTreeSet<_>>();
    if names
        != BTreeSet::from([
            "project_info",
            "submit_task",
            "task_status",
            "request_changes",
            "accept_task",
            "close_task",
        ])
    {
        return false;
    }
    let Some(info) = call("tools/call", json!({"name":"project_info","arguments":{}})) else {
        return false;
    };
    if info["isError"] == true {
        return false;
    }
    let structured = info.get("structuredContent").cloned().or_else(|| {
        info["content"]
            .as_array()?
            .iter()
            .find_map(|c| serde_json::from_str::<Value>(c["text"].as_str()?).ok())
    });
    let Some(info) = structured else { return false };
    info["project_id"] == project.id().as_str()
        && info["workspace"] == json!(project.workspace())
        && info["endpoint"] == project.opencode_endpoint().url()
}
pub fn record_state(
    layout: &RustStateLayout,
    project: &ProjectEntry,
    kind: &str,
) -> Result<&'static str, RuntimeError> {
    if !matches!(kind, "opencode" | "mcp") || layout.project_id() != project.id() {
        return Err(RuntimeError::Binding);
    }
    if !layout.project_dir().exists() {
        return Ok("missing");
    }
    layout
        .open_readonly()
        .map_err(|_| RuntimeError::Ownership)?;
    let Some(record) = read_record(&layout.project_dir().join(format!("{kind}.process.json")))?
    else {
        return Ok("missing");
    };
    if identity(record.pid).as_deref() != Some(&record.start) || boot_id()? != record.boot_id {
        return Ok("stale");
    }
    let endpoint = if kind == "opencode" {
        project.opencode_endpoint().url()
    } else {
        project
            .mcp_endpoint()
            .ok_or(RuntimeError::Binding)?
            .base()
            .url()
    };
    if record.project_id != *project.id()
        || record.checkout != project.workspace()
        || record.kind != kind
        || record.port
            != if kind == "opencode" {
                project.opencode_endpoint().port()
            } else {
                project.mcp_endpoint().ok_or(RuntimeError::Binding)?.port()
            }
        || record.endpoint != endpoint
        || record.task_id.to_string() != "00000000-0000-0000-0000-000000000000"
    {
        return Err(RuntimeError::ForeignProcess);
    }
    Ok("live")
}
