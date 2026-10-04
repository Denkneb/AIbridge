//! Identity-checked observation, with one deadline budget and no prompt send.
use crate::{
    execution::ExecutionError,
    observation::{Observation, RoundObserver},
};
use bridge_config::ProjectEntry;
use bridge_domain::{RoundStatus, TaskStatus};
use bridge_opencode::{SessionError, TransportError};
use bridge_storage::RoundUpdateOutcome;
use serde_json::json;
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// Cadence and final-publication limits; delivery/deadline belong to observer.
#[derive(Debug, Clone, Copy)]
pub struct ObservationSettings {
    pub poll_interval: Duration,
    pub stale_blocker_grace: Duration,
    pub verification_timeout: Duration,
    pub verification_tail_bytes: usize,
}
impl Default for ObservationSettings {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(1500),
            stale_blocker_grace: Duration::from_secs(15),
            verification_timeout: Duration::from_secs(120),
            verification_tail_bytes: 4096,
        }
    }
}
impl RoundObserver<'_> {
    /// Verifies unscoped server root and the saved session before observing.
    /// Transient session failures consume the same deadline as messages; no
    /// missing-history delivery conclusion is made while session checks retry.
    /// # Errors
    /// Refuses close/rebinding/foreign configuration before further HTTP/write.
    pub fn poll_verified(
        &mut self,
        project: &ProjectEntry,
        clock: impl Fn() -> Duration,
        stale_grace: Duration,
    ) -> Result<Observation, ExecutionError> {
        if self.done {
            return Err(ExecutionError::Round);
        }
        let (_, task) = self.execution.task_and_root(self.layout)?;
        if project.id() != &task.project_id || project.workspace() != Path::new(&task.workspace) {
            return Err(ExecutionError::Binding);
        }
        if !self.workspace_verified {
            self.guard()?;
            let identity = self.execution.client.verify_workspace();
            self.guard()?;
            self.record_time(clock())?;
            if identity.is_err() {
                return self.finish(
                    RoundStatus::Failed,
                    TaskStatus::Failed,
                    "workspace_mismatch",
                    json!({"error":"server workspace could not be verified"}),
                );
            }
            self.workspace_verified = true;
        }
        if !self.identity_verified {
            self.guard()?;
            let session = self.execution.client.get_session(&self.session);
            self.guard()?;
            let elapsed = clock();
            self.record_time(elapsed)?;
            match session {
                Err(SessionError::Transport(TransportError::NotFound)) => {
                    return self.finish(
                        RoundStatus::Failed,
                        TaskStatus::Failed,
                        "session_not_found",
                        json!({"error":"saved session was not found"}),
                    );
                }
                Ok(session) => {
                    // Require the SDK identity fields even though older Python
                    // accepted an absent directory. A damaged body proves no root.
                    if session.id() != Some(&self.session)
                        || !session.directory().is_some_and(|d| {
                            crate::session::directory_matches_workspace(d, &self.execution.root)
                        })
                    {
                        return self.finish(
                            RoundStatus::Failed,
                            TaskStatus::Failed,
                            "session_directory_mismatch",
                            json!({"error":"saved session identity could not be verified"}),
                        );
                    }
                    self.identity_verified = true;
                }
                Err(_) if elapsed > self.deadline() => {
                    return self.finish(RoundStatus::NeedsUser, TaskStatus::NeedsUser,
                        "transient_error", json!({"blockers":[{"type":"transient_error",
                        "detail":"saved session is temporarily unavailable; server execution may continue"}]}));
                }
                Err(_) => return Ok(Observation::Pending),
            }
        }
        self.poll_with_blockers(project, clock, stale_grace)
    }

    /// Runs an unused observer to a terminal result, including saved verifier
    /// publication. Caller retains execution fences and handles close cleanup
    /// after this method returns and the fenced context is dropped.
    /// # Errors
    /// Zero cadence/verification timeout and all observer guards fail closed.
    pub fn observe_to_completion(
        &mut self,
        project: &ProjectEntry,
        settings: ObservationSettings,
    ) -> Result<RoundUpdateOutcome, ExecutionError> {
        self.observe_from(project, settings, Instant::now())
    }
    pub(crate) fn observe_from(
        &mut self,
        project: &ProjectEntry,
        settings: ObservationSettings,
        start: Instant,
    ) -> Result<RoundUpdateOutcome, ExecutionError> {
        if settings.poll_interval.is_zero() || settings.verification_timeout.is_zero() {
            return Err(ExecutionError::Round);
        }
        loop {
            match self.poll_verified(project, || start.elapsed(), settings.stale_blocker_grace)? {
                Observation::Final(candidate) => {
                    return self.publish_final(
                        &candidate,
                        settings.verification_timeout,
                        settings.verification_tail_bytes,
                    );
                }
                Observation::Finished(outcome) => return Ok(*outcome),
                Observation::Pending => {}
            }
            let pause = Instant::now();
            while pause.elapsed() < settings.poll_interval {
                self.guard()?;
                thread::sleep(
                    settings
                        .poll_interval
                        .saturating_sub(pause.elapsed())
                        .min(Duration::from_millis(100)),
                );
            }
        }
    }
}
