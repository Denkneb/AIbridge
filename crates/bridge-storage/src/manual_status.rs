use super::{
    StorageConnection, current_round_number, load_task_for_update, utc_now_rfc3339_millis,
};
use bridge_domain::{ProjectId, TaskId, TaskStatus};
use rusqlite::{TransactionBehavior, params};

impl StorageConnection {
    /// Explicit status correction, bypassing ordinary domain transitions. Caller
    /// holds admission and worker fences. Execution evidence is left intact.
    pub fn set_task_status_manual(
        &mut self,
        id: TaskId,
        project: &ProjectId,
        expected: TaskStatus,
        target: TaskStatus,
        reason: &str,
    ) -> Result<(), &'static str> {
        if target.is_terminal() {
            return Err("terminal_status_forbidden");
        }
        if reason.trim().is_empty() || reason.len() > 2048 || reason.contains('\0') {
            return Err("invalid_status_reason");
        }
        let now = utc_now_rfc3339_millis();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "state_unavailable")?;
        let task = load_task_for_update(&tx, id).map_err(|_| "state_unavailable")?;
        if &task.project_id != project {
            return Err("task_binding_mismatch");
        }
        if task.status.is_terminal() {
            return Err("terminal_task_immutable");
        }
        if task.close_requested_at.is_some() {
            return Err("task_close_requested");
        }
        if task.status != expected {
            return Err("task_status_changed");
        }
        if task.status == target {
            return Ok(());
        }
        let number = current_round_number(&tx, id).map_err(|_| "state_unavailable")?;
        tx.execute(
            "UPDATE tasks SET status=?1,updated_at=?2 WHERE task_id=?3 AND project_id=?4",
            params![target.as_str(), now, id.to_string(), project.as_str()],
        )
        .map_err(|_| "state_unavailable")?;
        let message = serde_json::json!({"from":expected,"to":target,"reason":reason}).to_string();
        tx.execute("INSERT INTO events(task_id,round_number,kind,message,created_at) VALUES (?1,?2,'manual_status_change',?3,?4)",
            params![id.to_string(),number,message,now]).map_err(|_| "state_unavailable")?;
        tx.commit().map_err(|_| "state_unavailable")?;
        Ok(())
    }
}
