//! Guarded observation and publication while borrowing execution fences.
//! Basic message polling accepts caller session proof and blocker state; the
//! verified loop combines identity checks, grace and configured Once replies.
//! Observation never sends a prompt or activates a task.
use crate::execution::{ExecutionError, FencedRoundExecution, RoundExecution};
use bridge_domain::{RoundKind, RoundStatus, TaskStatus};
use bridge_opencode::Message;
use bridge_storage::{FinishRoundInput, RoundRow, RoundUpdateOutcome, RustStateLayout};
use serde_json::{Value, json};
use std::{collections::HashSet, fmt, time::Duration};

/// A final assistant is only a candidate until verification and collection.
pub struct FinalResponse {
    owner: uuid::Uuid,
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
/// Borrowing the fenced context keeps its locks alive throughout observation.
pub struct RoundObserver<'a> {
    owner: uuid::Uuid,
    pub(crate) execution: &'a RoundExecution,
    pub(crate) layout: &'a RustStateLayout,
    pub(crate) session: String,
    outbound: String,
    grace: Duration,
    deadline: Duration,
    elapsed: Duration,
    delivered: bool,
    last_messages: Vec<Message>,
    pub(crate) done: bool,
    pub(crate) identity_verified: bool,
    pub(crate) workspace_verified: bool,
    pub(crate) visible_history: bool,
    pub(crate) pending_blockers: Vec<Value>,
    pub(crate) blockers_since: Option<Duration>,
    pub(crate) replied: HashSet<String>,
    pub(crate) approvals: Vec<Value>,
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
        execution: &'a FencedRoundExecution,
        layout: &'a RustStateLayout,
        delivery_grace: Duration,
        deadline: Duration,
    ) -> Result<Self, ExecutionError> {
        let execution = &execution.execution;
        if deadline.is_zero() {
            return Err(ExecutionError::Round);
        }
        let (_, row) = inspect(execution, layout)?;
        let observer = Self {
            owner: uuid::Uuid::new_v4(),
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
            identity_verified: false,
            workspace_verified: false,
            visible_history: false,
            pending_blockers: Vec::new(),
            blockers_since: None,
            replied: HashSet::new(),
            approvals: Vec::new(),
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
        self.visible_history = false;
        self.guard()?;
        let messages = self.execution.client.list_messages(&self.session);
        let row = self.guard()?;
        let elapsed = clock();
        self.record_time(elapsed)?;
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
            self.visible_history = true;
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
                owner: self.owner,
                response: (*last).clone(),
                messages: self.last_messages.clone(),
                result: json!({"tool_errors":tool_errors,"blockers":[]}),
            };
            self.done = true;
            return Ok(Observation::Final(Box::new(candidate)));
        }
        Ok(Observation::Pending)
    }
    /// Publishes a candidate from this observer only after saved verification
    /// and change collection. Failed/unsafe tests remain visible for review;
    /// they never imply acceptance. Caller retains the worker fences.
    /// # Errors
    /// Rejects foreign candidates, close/rebinding and missing baselines before
    /// verification. A collection failure
    /// leaves the persisted verifier reusable for retry without rerunning it.
    pub fn publish_final(
        &mut self,
        candidate: &FinalResponse,
        timeout: Duration,
        tail_bytes: usize,
    ) -> Result<RoundUpdateOutcome, ExecutionError> {
        if !self.done || candidate.owner != self.owner {
            return Err(ExecutionError::Round);
        }
        self.guard()?;
        self.execution
            .baseline_json()?
            .ok_or(ExecutionError::Baseline)?;
        let verification = self.execution.verify(self.layout, timeout, tail_bytes)?;
        self.guard()?;
        let changes = self.execution.collect_repositories(self.layout)?;
        let mut result = crate::completion::collection_json(&changes)?;
        for (key, value) in candidate.result.as_object().ok_or(ExecutionError::Round)? {
            result[key] = value.clone();
        }
        result["verification"] = serde_json::to_value(verification.verification())
            .map_err(|_| ExecutionError::Verifier)?;
        self.guard()?;
        self.execution.finish(
            self.layout,
            FinishRoundInput {
                round: self.execution.round().clone(),
                round_status: RoundStatus::Complete,
                task_status: TaskStatus::AwaitingReview,
                response_message_id: candidate.response.info().id().map(str::to_owned),
                response: Some(candidate.response.text()),
                error_code: None,
                result_json: Some(result),
            },
            &candidate.messages,
        )
    }
    pub(crate) fn deadline(&self) -> Duration {
        self.deadline
    }
    pub(crate) fn record_time(&mut self, elapsed: Duration) -> Result<(), ExecutionError> {
        if elapsed < self.elapsed {
            return Err(ExecutionError::Round);
        }
        self.elapsed = elapsed;
        Ok(())
    }
    pub(crate) fn promote_observing(&self) -> Result<(), ExecutionError> {
        if self.guard()?.status == RoundStatus::Sent {
            let (mut storage, _) = self.execution.task_and_root(self.layout)?;
            storage
                .mark_round_observing(self.execution.round().clone())
                .map_err(|_| ExecutionError::Storage)?;
        }
        self.guard()?;
        Ok(())
    }
    pub(crate) fn finish(
        &mut self,
        round_status: RoundStatus,
        task_status: TaskStatus,
        code: &str,
        result: Value,
    ) -> Result<Observation, ExecutionError> {
        self.promote_observing()?;
        let mut result = result;
        if self.execution.baseline_json()?.is_some() {
            let changes = self.execution.collect_repositories(self.layout)?;
            let mut collected = crate::completion::collection_json(&changes)?;
            for (key, value) in result.as_object().ok_or(ExecutionError::Round)? {
                collected[key] = value.clone();
            }
            result = collected;
        }
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
