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
            "invalid_budget" => {
                let budget = &args["budget"];
                let detail = if !budget.is_object() {
                    "budget must be an object".to_owned()
                } else if budget
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|k| k != "limits" && k != "warning_threshold")
                {
                    "unknown budget field".to_owned()
                } else if !budget["limits"].as_object().is_some_and(|o| !o.is_empty()) {
                    "budget limits must be a non-empty object".to_owned()
                } else if let Some((key, _)) = budget["limits"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .find(|(_, v)| !v.as_f64().is_some_and(|n| n.is_finite() && n > 0.0))
                {
                    if bridge_storage::BUDGET_USAGE_FIELDS.contains(&key.as_str()) {
                        format!("budget limit '{key}' must be a positive finite number")
                    } else {
                        "unknown budget limit".to_owned()
                    }
                } else {
                    "warning_threshold must be in (0, 1]".to_owned()
                };
                result["detail"] = json!(detail);
            }
            "invalid_allowed_paths" => {
                result["detail"] = json!(if args["allowed_paths"].as_array().is_some_and(|p| p
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|s| std::path::Path::new(s).is_absolute()))
                {
                    "use a relative path or a trusted external directory"
                } else {
                    "allowed_paths must contain valid relative scopes or trusted external paths"
                });
            }
            "git_snapshot_failed" => {
                result["detail"] = json!("repository snapshot could not be collected");
            }
            "invalid_structured_findings" => {
                if let Some(items) = args["structured_findings"].as_array() {
                    if items.len() > 200 {
                        result["detail"] = json!("too many findings (max 200)");
                    } else if let Some(id) = args["task_id"].as_str().and_then(|s| s.parse().ok())
                        && let Ok(Some(task)) = self.status_task(id)
                    {
                        let trusted = self
                            .project
                            .auto_approve_external_directories()
                            .iter()
                            .map(|p| p.as_path())
                            .collect::<Vec<_>>();
                        for (index, item) in items.iter().enumerate() {
                            if bridge_worker::validate_revision_findings(
                                "review",
                                Some(&json!([item])),
                                self.project.workspace(),
                                &trusted,
                                &task.allowed_paths,
                            )
                            .is_ok()
                            {
                                continue;
                            }
                            let detail = if let Some(o) = item.as_object() {
                                if o.keys().any(|k| {
                                    !["severity", "path", "line", "code", "message"]
                                        .contains(&k.as_str())
                                }) {
                                    "unknown finding field"
                                } else if serde_json::from_value::<bridge_domain::FindingSeverity>(
                                    item["severity"].clone(),
                                )
                                .is_err()
                                {
                                    "invalid severity"
                                } else {
                                    let mut path_probe = item.clone();
                                    path_probe["line"] = Value::Null;
                                    path_probe["code"] = json!("review");
                                    path_probe["message"] = json!("finding");
                                    if bridge_worker::validate_revision_findings(
                                        "review",
                                        Some(&json!([path_probe])),
                                        self.project.workspace(),
                                        &trusted,
                                        &task.allowed_paths,
                                    )
                                    .is_err()
                                    {
                                        "finding path is invalid or outside allowed scope"
                                    } else if item.get("line").is_some_and(|v| {
                                        !v.is_null()
                                            && !v
                                                .as_u64()
                                                .is_some_and(|n| (1..=1_000_000).contains(&n))
                                    }) {
                                        "line must be a positive integer"
                                    } else if !item["code"].as_str().is_some_and(|s| {
                                        !s.is_empty()
                                            && s.len() <= 64
                                            && s.as_bytes()[0].is_ascii_alphanumeric()
                                            && s.bytes().all(|b| {
                                                b.is_ascii_alphanumeric() || b"._+-".contains(&b)
                                            })
                                    }) {
                                        "invalid code"
                                    } else if !item["message"]
                                        .as_str()
                                        .is_some_and(|s| !s.trim().is_empty())
                                    {
                                        "message must be a non-empty string"
                                    } else {
                                        "message is too long"
                                    }
                                }
                            } else {
                                "finding must be an object"
                            };
                            result["index"] = json!(index);
                            result["detail"] = json!(detail);
                            break;
                        }
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
                        result["budget"] = state;
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
