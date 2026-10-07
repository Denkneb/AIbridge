//! Read-only bounded endpoint probes and ownership diagnostics.
use crate::{
    RuntimeError,
    process::{boot_id, identity, read_record},
};
use bridge_config::ProjectEntry;
use bridge_opencode::{
    DocError, HealthError, HttpRequest, HttpTransport, IdentityError, OpenCodeClient,
    TransportError,
};
use bridge_storage::RustStateLayout;
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenCodeIssue {
    Timeout,
    Unresponsive,
    Unavailable,
    Authentication,
    Unhealthy,
    InvalidHealth,
    WorkspaceMismatch,
    InvalidWorkspace,
    IncompatibleApi,
    InvalidApi,
    Http(u16),
    Exited,
}
impl OpenCodeIssue {
    pub fn code(self) -> &'static str {
        match self {
            Self::Timeout => "opencode_request_timeout",
            Self::Unresponsive => "opencode_server_unresponsive",
            Self::Unavailable => "opencode_endpoint_unavailable",
            Self::Authentication => "opencode_authentication_failed",
            Self::Unhealthy => "opencode_server_unhealthy",
            Self::InvalidHealth => "opencode_health_invalid",
            Self::WorkspaceMismatch => "opencode_workspace_mismatch",
            Self::InvalidWorkspace => "opencode_workspace_unverified",
            Self::IncompatibleApi => "opencode_api_incompatible",
            Self::InvalidApi => "opencode_api_invalid",
            Self::Http(_) => "opencode_http_error",
            Self::Exited => "opencode_process_exited",
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::Timeout => "OpenCode не ответил за время проверки готовности.",
            Self::Unresponsive => {
                "Процесс OpenCode запущен, но не отвечает на HTTP-проверку готовности; возможно, сервер завис. Проверьте его логи перед явным перезапуском."
            }
            Self::Unavailable => "Не удалось подключиться к HTTP-серверу OpenCode.",
            Self::Authentication => "OpenCode отклонил учётные данные или файл пароля недоступен.",
            Self::Unhealthy => "OpenCode отвечает, но сообщает, что сервер не готов.",
            Self::InvalidHealth => "OpenCode вернул некорректный ответ проверки здоровья.",
            Self::WorkspaceMismatch => "Сервер OpenCode обслуживает другой workspace.",
            Self::InvalidWorkspace => {
                "OpenCode не вернул корректный workspace; привязка сервера не подтверждена."
            }
            Self::IncompatibleApi => {
                "API запущенного OpenCode несовместим с требованиями AIbridge."
            }
            Self::InvalidApi => "OpenCode вернул некорректное описание API.",
            Self::Http(_) => "OpenCode ответил на проверку готовности ошибкой HTTP.",
            Self::Exited => {
                "Процесс OpenCode завершился до успешной проверки готовности. Проверьте серверный лог."
            }
        }
    }
    /// A timeout alone does not prove a live process, or the cause of a hang.
    pub fn with_live_process(self, live: bool) -> Self {
        if live && self == Self::Timeout {
            Self::Unresponsive
        } else {
            self
        }
    }
}
fn transport_issue(error: TransportError, invalid: OpenCodeIssue) -> OpenCodeIssue {
    match error {
        TransportError::Timeout => OpenCodeIssue::Timeout,
        TransportError::Unavailable => OpenCodeIssue::Unavailable,
        TransportError::InvalidAuth | TransportError::Unauthorized => OpenCodeIssue::Authentication,
        TransportError::HttpStatus(status) => OpenCodeIssue::Http(status),
        TransportError::NotFound => OpenCodeIssue::Http(404),
        _ => invalid,
    }
}
pub fn opencode_probe(project: &ProjectEntry, timeout: Duration) -> Result<(), OpenCodeIssue> {
    let client = OpenCodeClient::from_project(project, timeout)
        .map_err(|_| OpenCodeIssue::Authentication)?;
    let health = client.health().map_err(|e| match e {
        HealthError::Transport(error) => transport_issue(error, OpenCodeIssue::InvalidHealth),
        _ => OpenCodeIssue::InvalidHealth,
    })?;
    if !health.healthy() {
        return Err(OpenCodeIssue::Unhealthy);
    }
    client.verify_workspace().map_err(|e| match e {
        IdentityError::Transport(error) => transport_issue(error, OpenCodeIssue::InvalidWorkspace),
        IdentityError::Mismatch => OpenCodeIssue::WorkspaceMismatch,
        _ => OpenCodeIssue::InvalidWorkspace,
    })?;
    let doc = client.check_compatibility().map_err(|e| match e {
        DocError::Transport(error) => transport_issue(error, OpenCodeIssue::InvalidApi),
        _ => OpenCodeIssue::InvalidApi,
    })?;
    if !doc.is_compatible() {
        return Err(OpenCodeIssue::IncompatibleApi);
    }
    Ok(())
}
pub fn opencode(project: &ProjectEntry, timeout: Duration) -> bool {
    opencode_probe(project, timeout).is_ok()
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
            "set_task_status",
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
    require_main_record(&record, project, kind)?;
    Ok("live")
}

pub(crate) fn require_main_record(
    record: &crate::process::ProcessRecord,
    project: &ProjectEntry,
    kind: &str,
) -> Result<(), RuntimeError> {
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
    Ok(())
}
