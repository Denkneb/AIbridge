//! Atomic review transition and exact-lease rollback for MCP worker spawning.
use super::*;

impl StorageConnection {
    /// Caller holds task/review fences and proved the server turn idle. Frozen
    /// manual delivery only; accept/event/reservation release commit together.
    pub fn accept_manual_task(
        &mut self,
        id: TaskId,
        project: &ProjectId,
    ) -> Result<Task, RoundUpdateError> {
        let now = utc_now_rfc3339_millis();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;
        let task = load_task_for_update(&tx, id)?;
        if &task.project_id != project {
            return Err(RoundUpdateError::ProjectMismatch);
        }
        if task.delivery_mode != bridge_domain::DeliveryMode::Manual
            || task.close_requested_at.is_some()
        {
            return Err(RoundUpdateError::InvalidTaskTransition);
        }
        if task.status == TaskStatus::Accepted {
            return Ok(task);
        }
        if task.status != TaskStatus::AwaitingReview {
            return Err(RoundUpdateError::InvalidTaskTransition);
        }
        let number = current_round_number(&tx, id)?;
        let (_, round) = validate_current_round(
            &tx,
            &RoundRef {
                task_id: id,
                project_id: project.clone(),
                round_number: number,
            },
        )?;
        if round.status != RoundStatus::Complete {
            return Err(RoundUpdateError::InvalidPersistedState);
        }
        task.status
            .require_transition(TaskStatus::Accepted)
            .map_err(|_| RoundUpdateError::InvalidTaskTransition)?;
        tx.execute(
            "UPDATE tasks SET status='accepted',updated_at=?1 WHERE task_id=?2",
            params![now, id.to_string()],
        )
        .map_err(RoundUpdateError::Database)?;
        writers::release_terminal(&tx, id, TaskStatus::Accepted)
            .map_err(RoundUpdateError::Database)?;
        tx.execute("INSERT INTO events(task_id,round_number,kind,message,created_at) VALUES (?1,NULL,'accepted','task accepted by reviewer',?2)",params![id.to_string(),now])
            .map_err(RoundUpdateError::Database)?;
        let task = load_task_for_update(&tx, id)?;
        tx.commit().map_err(RoundUpdateError::Database)?;
        Ok(task)
    }

    /// Release only the failed parent's spawn lease, before the child worker
    /// overwrites it. Close/stale/current/status guards prevent stale rollback.
    pub fn release_worker_spawn(
        &mut self,
        reference: &RoundRef,
        lease: &str,
    ) -> Result<bool, RoundUpdateError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;
        let (task, row) = validate_current_round(&tx, reference)?;
        if task.close_requested_at.is_some()
            || !matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising)
            || !row.status.is_open()
            || row.worker_started_at.as_deref() != Some(lease)
        {
            return Ok(false);
        }
        tx.execute("UPDATE rounds SET worker_started_at=NULL,worker_deadline_at=NULL WHERE task_id=?1 AND round_number=?2",params![reference.task_id.to_string(),reference.round_number])
            .map_err(RoundUpdateError::Database)?;
        tx.commit().map_err(RoundUpdateError::Database)?;
        Ok(true)
    }
}
