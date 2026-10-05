//! Frozen on-accept orchestration and redacted delivery visibility.
use crate::McpServer;
use bridge_domain::{TaskId, TaskStatus};
use bridge_storage::{Task, WorktreeDeliveryState};
use bridge_worker::{WorkerLock, WorkerLockOutcome};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
type Result<T> = std::result::Result<T, &'static str>;
impl McpServer {
    pub(super) fn delivery_summary(&self, id: TaskId) -> Value {
        let Ok(storage) = self.storage() else {
            return json!({"mode":"on_accept","state":"unknown","delivery_state":"unknown"});
        };
        let record = match storage.get_worktree(id, self.project.id()) {
            Ok(Some(r)) => r,
            _ => return json!({"mode":"on_accept","state":"unknown","delivery_state":"unknown"}),
        };
        if record.delivery_state == Some(WorktreeDeliveryState::Delivered)
            || record.delivered_at.is_some()
        {
            return json!({"mode":"on_accept","state":"delivered","delivered_at":record.delivered_at});
        }
        let actual = record
            .delivery_state
            .unwrap_or(WorktreeDeliveryState::None)
            .as_str();
        let last:Option<(String,String)>=storage.connection().query_row("SELECT message,created_at FROM events WHERE task_id=?1 AND kind='delivery_refused' ORDER BY id DESC LIMIT 1",[id.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).optional().ok().flatten();
        let mut summary = json!({"mode":"on_accept","delivery_state":actual,"state":if actual=="applying"{"applying"}else if last.is_some(){"refused"}else{"pending"}});
        if let Some((message, at)) = last {
            // Events produced here contain only a fixed code and fixed detail.
            // Foreign or manually corrupted diagnostics are never echoed.
            if let Some((code, _)) = message.split_once(": ")
                && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                && code.len() < 64
            {
                summary["last_attempt"] =
                    json!({"at":at,"code":code,"message":"task accepted, result not delivered"});
            }
        }
        summary
    }
    fn refusal(&self, id: TaskId, code: &str, paths: Vec<String>, persist: bool) -> Value {
        if persist && let Ok(storage) = self.storage() {
            let _=storage.connection().execute("INSERT INTO events(task_id,round_number,kind,message,created_at) VALUES (?1,NULL,'delivery_refused',?2,strftime('%Y-%m-%dT%H:%M:%fZ','now'))",rusqlite::params![id.to_string(),format!("{code}: task accepted, result not delivered")]);
        }
        let summary = self.delivery_summary(id);
        json!({"mode":"on_accept","state":"refused","code":code,"message":"result not delivered","paths":paths,"delivery_state":summary.get("delivery_state").cloned().unwrap_or(json!(if summary["state"]=="delivered"{"delivered"}else{"unknown"}))})
    }
    fn accepted_delivery_result(&self, task: &Task) -> Value {
        let result = bridge_delivery::automatic_admitted(&self.layout, &self.project, task.task_id);
        let delivery = match result {
            Ok(_) => self.delivery_summary(task.task_id),
            Err(e) => self.refusal(task.task_id, e.code, e.paths, true),
        };
        json!({"task_id":task.task_id.to_string(),"status":"accepted","delivery_mode":"on_accept","delivery":delivery})
    }
    pub(super) fn accept_on_accept(&self, task: &Task) -> Result<Value> {
        let id = task.task_id;
        if self.execution_mode(id)? != bridge_domain::ExecutionMode::Worktree {
            return Err("delivery_binding_invalid");
        }
        if task.status == TaskStatus::Accepted && self.delivery_summary(id)["state"] == "delivered"
        {
            return Ok(
                json!({"task_id":id.to_string(),"status":"accepted","delivery_mode":"on_accept","delivery":self.delivery_summary(id)}),
            );
        }
        let _fences =
            bridge_worker::admission::try_on_accept_fences(&self.layout, &self.project, id)
                .map_err(|_| "state_unavailable")?
                .ok_or("worker_running")?;
        let task = self.task(id)?.ok_or("unknown_task")?;
        if !matches!(
            task.status,
            TaskStatus::Accepted | TaskStatus::AwaitingReview
        ) || task.close_requested_at.is_some()
        {
            return Err("not_awaiting_review");
        }
        if task.status == TaskStatus::AwaitingReview {
            if task
                .snapshot
                .as_ref()
                .is_some_and(|s| s.get("automation_run_id").is_some())
            {
                return Err("automation_managed");
            }
            self.turn_idle(&task)?;
        }
        let _admission = match WorkerLock::try_acquire_admission(&self.layout)
            .map_err(|_| "state_unavailable")?
        {
            WorkerLockOutcome::Busy => {
                let mut result = json!({"task_id":id.to_string(),"status":task.status,"delivery_mode":"on_accept","delivery":self.refusal(id,"admission_lock_busy",vec![],task.status==TaskStatus::Accepted)});
                if task.status != TaskStatus::Accepted {
                    result["error"] = json!("admission_lock_busy");
                }
                return Ok(result);
            }
            WorkerLockOutcome::Acquired(g) => g,
        };
        if task.status == TaskStatus::AwaitingReview {
            let layouts = self.layouts()?;
            bridge_runtime::stop_worktree_server(
                &self.layout,
                &self.project,
                id,
                &layouts.iter().collect::<Vec<_>>(),
            )
            .map_err(|_| "worktree_server_stop_failed")?;
            let task = self
                .storage()?
                .accept_review_task(id, self.project.id())
                .map_err(|_| "accept_failed")?;
            Ok(self.accepted_delivery_result(&task))
        } else {
            Ok(self.accepted_delivery_result(&task))
        }
    }
}
