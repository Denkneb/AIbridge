//! Combined current-session blockers with one bounded stale window.
use crate::{
    execution::ExecutionError,
    observation::{Observation, RoundObserver},
};
use bridge_config::{Endpoint, ProjectEntry};
use bridge_domain::{RoundStatus, TaskStatus};
use bridge_opencode::PermissionReply;
use serde_json::json;
use std::{path::Path, time::Duration};

impl RoundObserver<'_> {
    /// Message observation precedes blockers and deadline; questions are never
    /// answered. Allowed permissions get Once at most once per observer.
    /// Caller retains fences and supplies a monotonic clock from round start.
    /// # Errors
    /// Changed project/root/session/close refuses before any further HTTP/write.
    /// List failures keep observing like the reference; failed replies block.
    pub fn poll_with_blockers(
        &mut self,
        project: &ProjectEntry,
        clock: impl Fn() -> Duration,
        stale_grace: Duration,
    ) -> Result<Observation, ExecutionError> {
        let (_, task) = self.execution.task_and_root(self.layout)?;
        if project.id() != &task.project_id || project.workspace() != Path::new(&task.workspace) {
            return Err(ExecutionError::Binding);
        }
        let view = if let Some(port) = self.execution.server_port {
            project
                .execution_view(
                    &self.execution.root,
                    Endpoint::loopback(port).map_err(|_| ExecutionError::Binding)?,
                )
                .map_err(|_| ExecutionError::Binding)?
        } else {
            project.clone()
        };
        let pending = self.pending_blockers.clone();
        let observed = self.poll(&clock, &pending)?;
        if !matches!(observed, Observation::Pending) || !self.visible_history {
            return Ok(observed);
        }
        self.guard()?;
        let permissions = self.execution.client.list_permissions().unwrap_or_default();
        self.guard()?;
        let mut blockers = Vec::new();
        for permission in permissions
            .iter()
            .filter(|p| p.belongs_to_session(&self.session))
        {
            self.guard()?;
            let id = permission.id().unwrap_or("");
            if self.replied.contains(id) {
                continue;
            }
            let raw = json!({"id":permission.id(),"permission":permission.permission(),
                "patterns":permission.patterns(),"metadata":permission.metadata()});
            let decision =
                crate::auto_approval::permission_decision(&view, self.layout.state_root(), &raw);
            let mut blocker = json!({"type":"permission","permission":permission.permission(),
                "patterns":permission.patterns(),"reason":decision.reason()});
            if let Some(detail) = decision.detail() {
                blocker["detail"] = json!(detail);
            }
            if decision.approved() {
                self.guard()?;
                if self
                    .execution
                    .client
                    .reply_permission(id, PermissionReply::Once, None)
                    .is_ok()
                {
                    // Cache before the post-HTTP guard so a successful reply
                    // cannot repeat if persistence/close validation then fails.
                    self.replied.insert(id.to_owned());
                    self.approvals
                        .push(json!({"id":id,"permission":permission.permission()}));
                    self.guard()?;
                    continue;
                }
                self.guard()?;
                blocker["reason"] = json!("reply_failed");
                blocker
                    .as_object_mut()
                    .ok_or(ExecutionError::Round)?
                    .remove("detail");
            }
            blockers.push(blocker);
        }
        self.guard()?;
        let questions = self.execution.client.list_questions().unwrap_or_default();
        self.guard()?;
        blockers.extend(
            questions
                .iter()
                .filter(|q| q.belongs_to_session(&self.session))
                .map(|q| {
                    let blocker = q.blocker();
                    json!({"type":blocker.kind(),"text":blocker.text()})
                }),
        );
        let now = clock();
        self.record_time(now)?;
        if blockers.is_empty() {
            self.blockers_since = None;
            self.pending_blockers.clear();
            return Ok(Observation::Pending);
        }
        let since = *self.blockers_since.get_or_insert(now);
        self.pending_blockers = blockers;
        if now - since < stale_grace {
            return Ok(Observation::Pending);
        }
        let mut result = json!({"blockers":self.pending_blockers});
        if !self.approvals.is_empty() {
            result["auto_approved"] = json!(self.approvals);
        }
        self.finish(
            RoundStatus::NeedsUser,
            TaskStatus::NeedsUser,
            "needs_user",
            result,
        )
    }
}
