//! Final accepted cumulative artifact, then durable delivery or manual-ready.
use crate::{
    coordinator::{Coordinator, CoordinatorError, ReviewClient, Tick, task_id},
    run::{check_binding, check_config_binding},
};
use bridge_storage::{
    WorktreeDeliveryState,
    automation::{RunControl, RunStatus},
};
use serde_json::json;
impl<M: ReviewClient> Coordinator<M> {
    pub(crate) fn deliver(
        &mut self,
        fault: &mut impl FnMut(&str) -> bool,
    ) -> Result<Tick, CoordinatorError> {
        let run = self
            .store
            .load(Some(self.id))
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?;
        check_config_binding(&self.project, &self.layout, &run)
            .map_err(|_| CoordinatorError("automation_binding_changed"))?;
        let final_item = self.document["steps"]
            .as_array()
            .and_then(|s| s.last())
            .ok_or(CoordinatorError("automation_final_missing"))?
            .clone();
        if final_item["phase"] != "accepted" || final_item["review"]["decision"] != "accept" {
            return Err(CoordinatorError("automation_final_not_accepted"));
        }
        let id = task_id(&final_item)?;
        let task = self.task(id)?;
        if task.status != bridge_domain::TaskStatus::Accepted {
            return Err(CoordinatorError("automation_final_not_accepted"));
        }
        let evidence = bridge_worker::acceptance::acceptance_checks(
            &self.layout,
            &self.project,
            &task,
            &final_item["step"],
        )
        .map_err(CoordinatorError)?;
        if final_item["review"]["fingerprint"] != evidence.fingerprint
            || final_item["review"]["round"] != json!(evidence.round)
        {
            return Err(CoordinatorError("automation_final_review_stale"));
        }
        let record = self
            .layout
            .open_readonly()
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?
            .get_worktree(id, self.project.id())
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?
            .ok_or(CoordinatorError("automation_checkout_missing"))?;
        if record.delivery_state == Some(WorktreeDeliveryState::Delivered) {
            self.save(RunStatus::Completed)?;
            return Ok(Tick::Done);
        }
        if run.control() != RunControl::Run {
            return Err(CoordinatorError("automation_cancelled"));
        }
        if self.document["plan"]["delivery"] == "manual" {
            check_binding(&self.project, &self.layout, &run)
                .map_err(|_| CoordinatorError("automation_binding_changed"))?;
            bridge_delivery::dry_run(&self.layout, &self.project, id)
                .map_err(|e| CoordinatorError(e.code))?;
            self.save(RunStatus::Ready)?;
            return Ok(Tick::Done);
        }
        if self.document["plan"]["delivery"] != "apply" {
            return Err(CoordinatorError("automation_plan_invalid"));
        }
        match bridge_delivery::apply_with_fault(&self.layout, &self.project, id, |p| fault(p)) {
            Ok(_) => {}
            Err(e) if e.code == "simulated_crash" => {
                return Err(CoordinatorError("simulated_crash"));
            }
            Err(e) if e.code == "project_busy" => return Ok(Tick::Continue),
            Err(e) => return Err(CoordinatorError(e.code)),
        }
        if fault("after_delivery") {
            return Err(CoordinatorError("simulated_crash"));
        }
        let record = self
            .layout
            .open_readonly()
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?
            .get_worktree(id, self.project.id())
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?
            .ok_or(CoordinatorError("automation_checkout_missing"))?;
        if record.delivery_state != Some(WorktreeDeliveryState::Delivered) {
            return Err(CoordinatorError("automation_delivery_incomplete"));
        }
        self.save(RunStatus::Completed)?;
        Ok(Tick::Done)
    }
}
