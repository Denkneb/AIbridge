//! Safe public error context. Diagnostic strings never render caller payloads.
use crate::McpServer;
use serde_json::{Value, json};
use std::collections::BTreeSet;

impl McpServer {
    pub(super) fn error_payload(&self, code: &str, args: &Value) -> Value {
        let mut result = json!({"error":code});
        match code {
            "suspected_secrets" => {
                fn collect(v: &Value, out: &mut BTreeSet<&'static str>) {
                    match v {
                        Value::String(s) => {
                            out.extend(bridge_config::suspected_secret_categories(s))
                        }
                        Value::Array(a) => a.iter().for_each(|v| collect(v, out)),
                        Value::Object(o) => o.values().for_each(|v| collect(v, out)),
                        _ => {}
                    }
                }
                let mut categories = BTreeSet::new();
                collect(args, &mut categories);
                result["categories"] = json!(categories);
            }
            "invalid_test_commands" => {
                if let Some(commands) = args["test_commands"].as_array() {
                    let refs = commands
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>();
                    if refs.len() == commands.len()
                        && let Some(problem) =
                            bridge_command_policy::validate_test_commands(&refs).first()
                    {
                        result["index"] = json!(problem.index());
                        result["reason"] = json!(problem.reason().as_str());
                    }
                }
            }
            "not_awaiting_review" | "revision_limit" | "budget_exhausted" | "budget_corrupt" => {
                if let Some(id) = args["task_id"].as_str().and_then(|s| s.parse().ok())
                    && let Ok(Some(task)) = self.status_task(id)
                {
                    result["status"] = json!(task.status);
                    if matches!(code, "budget_exhausted" | "budget_corrupt")
                        && let Ok(mut storage) = self.storage()
                        && let Ok(decision) =
                            storage.revision_budget_decision(id, self.project.id(), false)
                        && let Some(state) = decision.state
                    {
                        result["budget_state"] = state;
                    }
                }
                if code == "revision_limit" {
                    result["detail"] = json!(format!(
                        "max_rounds={} reached; no new round sent",
                        self.project.max_rounds()
                    ));
                }
            }
            _ => {}
        }
        result
    }
}
