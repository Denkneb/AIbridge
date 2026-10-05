//! Source-compatible compact/review/verbose status payloads.
use crate::McpServer;
use bridge_domain::{ExecutionMode, ProfileDefinitionSource, TaskStatus, VerifierState};
use bridge_storage::{
    RoundRow, Task,
    usage::{normalize_usage, total_saved_usage},
};
use bridge_worker::recovery_startup::task_worker_running;
use serde_json::{Value, json};
type Result<T> = std::result::Result<T, &'static str>;
fn nonempty(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
        Value::String(v) => !v.is_empty(),
        _ => true,
    }
}
fn copy_nonempty(out: &mut Value, stored: &Value, keys: &[&str]) {
    for key in keys {
        if let Some(v) = stored.get(*key).filter(|v| nonempty(v)) {
            out[*key] = v.clone();
        }
    }
}
fn model(v: &Value) -> Option<Value> {
    let provider = v.get("provider_id")?.as_str()?.trim();
    let model = v.get("model_id")?.as_str()?.trim();
    (!provider.is_empty() && !model.is_empty())
        .then(|| json!({"provider_id":provider,"model_id":model}))
}
fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        s.into()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}
fn action(server: &McpServer, task: &Task, row: Option<&RoundRow>, mode: ExecutionMode) -> Value {
    let make_command = |command: &str, attach: bool| {
        let mut args = vec![
            "agent-bridge".to_owned(),
            command.to_owned(),
            "--project".into(),
            task.project_id.to_string(),
        ];
        if attach || mode == ExecutionMode::Worktree {
            args.extend(["--task".into(), task.task_id.to_string()]);
        }
        args.extend([
            "--config".into(),
            server.project.source_path().to_string_lossy().into_owned(),
            "--state-root".into(),
            server.layout.state_root().to_string_lossy().into_owned(),
        ]);
        args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
    };
    let session = task.session_id.as_ref();
    json!({"type":"open_project_console", "message": if session.is_some() {
        "OpenCode приостановил задачу и ждёт ответа пользователя: нужно обработать запрос разрешения (permission) или вопрос (question) в OpenCode TUI."
    } else { "OpenCode приостановил задачу и ждёт ответа пользователя, но его сессия ещё недоступна, поэтому открыть TUI пока нельзя." },
    "command":make_command("console",false),"session_id":session,
    "session_title":row.and_then(|r|session.map(|_|bridge_worker::round_session_title(task.task_id,r.round_number))),
    "fallback_command":session.map(|_|make_command("attach-opencode",true)),
    "instructions": if session.is_some() {
        "Откройте проектную OpenCode TUI командой из command только если она ещё не открыта. В уже открытой TUI переключитесь на session_id/session_title, осознанно ответьте на запрос разрешения (permission) или вопрос (question) и не разрешайте внешний доступ автоматически. fallback_command открывает нужную сессию напрямую. Затем повторите task_status(wait_seconds=300)."
    } else { "Откройте проектную OpenCode TUI командой из command только если она ещё не открыта. Сессия задачи пока не создана: изучите blockers/error и повторите task_status(wait_seconds=300). Когда session_id появится, переключитесь на неё в этой же TUI; не разрешайте внешний доступ автоматически." }})
}
impl McpServer {
    pub(super) fn status_task(&self, id: bridge_domain::TaskId) -> Result<Option<Task>> {
        use rusqlite::OptionalExtension;
        let task = self
            .storage()?
            .connection()
            .query_row(
                "SELECT * FROM tasks WHERE task_id=?1",
                [id.to_string()],
                |r| Ok(Task::from_row_for_status(r)),
            )
            .optional()
            .map_err(|_| "state_unavailable")?
            .transpose()
            .map_err(|_| "state_unavailable")?;
        if task.as_ref().is_some_and(|t| {
            &t.project_id != self.project.id()
                || std::path::Path::new(&t.workspace) != self.project.workspace()
        }) {
            return Err("task_binding_mismatch");
        }
        Ok(task)
    }

    pub(super) fn result(&self, task: &Task, verbose: bool) -> Result<Value> {
        let mut storage = self.storage()?;
        let rows = {
            let mut q = storage
                .connection()
                .prepare(
                    "SELECT * FROM rounds WHERE task_id=?1 AND project_id=?2 ORDER BY round_number",
                )
                .map_err(|_| "state_unavailable")?;
            q.query_map(
                rusqlite::params![task.task_id.to_string(), task.project_id.as_str()],
                |r| Ok(RoundRow::from_row(r)),
            )
            .map_err(|_| "state_unavailable")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "state_unavailable")?
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "state_unavailable")?
        };
        let row = rows.last();
        let stored = row.and_then(|r| r.result_json.clone()).unwrap_or(json!({}));
        let snapshot = task.snapshot.clone().unwrap_or(json!({}));
        let inflight = matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising);
        let mode = self.execution_mode(task.task_id)?;
        let running = !task.status.is_terminal()
            && task_worker_running(&self.layout, task.task_id).map_err(|_| "state_unavailable")?;
        let worker = json!({"running":running,"started_at":row.and_then(|r|r.worker_started_at.as_deref()),"deadline_at":row.and_then(|r|r.worker_deadline_at.as_deref())});
        let mut result = json!({"task_id":task.task_id.to_string(),"project_id":task.project_id.as_str(),"status":task.status,"round_number":row.map(|r|r.round_number),"revision_count":task.revision_count});
        if let Some(session) = &task.session_id {
            result["session_id"] = json!(session);
        }
        let baseline = stored
            .get("baseline_dirty_paths")
            .filter(|v| nonempty(v))
            .or(snapshot.get("dirty_paths"))
            .cloned()
            .unwrap_or(json!([]));
        let before = stored
            .get("head_before")
            .or(snapshot.get("head"))
            .cloned()
            .unwrap_or(Value::Null);
        let after = stored.get("head_after").cloned().unwrap_or(before.clone());
        let repos=stored.get("repositories").filter(|v|v.as_array().is_some_and(|a|!a.is_empty())).cloned().unwrap_or_else(||json!([{
            "root":task.workspace,"baseline_dirty_paths":baseline,"changed_paths":stored.get("changed_paths").cloned().unwrap_or(json!([])),"committed_paths":stored.get("committed_paths").cloned().unwrap_or(json!([])),
            "scope_violations":stored.get("scope_violations").cloned().unwrap_or(json!([])),"git_policy_violations":stored.get("git_policy_violations").cloned().unwrap_or(json!([])),"head_before":before,"head_after":after
        }]));
        if inflight {
            result["created_at"] = json!(task.created_at);
            result["updated_at"] = json!(task.updated_at);
            result["session_id"] = json!(task.session_id);
            result["close_requested"] = json!(task.close_requested_at.is_some());
            result["close_requested_at"] = json!(task.close_requested_at);
            result["worker"] = worker.clone();
            result["phase"] = json!(if row
                .is_some_and(|r| r.verifier_state == Some(VerifierState::Running))
            {
                "verifying"
            } else {
                "agent"
            });
            if let Some(row) = row.filter(|r| r.verifier_state == Some(VerifierState::Running)) {
                result["verification_progress"] = storage
                    .verifier_progress(
                        &bridge_storage::RoundRef {
                            task_id: task.task_id,
                            project_id: task.project_id.clone(),
                            round_number: row.round_number,
                        },
                        task.test_commands.len() as u64,
                    )
                    .map_err(|_| "state_unavailable")?
                    .unwrap_or(Value::Null);
            }
        } else if verbose {
            result["created_at"] = json!(task.created_at);
            result["updated_at"] = json!(task.updated_at);
            result["session_id"] = json!(task.session_id);
            result["close_requested"] = json!(task.close_requested_at.is_some());
            result["close_requested_at"] = json!(task.close_requested_at);
            result["result"] = json!(row.and_then(|r| r.response.as_deref()));
            for key in [
                "changed_paths",
                "scope_violations",
                "git_policy_violations",
                "committed_paths",
                "tool_errors",
                "blockers",
            ] {
                result[key] = stored.get(key).cloned().unwrap_or(json!([]));
            }
            result["baseline_dirty_paths"] = baseline.clone();
            result["task_changed_paths"] = stored
                .get("task_changed_paths")
                .or(stored.get("changed_paths"))
                .cloned()
                .unwrap_or(json!([]));
            result["repositories"] = repos.clone();
            result["head_before"] = before.clone();
            result["head_after"] = after.clone();
            result["allow_dirty"] = snapshot.get("allow_dirty").cloned().unwrap_or(json!(false));
            result["allow_commit"] = snapshot
                .get("allow_commit")
                .cloned()
                .unwrap_or(json!(false));
            result["error"] = stored.get("error").cloned().unwrap_or(Value::Null);
            result["usage"] = json!({"last_round":normalize_usage(&stored["usage"]),"task_total":total_saved_usage(rows.iter().filter_map(|r|r.result_json.clone()))});
            let mut models = Vec::new();
            for r in &rows {
                if let Some(m) = r.result_json.as_ref().and_then(|v| model(&v["model"]))
                    && !models.contains(&m)
                {
                    models.push(m);
                }
            }
            result["model"] = json!({"last_round":model(&stored["model"]),"task_models":models});
            result["worker"] = worker.clone();
            if let Some(v) = stored.get("verification").filter(|v| !v.is_null()) {
                result["verification"] = v.clone();
            }
        } else if task.status == TaskStatus::AwaitingReview {
            copy_nonempty(
                &mut result,
                &stored,
                &[
                    "changed_paths",
                    "task_changed_paths",
                    "committed_paths",
                    "scope_violations",
                    "git_policy_violations",
                    "blockers",
                    "tool_errors",
                    "verification",
                ],
            );
            for key in ["error", "head_before", "head_after"] {
                if let Some(v) = stored.get(key).filter(|v| !v.is_null()) {
                    result[key] = v.clone();
                }
            }
            result["allow_dirty"] = snapshot.get("allow_dirty").cloned().unwrap_or(json!(false));
            result["allow_commit"] = snapshot
                .get("allow_commit")
                .cloned()
                .unwrap_or(json!(false));
            if nonempty(&baseline) {
                result["baseline_dirty_paths"] = baseline;
            }
            if repos.as_array().is_some_and(|a| {
                a.iter().any(|r| {
                    nonempty(&r["scope_violations"]) || nonempty(&r["git_policy_violations"])
                })
            }) {
                result["repositories"] = repos;
            }
        } else if matches!(
            task.status,
            TaskStatus::NeedsUser | TaskStatus::Failed | TaskStatus::DeliveryUnknown
        ) {
            result["updated_at"] = json!(task.updated_at);
            copy_nonempty(&mut result, &stored, &["blockers", "tool_errors"]);
            if let Some(v) = stored.get("error").filter(|v| !v.is_null()) {
                result["error"] = v.clone();
            }
        }
        if task.status == TaskStatus::NeedsUser {
            result["session_id"] = json!(task.session_id);
            result["worker"] = worker.clone();
            result["user_action"] = action(self, task, row, mode);
        }
        if !inflight && !task.status.is_terminal() {
            if task.close_requested_at.is_some() {
                result["close_requested"] = json!(true);
                result["close_requested_at"] = json!(task.close_requested_at);
            }
            if running {
                result["worker"] = worker;
            }
        }
        if let Some(profile) = storage
            .get_task_profile(task.task_id, self.project.id())
            .map_err(|_| "profile_snapshot_corrupt")?
            && (profile.source != ProfileDefinitionSource::Builtin || profile.id != "implementer")
        {
            result["profile"] = json!(profile.id);
            result["profile_source"] = json!(profile.source);
        }
        if !inflight && (verbose || task.status == TaskStatus::AwaitingReview) {
            let decision = storage
                .revision_budget_decision(task.task_id, self.project.id(), false)
                .map_err(|_| "state_unavailable")?;
            if let Some(mut b) = decision.state
                && b.get("corrupt") != Some(&json!(true))
            {
                b["configured"] = json!(true);
                if !verbose {
                    b.as_object_mut().unwrap().retain(|k, _| {
                        [
                            "configured",
                            "warning",
                            "exhausted",
                            "exhausted_fields",
                            "gate",
                            "gate_reason",
                            "corrupt",
                        ]
                        .contains(&k.as_str())
                    });
                }
                result["budget"] = b;
            }
        }
        if mode == ExecutionMode::Worktree {
            result["execution_mode"] = json!("worktree");
            result["delivery_mode"] = json!(task.delivery_mode);
            if let Some(record) = storage
                .get_worktree(task.task_id, self.project.id())
                .map_err(|_| "state_unavailable")?
            {
                let mut state = json!({"status":record.status.as_str(),"delivery_state":record.delivery_state.map_or("none", |s|s.as_str())});
                if let Some(port) = record.server_port {
                    state["server_port"] = json!(port.get());
                }
                if let Some(endpoint) = record.server_endpoint {
                    state["server_endpoint"] = json!(endpoint);
                    state["server_state"] = json!(match bridge_runtime::worktree_server_state(
                        &self.layout,
                        &self.project,
                        task.task_id
                    ) {
                        Ok(bridge_runtime::ServerState::Missing) => "missing",
                        Ok(bridge_runtime::ServerState::Stale) => "stale",
                        Ok(bridge_runtime::ServerState::Live) => "running",
                        Err(_) => "unknown",
                    });
                }
                if let Some(at) = record.delivered_at {
                    state["delivered_at"] = json!(at);
                }
                if verbose || task.status == TaskStatus::AwaitingReview {
                    result["worktree"] = json!(record.path);
                }
                result["worktree_state"] = state;
            } else {
                result["worktree_state"] = json!({"status":"missing"});
            }
        }
        Ok(result)
    }
}
