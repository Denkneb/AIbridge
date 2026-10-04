//! One guarded message poll. The caller owns worker fences, session identity
//! proof, monotonic start time, sleep cadence and permission/question grace.
//! This service never sends a prompt, replies to a blocker or activates a task.
use crate::execution::{ExecutionError, RoundExecution};
use bridge_domain::{RoundKind, RoundStatus, TaskStatus};
use bridge_opencode::Message;
use bridge_storage::{FinishRoundInput, RoundRow, RoundUpdateOutcome, RustStateLayout};
use serde_json::{Value, json};
use std::{collections::HashSet, fmt, time::Duration};

/// A final assistant is only a candidate until verification and collection.
pub struct FinalResponse {
    pub(crate) messages: Vec<Message>,
    pub(crate) response: Message,
    pub(crate) result: Value,
}
impl FinalResponse {
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
    pub fn response(&self) -> &Message {
        &self.response
    }
    pub fn result(&self) -> &Value {
        &self.result
    }
}
impl fmt::Debug for FinalResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FinalResponse { .. }")
    }
}
/// Poll results contain no assistant content in diagnostics.
pub enum Observation {
    Pending,
    Final(Box<FinalResponse>),
    Finished(Box<RoundUpdateOutcome>),
}
impl fmt::Debug for Observation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pending => "Observation::Pending",
            Self::Final(_) => "Observation::Final { .. }",
            Self::Finished(_) => "Observation::Finished { .. }",
        })
    }
}
/// Bound to one immutable execution/client/layout and persisted session/prompt.
/// Keep the corresponding worker fences alive for the entire observer lifetime.
pub struct RoundObserver<'a> {
    pub(crate) execution: &'a RoundExecution,
    pub(crate) layout: &'a RustStateLayout,
    session: String,
    outbound: String,
    grace: Duration,
    deadline: Duration,
    elapsed: Duration,
    delivered: bool,
    last_messages: Vec<Message>,
    done: bool,
}
impl fmt::Debug for RoundObserver<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoundObserver { .. }")
    }
}
impl<'a> RoundObserver<'a> {
    /// # Errors
    /// Rejects stale, closed, unattempted or unbound rounds before any HTTP.
    pub fn new(
        execution: &'a RoundExecution,
        layout: &'a RustStateLayout,
        delivery_grace: Duration,
        deadline: Duration,
    ) -> Result<Self, ExecutionError> {
        if deadline.is_zero() {
            return Err(ExecutionError::Round);
        }
        let (_, row) = inspect(execution, layout)?;
        let observer = Self {
            execution,
            layout,
            session: row.session_id.ok_or(ExecutionError::Round)?,
            outbound: row.outbound_message_id.ok_or(ExecutionError::Round)?,
            grace: delivery_grace,
            deadline,
            elapsed: Duration::ZERO,
            delivered: false,
            last_messages: Vec::new(),
            done: false,
        };
        observer.guard()?;
        Ok(observer)
    }
    pub(crate) fn guard(&self) -> Result<RoundRow, ExecutionError> {
        let (_, row) = inspect(self.execution, self.layout)?;
        if row.session_id.as_deref() != Some(&self.session)
            || row.outbound_message_id.as_deref() != Some(&self.outbound)
        {
            return Err(ExecutionError::Round);
        }
        Ok(row)
    }
    /// One GET, then guards again and samples total elapsed time. The clock
    /// must measure from the round start, including session checks and HTTP.
    /// Pending blockers are supplied by the caller's current-session grace
    /// tracker; at deadline they take precedence over the generic blocker.
    /// # Errors
    /// Binding/close changes and a decreasing clock fail closed. Transport
    /// failures remain observable until grace/deadline, with last good usage.
    pub fn poll(
        &mut self,
        clock: impl FnOnce() -> Duration,
        pending_blockers: &[Value],
    ) -> Result<Observation, ExecutionError> {
        if self.done {
            return Err(ExecutionError::Round);
        }
        self.guard()?;
        let messages = self.execution.client.list_messages(&self.session);
        let row = self.guard()?;
        let elapsed = clock();
        if elapsed < self.elapsed {
            return Err(ExecutionError::Round);
        }
        self.elapsed = elapsed;
        if messages.as_ref().is_ok_and(|history| {
            history
                .iter()
                .any(|m| m.info().session_id().is_some_and(|s| s != self.session))
        }) {
            return Err(ExecutionError::Binding);
        }
        // Sent is an ambiguous POST outcome. Enter observation without a resend.
        if row.status == RoundStatus::Sent {
            let (mut storage, _) = self.execution.task_and_root(self.layout)?;
            storage
                .mark_round_observing(self.execution.round().clone())
                .map_err(|_| ExecutionError::Storage)?;
        }
        let has_outbound = messages.as_ref().is_ok_and(|history| {
            history
                .iter()
                .any(|m| m.is_user() && m.info().id() == Some(&self.outbound))
        });
        if has_outbound {
            self.delivered = true;
            self.last_messages = messages.map_err(|_| ExecutionError::Runtime)?;
        }
        // Delivery evidence precedes deadline, even when both windows elapsed.
        if !has_outbound && !self.delivered && elapsed > self.grace {
            return self.finish(
                RoundStatus::DeliveryUnknown,
                TaskStatus::DeliveryUnknown,
                "delivery_unknown",
                json!({"error":"prompt delivery could not be confirmed"}),
            );
        }
        if elapsed > self.deadline {
            let blockers = if pending_blockers.is_empty() {
                json!([{"type":"deadline","detail":"round deadline reached; server execution may continue"}])
            } else {
                json!(pending_blockers)
            };
            return self.finish(
                RoundStatus::NeedsUser,
                TaskStatus::NeedsUser,
                if pending_blockers.is_empty() {
                    "deadline"
                } else {
                    "needs_user"
                },
                json!({"blockers":blockers}),
            );
        }
        if !has_outbound {
            return Ok(Observation::Pending);
        }
        let (users, assistants) = round_messages(&self.last_messages, &self.outbound);
        let Some(last) = assistants.last() else {
            return Ok(Observation::Pending);
        };
        if last.has_error() {
            let parent = last.info().parent_id();
            let later_user = users
                .iter()
                .position(|u| Some(*u) == parent)
                .is_some_and(|i| i + 1 < users.len());
            if !later_user {
                // Keep diagnostics fixed; raw server errors may contain secrets.
                return self.finish(
                    RoundStatus::Failed,
                    TaskStatus::Failed,
                    "assistant_error",
                    json!({"error":"assistant execution failed"}),
                );
            }
        }
        if is_final(last) {
            let tool_errors: Vec<Value> = assistants
                .iter()
                .flat_map(|m| m.parts())
                .filter(|p| p.is_tool() && p.tool_status() == Some("error"))
                .map(|_| json!("tool execution failed"))
                .collect();
            let candidate = FinalResponse {
                response: (*last).clone(),
                messages: self.last_messages.clone(),
                result: json!({"tool_errors":tool_errors,"blockers":[]}),
            };
            self.done = true;
            return Ok(Observation::Final(Box::new(candidate)));
        }
        Ok(Observation::Pending)
    }
    fn finish(
        &mut self,
        round_status: RoundStatus,
        task_status: TaskStatus,
        code: &str,
        result: Value,
    ) -> Result<Observation, ExecutionError> {
        self.guard()?;
        let outcome = self.execution.finish(
            self.layout,
            FinishRoundInput {
                round: self.execution.round().clone(),
                round_status,
                task_status,
                response_message_id: None,
                response: None,
                error_code: Some(code.to_owned()),
                result_json: Some(result),
            },
            &self.last_messages,
        )?;
        self.done = true;
        Ok(Observation::Finished(Box::new(outcome)))
    }
}
fn inspect(
    execution: &RoundExecution,
    layout: &RustStateLayout,
) -> Result<(bridge_storage::StorageConnection, RoundRow), ExecutionError> {
    let (storage, task) = execution.task_and_root(layout)?;
    let round = execution.round();
    let row = storage
        .connection()
        .query_row(
            "SELECT * FROM rounds WHERE task_id=?1 AND round_number=?2",
            rusqlite::params![round.task_id.to_string(), round.round_number],
            |r| Ok(RoundRow::from_row(r)),
        )
        .map_err(|_| ExecutionError::Storage)?
        .map_err(|_| ExecutionError::Round)?;
    if row.project_id != round.project_id
        || task.close_requested_at.is_some()
        || !row.attempted
        || !matches!(
            row.status,
            RoundStatus::Sent
                | RoundStatus::Observing
                | RoundStatus::NeedsUser
                | RoundStatus::DeliveryUnknown
        )
        || !matches!(
            (row.kind, task.status),
            (RoundKind::Implement, TaskStatus::Implementing)
                | (RoundKind::Revise, TaskStatus::Revising)
                | (_, TaskStatus::NeedsUser)
                | (_, TaskStatus::DeliveryUnknown)
        )
        || row.session_id.as_deref().is_none_or(str::is_empty)
        || row.outbound_message_id.as_deref().is_none_or(str::is_empty)
        || execution.client.workspace() != execution.root
    {
        return Err(ExecutionError::Round);
    }
    Ok((storage, row))
}
fn round_messages<'a>(messages: &'a [Message], outbound: &str) -> (Vec<&'a str>, Vec<&'a Message>) {
    let users: Vec<_> = messages
        .iter()
        .filter(|m| m.is_user())
        .skip_while(|m| m.info().id() != Some(outbound))
        .filter_map(|m| m.info().id())
        .collect();
    let ids: HashSet<_> = users.iter().copied().collect();
    let assistants = messages
        .iter()
        .filter(|m| m.is_assistant() && m.info().parent_id().is_some_and(|p| ids.contains(p)))
        .collect();
    (users, assistants)
}
fn is_final(message: &Message) -> bool {
    message.is_assistant()
        && !message.has_error()
        && message.is_completed()
        && message
            .info()
            .finish()
            .is_some_and(|f| !f.is_empty() && f != "tool-calls" && f != "unknown")
        && !message.has_pending_tool_parts()
}
