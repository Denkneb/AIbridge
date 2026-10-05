//! Fixed-shape verifier progress. Completion payloads remain typed and immutable.
use crate::{
    RoundRef, RoundUpdateError, StorageConnection, utc_now_rfc3339_millis, validate_current_round,
};
use bridge_domain::{TaskStatus, VerifierState};
use rusqlite::{TransactionBehavior, params};
use serde_json::{Value, json};
impl StorageConnection {
    /// Persists a command counter before the command runs, under current-round guards.
    /// # Errors
    /// Refuses completed/stale/closed rounds and invalid counter bounds.
    pub fn save_verifier_progress(
        &mut self,
        round: &RoundRef,
        index: u64,
        count: u64,
    ) -> Result<(), RoundUpdateError> {
        if index > count {
            return Err(RoundUpdateError::InvalidInput);
        }
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(RoundUpdateError::Database)?;
        let (task, row) = validate_current_round(&tx, round)?;
        if row.verifier_state != Some(VerifierState::Running)
            || task.close_requested_at.is_some()
            || !matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising)
        {
            return Err(RoundUpdateError::InvalidPersistedState);
        }
        tx.execute(
            "UPDATE rounds SET verifier_json=?1,updated_at=?2 WHERE task_id=?3 AND round_number=?4",
            params![
                json!({"state":"running","command_index":index,"command_count":count}).to_string(),
                utc_now_rfc3339_millis(),
                round.task_id.to_string(),
                round.round_number
            ],
        )
        .map_err(RoundUpdateError::Database)?;
        tx.commit().map_err(RoundUpdateError::Database)
    }
    /// Returns only safe counters; no command text, logs or arbitrary persisted strings.
    /// # Errors
    /// Ownership/SQL/current-round validation errors remain fixed categories.
    pub fn verifier_progress(
        &self,
        round: &RoundRef,
        fallback_count: u64,
    ) -> Result<Option<Value>, RoundUpdateError> {
        let (_, row) = validate_current_round(self.connection(), round)?;
        if row.verifier_state != Some(VerifierState::Running) {
            return Ok(None);
        }
        let raw: Option<String> = self
            .connection()
            .query_row(
                "SELECT verifier_json FROM rounds WHERE task_id=?1 AND round_number=?2",
                params![round.task_id.to_string(), round.round_number],
                |r| r.get(0),
            )
            .map_err(RoundUpdateError::Database)?;
        let v = raw
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(Value::Null);
        let count = v["command_count"]
            .as_u64()
            .filter(|n| *n > 0)
            .unwrap_or(fallback_count);
        Ok(Some(
            json!({"state":"running","command_index":v["command_index"].as_u64().unwrap_or(0).min(count),"command_count":count}),
        ))
    }
}
