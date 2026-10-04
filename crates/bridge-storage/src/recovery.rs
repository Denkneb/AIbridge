//! Atomic observation-recovery claims. No new round or outbound message is made.
use super::{
    RoundRef, RoundRow, RoundUpdateError, StorageConnection, Task, current_round_number,
    read_round_update_outcome, utc_now_rfc3339_millis, validate_current_round,
};
use bridge_domain::{ProjectId, RoundKind, RoundStatus, TaskId, TaskStatus};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::fmt;

/// Opaque claimant identity. Event identity fences retries even in one clock ms.
#[derive(Clone)]
pub struct RecoveryClaim {
    reference: RoundRef,
    round: RoundRow,
    event_id: i64,
    previous_status: RoundStatus,
    lease: String,
}
impl RecoveryClaim {
    #[must_use]
    pub fn reference(&self) -> &RoundRef {
        &self.reference
    }
    #[must_use]
    pub fn round(&self) -> &RoundRow {
        &self.round
    }
    #[must_use]
    pub fn lease(&self) -> &str {
        &self.lease
    }
}
impl fmt::Debug for RecoveryClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryClaim { .. }")
    }
}
impl StorageConnection {
    /// Claims exactly the current open needs_user round for explicit recovery.
    /// An attempted parked round resumes observation; an unsent parked round
    /// becomes pending. Pending/sent rounds retain status.
    /// # Errors
    /// Corrupt task/round, invalid transition or SQLite failures roll back all rows.
    pub fn claim_needs_user_recovery(
        &mut self,
        id: TaskId,
        project: &ProjectId,
    ) -> Result<Option<RecoveryClaim>, RoundUpdateError> {
        let now = utc_now_rfc3339_millis();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;
        let task = tx
            .query_row(
                "SELECT * FROM tasks WHERE task_id=?1 AND project_id=?2",
                params![id.to_string(), project.as_str()],
                |r| Ok(Task::from_row(r)),
            )
            .optional()
            .map_err(RoundUpdateError::Database)?;
        let Some(task) = task else {
            return Ok(None);
        };
        let task = task.map_err(RoundUpdateError::TaskRow)?;
        if task.status != TaskStatus::NeedsUser || task.close_requested_at.is_some() {
            return Ok(None);
        }
        let number = match current_round_number(&tx, id) {
            Ok(n) => n,
            Err(RoundUpdateError::MissingRound) => return Ok(None),
            Err(e) => return Err(e),
        };
        let reference = RoundRef {
            task_id: id,
            project_id: project.clone(),
            round_number: number,
        };
        let (_, row) = validate_current_round(&tx, &reference)?;
        if !row.status.is_open() {
            return Ok(None);
        }
        let status = match row.kind {
            RoundKind::Implement => TaskStatus::Implementing,
            RoundKind::Revise => TaskStatus::Revising,
            _ => return Err(RoundUpdateError::InvalidPersistedState),
        };
        task.status
            .require_transition(status)
            .map_err(|_| RoundUpdateError::InvalidTaskTransition)?;
        let changed=tx.execute("UPDATE tasks SET status=?1,updated_at=?2 WHERE task_id=?3 AND project_id=?4 AND status='needs_user' AND close_requested_at IS NULL",
            params![status.as_str(),now,id.to_string(),project.as_str()]).map_err(RoundUpdateError::Database)?;
        if changed != 1 {
            return Ok(None);
        }
        let round_status = if row.status == RoundStatus::NeedsUser {
            if row.attempted {
                RoundStatus::Observing
            } else {
                RoundStatus::Pending
            }
        } else {
            row.status
        };
        tx.execute("UPDATE rounds SET status=?1,error_code=CASE WHEN status='needs_user' THEN NULL ELSE error_code END,worker_started_at=?2,updated_at=?2 WHERE task_id=?3 AND round_number=?4",
            params![round_status.as_str(),now,id.to_string(),number]).map_err(RoundUpdateError::Database)?;
        tx.execute("INSERT INTO events(task_id,round_number,kind,message,created_at) VALUES (?1,?2,'needs_user_recovery','needs_user round claimed for observation recovery',?3)",params![id.to_string(),number,now]).map_err(RoundUpdateError::Database)?;
        let event_id = tx.last_insert_rowid();
        let outcome = read_round_update_outcome(&tx, id, number)?;
        tx.commit().map_err(RoundUpdateError::Database)?;
        Ok(Some(RecoveryClaim {
            reference,
            round: outcome.round,
            event_id,
            previous_status: row.status,
            lease: now,
        }))
    }
    /// Rolls back a failed spawn only for this exact claim, before worker start.
    /// Repeated/stale release, new round, terminal task or pending close are no-ops.
    /// # Errors
    /// Corrupt persisted rows and SQLite failures fail closed atomically.
    pub fn release_needs_user_recovery(
        &mut self,
        claim: &RecoveryClaim,
    ) -> Result<bool, RoundUpdateError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;
        let reference = claim.reference();
        let (task, row) = match validate_current_round(&tx, reference) {
            Ok(value) => value,
            Err(
                RoundUpdateError::MissingTask
                | RoundUpdateError::MissingRound
                | RoundUpdateError::NotCurrentRound,
            ) => return Ok(false),
            Err(error) => return Err(error),
        };
        let latest:Option<i64>=tx.query_row("SELECT MAX(id) FROM events WHERE task_id=?1 AND round_number=?2 AND kind='needs_user_recovery'",
            params![reference.task_id.to_string(),reference.round_number],|r|r.get(0)).map_err(RoundUpdateError::Database)?;
        if latest != Some(claim.event_id)
            || row.worker_started_at.as_deref() != Some(claim.lease.as_str())
            || !matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising)
            || task.close_requested_at.is_some()
            || !row.status.is_open()
        {
            return Ok(false);
        }
        let now = utc_now_rfc3339_millis();
        let status = if claim.previous_status == RoundStatus::NeedsUser
            && (row.status == RoundStatus::Observing
                || (row.status == RoundStatus::Pending && !row.attempted))
        {
            RoundStatus::NeedsUser
        } else {
            row.status
        };
        tx.execute("UPDATE rounds SET status=?1,worker_started_at=NULL,worker_deadline_at=NULL,updated_at=?2 WHERE task_id=?3 AND round_number=?4",
            params![status.as_str(),now,reference.task_id.to_string(),reference.round_number]).map_err(RoundUpdateError::Database)?;
        tx.execute(
            "UPDATE tasks SET status='needs_user',updated_at=?1 WHERE task_id=?2 AND project_id=?3",
            params![
                now,
                reference.task_id.to_string(),
                reference.project_id.as_str()
            ],
        )
        .map_err(RoundUpdateError::Database)?;
        tx.execute("INSERT INTO events(task_id,round_number,kind,message,created_at) VALUES (?1,?2,'needs_user_recovery_failed','needs_user recovery spawn failed closed',?3)",
            params![reference.task_id.to_string(),reference.round_number,now]).map_err(RoundUpdateError::Database)?;
        tx.commit().map_err(RoundUpdateError::Database)?;
        Ok(true)
    }
}
