//! JSON-RPC/MCP lifecycle with fixed, redacted protocol errors.
use crate::McpServer;
use serde_json::{Value, json};
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", PROTOCOL_VERSION];
#[derive(Default)]
pub struct Protocol {
    phase: Phase,
    stateless: bool,
}
#[derive(Default, PartialEq, Eq)]
enum Phase {
    #[default]
    New,
    Initializing,
    Ready,
}
impl Protocol {
    /// Sessionless HTTP carries the version on each request; no session ids.
    pub fn stateless_http() -> Self {
        Self {
            phase: Phase::Ready,
            stateless: true,
        }
    }
    /// Handles one JSON message. Notifications never produce a response.
    pub fn handle_bytes(&mut self, server: &McpServer, bytes: &[u8]) -> Option<Value> {
        match serde_json::from_slice(bytes) {
            Ok(value) => self.handle(server, value),
            Err(_) => Some(error(Value::Null, -32700, "Parse error")),
        }
    }
    pub fn handle(&mut self, server: &McpServer, message: Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(error(Value::Null, -32600, "Invalid Request"));
        };
        let id = object.get("id").cloned();
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || id.as_ref().is_some_and(|id| {
                !id.is_string() && !(id.as_i64().is_some() || id.as_u64().is_some())
            })
        {
            return Some(error(Value::Null, -32600, "Invalid Request"));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            // This server issues no requests; received responses are ignored.
            if id.is_some() && (object.contains_key("result") || object.contains_key("error")) {
                return None;
            }
            return Some(error(Value::Null, -32600, "Invalid Request"));
        };
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        let Some(id) = id else {
            if method == "notifications/initialized"
                && params.is_object()
                && self.phase == Phase::Initializing
            {
                self.phase = Phase::Ready;
            }
            return None;
        };
        if !params.is_object() || params.get("_meta").is_some_and(|meta| !meta.is_object()) {
            return Some(error(id, -32602, "Invalid params"));
        }
        let result = match method {
            "initialize" => {
                let version = params.get("protocolVersion").and_then(Value::as_str);
                let client = params.get("clientInfo");
                if version.is_none()
                    || !params.get("capabilities").is_some_and(Value::is_object)
                    || !client
                        .and_then(|c| c.get("name"))
                        .is_some_and(Value::is_string)
                    || !client
                        .and_then(|c| c.get("version"))
                        .is_some_and(Value::is_string)
                {
                    return Some(error(id, -32602, "Invalid params"));
                }
                if !self.stateless && self.phase != Phase::New {
                    return Some(error(id, -32600, "Already initialized"));
                }
                self.phase = Phase::Initializing;
                let version = version
                    .filter(|v| SUPPORTED_VERSIONS.contains(v))
                    .unwrap_or(PROTOCOL_VERSION);
                json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},
                    "serverInfo":{"name":"agent-bridge","version":env!("CARGO_PKG_VERSION")},
                    "instructions":"Rust MCP currently exposes project_info only; delegated task tools and startup recovery are not implemented."})
            }
            "ping" => json!({}),
            _ if self.phase != Phase::Ready => {
                return Some(error(id, -32000, "Server not initialized"));
            }
            "tools/list" => {
                if !params
                    .as_object()
                    .is_some_and(|p| p.keys().all(|key| key == "_meta"))
                {
                    return Some(error(id, -32602, "Invalid params"));
                }
                json!({"tools":[{"name":"project_info","description":"Return the immutable project binding and active task state.",
                    "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
                    "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}]})
            }
            "tools/call" => {
                if params.get("name").and_then(Value::as_str) != Some("project_info") {
                    return Some(error(id, -32602, "Unknown or unavailable tool"));
                }
                if params
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|k| k != "name" && k != "arguments" && k != "_meta")
                    || params
                        .get("arguments")
                        .is_some_and(|v| !v.as_object().is_some_and(|o| o.is_empty()))
                {
                    return Some(error(id, -32602, "Invalid params"));
                }
                match server.project_info() {
                    Ok(payload) => {
                        json!({"content":[{"type":"text","text":payload.to_string()}],"structuredContent":payload,"isError":false})
                    }
                    Err(_) => {
                        json!({"content":[{"type":"text","text":"mcp_state_unavailable"}],"isError":true})
                    }
                }
            }
            _ => return Some(error(id, -32601, "Method not found")),
        };
        Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
    }
}
fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
