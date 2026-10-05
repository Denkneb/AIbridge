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
    previous_task_status: TaskStatus,
    previous_error_code: Option<String>,
    event_kind: &'static str,
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
        self.claim_observation_recovery(id, project, TaskStatus::NeedsUser)
    }
    /// Claims an attempted ambiguous delivery for observation, never redelivery.
    /// # Errors
    /// Corrupt identities and SQLite failures roll back the entire claim.
    pub fn claim_delivery_recovery(
        &mut self,
        id: TaskId,
        project: &ProjectId,
    ) -> Result<Option<RecoveryClaim>, RoundUpdateError> {
        self.claim_observation_recovery(id, project, TaskStatus::DeliveryUnknown)
    }
    fn claim_observation_recovery(
        &mut self,
        id: TaskId,
        project: &ProjectId,
        parked: TaskStatus,
    ) -> Result<Option<RecoveryClaim>, RoundUpdateError> {
        let event_kind = match parked {
            TaskStatus::NeedsUser => "needs_user_recovery",
            TaskStatus::DeliveryUnknown => "delivery_recovery",
            _ => return Err(RoundUpdateError::InvalidPersistedState),
        };
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
        if task.status != parked || task.close_requested_at.is_some() {
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
        if parked == TaskStatus::DeliveryUnknown
            && (!row.attempted
                || row.status != RoundStatus::DeliveryUnknown
                || row.session_id.is_none()
                || row.outbound_message_id.is_none())
        {
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
        let changed=tx.execute("UPDATE tasks SET status=?1,updated_at=?2 WHERE task_id=?3 AND project_id=?4 AND status=?5 AND close_requested_at IS NULL",
            params![status.as_str(),now,id.to_string(),project.as_str(),parked.as_str()]).map_err(RoundUpdateError::Database)?;
        if changed != 1 {
            return Ok(None);
        }
        let round_status = if parked == TaskStatus::DeliveryUnknown {
            RoundStatus::Observing
        } else if row.status == RoundStatus::NeedsUser {
            if row.attempted {
                RoundStatus::Observing
            } else {
                RoundStatus::Pending
            }
        } else {
            row.status
        };
        tx.execute("UPDATE rounds SET status=?1,error_code=CASE WHEN status IN ('needs_user','delivery_unknown') THEN NULL ELSE error_code END,worker_started_at=?2,updated_at=?2 WHERE task_id=?3 AND round_number=?4",
            params![round_status.as_str(),now,id.to_string(),number]).map_err(RoundUpdateError::Database)?;
        tx.execute("INSERT INTO events(task_id,round_number,kind,message,created_at) VALUES (?1,?2,?3,'round claimed for observation recovery',?4)",params![id.to_string(),number,event_kind,now]).map_err(RoundUpdateError::Database)?;
        let event_id = tx.last_insert_rowid();
        let outcome = read_round_update_outcome(&tx, id, number)?;
        tx.commit().map_err(RoundUpdateError::Database)?;
        Ok(Some(RecoveryClaim {
            reference,
            round: outcome.round,
            event_id,
            previous_status: row.status,
            previous_task_status: parked,
            previous_error_code: row.error_code,
            event_kind,
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
        self.release_observation_recovery(claim)
    }
    /// Releases this exact observation claim if no worker has started.
    /// # Errors
    /// Storage corruption or write failure rolls back all changes.
    pub fn release_observation_recovery(
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
        let latest: Option<i64> = tx
            .query_row(
                "SELECT MAX(id) FROM events WHERE task_id=?1 AND round_number=?2 AND kind IN ('needs_user_recovery','delivery_recovery')",
                params![
                    reference.task_id.to_string(),
                    reference.round_number
                ],
                |r| r.get(0),
            )
            .map_err(RoundUpdateError::Database)?;
        if latest != Some(claim.event_id)
            || row.worker_started_at.as_deref() != Some(claim.lease.as_str())
            || !matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising)
            || task.close_requested_at.is_some()
            || !row.status.is_open()
        {
            return Ok(false);
        }
        let now = utc_now_rfc3339_millis();
        let status = if claim.previous_task_status == TaskStatus::DeliveryUnknown {
            claim.previous_status
        } else if claim.previous_status == RoundStatus::NeedsUser
            && (row.status == RoundStatus::Observing
                || (row.status == RoundStatus::Pending && !row.attempted))
        {
            RoundStatus::NeedsUser
        } else {
            row.status
        };
        tx.execute("UPDATE rounds SET status=?1,worker_started_at=NULL,worker_deadline_at=NULL,updated_at=?2,error_code=?5 WHERE task_id=?3 AND round_number=?4",
            params![status.as_str(),now,reference.task_id.to_string(),reference.round_number,claim.previous_error_code]).map_err(RoundUpdateError::Database)?;
        tx.execute(
            "UPDATE tasks SET status=?4,updated_at=?1 WHERE task_id=?2 AND project_id=?3",
            params![
                now,
                reference.task_id.to_string(),
                reference.project_id.as_str(),
                claim.previous_task_status.as_str()
            ],
        )
        .map_err(RoundUpdateError::Database)?;
        tx.execute("INSERT INTO events(task_id,round_number,kind,message,created_at) VALUES (?1,?2,?3,'recovery spawn failed closed',?4)",
            params![reference.task_id.to_string(),reference.round_number,format!("{}_failed",claim.event_kind),now]).map_err(RoundUpdateError::Database)?;
        tx.commit().map_err(RoundUpdateError::Database)?;
        Ok(true)
    }
}
