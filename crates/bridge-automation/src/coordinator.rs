//! Durable sequential FSM. Outbound intents precede idempotent MCP mutations.
use crate::{
    codex::{CodexClient, CodexError, Operation, validate_answer},
    plan::validate_plan,
    run::{AutomationLock, check_binding},
};
use bridge_config::ProjectEntry;
use bridge_domain::{TaskId, TaskStatus};
use bridge_mcp::{McpServer, WorkerSpawner};
use bridge_storage::{
    RustStateLayout, Task,
    automation::{AutomationRunStore, RunControl, RunId, RunStatus},
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoordinatorError(pub &'static str);
impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for CoordinatorError {}
type Result<T> = std::result::Result<T, CoordinatorError>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Continue,
    Paused,
    Blocked,
    Stopped,
    Done,
}
pub trait ReviewClient {
    fn call(
        &mut self,
        kind: Operation,
        workspace: &Path,
        context: &Value,
        timeout: Duration,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> std::result::Result<Value, CodexError>;
}
impl ReviewClient for CodexClient {
    fn call(
        &mut self,
        kind: Operation,
        workspace: &Path,
        context: &Value,
        timeout: Duration,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> std::result::Result<Value, CodexError> {
        self.set_timeout(timeout)?;
        CodexClient::call(self, kind, workspace, context, cancelled)
    }
}
pub struct Coordinator<M: ReviewClient> {
    _guard: AutomationLock,
    pub(crate) layout: RustStateLayout,
    pub(crate) project: ProjectEntry,
    pub(crate) store: AutomationRunStore,
    pub(crate) id: RunId,
    pub(crate) document: Value,
    server: McpServer,
    model: M,
    started: Instant,
    elapsed: f64,
}
impl<M: ReviewClient> Coordinator<M> {
    pub fn open(
        project: ProjectEntry,
        layout: RustStateLayout,
        id: RunId,
        spawner: WorkerSpawner,
        registry: Vec<ProjectEntry>,
        model: M,
    ) -> Result<Self> {
        let guard =
            AutomationLock::acquire(&layout).map_err(|_| CoordinatorError("automation_busy"))?;
        Self::with_guard(project, layout, id, spawner, registry, model, guard)
    }
    /// Private worker transfers its proven lifetime supervisor fence here.
    #[allow(clippy::too_many_arguments)]
    pub fn with_guard(
        project: ProjectEntry,
        layout: RustStateLayout,
        id: RunId,
        spawner: WorkerSpawner,
        registry: Vec<ProjectEntry>,
        model: M,
        guard: AutomationLock,
    ) -> Result<Self> {
        let store = AutomationRunStore::new(layout.clone());
        let run = store
            .load(Some(id))
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?;
        let doc = run.document().clone();
        validate_document(&project, &doc)?;
        let max = doc["plan"]["max_revisions"]
            .as_u64()
            .ok_or(CoordinatorError("automation_plan_invalid"))?;
        let view = project
            .automation_view(max)
            .map_err(|_| CoordinatorError("automation_plan_invalid"))?;
        let server = McpServer::internal(view, layout.clone(), spawner, registry)
            .map_err(|_| CoordinatorError("automation_service_unavailable"))?;
        let elapsed = doc["elapsed"]
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or(CoordinatorError("automation_state_corrupt"))?;
        Ok(Self {
            _guard: guard,
            layout,
            project,
            store,
            id,
            document: doc,
            server,
            model,
            started: Instant::now(),
            elapsed,
        })
    }
    pub fn document(&self) -> &Value {
        &self.document
    }
    pub(crate) fn save(&mut self, status: RunStatus) -> Result<()> {
        self.document["elapsed"] = json!(self.elapsed + self.started.elapsed().as_secs_f64());
        self.document = self
            .store
            .save(&self.document, status)
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?
            .document()
            .clone();
        Ok(())
    }
    fn task(&self, id: TaskId) -> Result<Task> {
        let task = self
            .layout
            .open_readonly()
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?
            .get_task(id)
            .map_err(|_| CoordinatorError("automation_task_invalid"))?
            .ok_or(CoordinatorError("automation_task_missing"))?;
        if task.project_id != *self.project.id()
            || Path::new(&task.workspace) != self.project.workspace()
        {
            return Err(CoordinatorError("automation_task_binding_changed"));
        }
        Ok(task)
    }
    pub(crate) fn execution_root(&self, id: TaskId) -> Result<PathBuf> {
        let task = self.task(id)?;
        let storage = self
            .layout
            .open_readonly()
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?;
        let record = storage
            .get_worktree(id, self.project.id())
            .map_err(|_| CoordinatorError("automation_checkout_invalid"))?
            .ok_or(CoordinatorError("automation_checkout_missing"))?;
        if record.status != bridge_storage::WorktreeStatus::Created
            || record.base_head != task.base_head
        {
            return Err(CoordinatorError("automation_checkout_invalid"));
        }
        let binding = bridge_git::checkout::probe_checkout(
            self.project.workspace(),
            &self.layout.project_dir(),
            id,
            Path::new(&record.path),
            task.base_head.as_deref(),
        )
        .map_err(|_| CoordinatorError("automation_checkout_invalid"))?;
        if record.runtime_dir.as_deref() != binding.paths.runtime_dir.to_str() {
            return Err(CoordinatorError("automation_checkout_invalid"));
        }
        Ok(binding.paths.checkout)
    }
    fn model_call(&mut self, kind: Operation, workspace: &Path, context: &Value) -> Result<Value> {
        for attempt in 0..3 {
            let remaining = self.document["plan"]["max_seconds"]
                .as_f64()
                .ok_or(CoordinatorError("automation_plan_invalid"))?
                - (self.elapsed + self.started.elapsed().as_secs_f64());
            if remaining <= 0.0 {
                return Err(CoordinatorError("automation_time_limit"));
            }
            let timeout = Duration::from_secs_f64(
                remaining.min(
                    self.document["plan"]["codex_timeout"]
                        .as_f64()
                        .ok_or(CoordinatorError("automation_plan_invalid"))?,
                ),
            );
            let store = self.store.clone();
            let id = self.id;
            let mut cancelled = move || {
                store.load(Some(id)).map_or(true, |r| {
                    r.control() != RunControl::Run || r.status() != RunStatus::Running
                })
            };
            let answer = self
                .model
                .call(kind, workspace, context, timeout, &mut cancelled)
                .and_then(|v| validate_answer(kind, v));
            if cancelled() {
                return Err(CoordinatorError("automation_cancelled"));
            }
            match answer {
                Ok(v) => return Ok(v),
                Err(CodexError::Cancelled) => return Err(CoordinatorError("automation_cancelled")),
                Err(_) if attempt < 2 => {
                    let deadline = Instant::now() + Duration::from_millis(100 * (1 << attempt));
                    while Instant::now() < deadline {
                        if cancelled() {
                            return Err(CoordinatorError("automation_cancelled"));
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                Err(_) => return Err(CoordinatorError("automation_model_failed")),
            }
        }
        Err(CoordinatorError("automation_model_failed"))
    }
    fn revision(&mut self, index: usize, findings: String) -> Result<()> {
        let revisions = self.document["steps"][index]["revisions"]
            .as_u64()
            .ok_or(CoordinatorError("automation_state_corrupt"))?;
        if revisions
            >= self.document["plan"]["max_revisions"]
                .as_u64()
                .ok_or(CoordinatorError("automation_plan_invalid"))?
        {
            return Err(CoordinatorError("automation_revision_limit"));
        }
        let step = self.document["steps"][index]["step"]["id"]
            .as_str()
            .ok_or(CoordinatorError("automation_state_corrupt"))?;
        self.document["steps"][index]["pending_revision"] = json!({"request_id":format!("auto:{}:{step}:rev:{}",self.id,revisions+1),"findings":findings});
        self.document["steps"][index]["phase"] = json!("revise");
        self.save(RunStatus::Running)
    }
    fn call(&self, name: &str, args: &Value) -> Result<Value> {
        let result = self.server.call_automation(name, args, self.id);
        if result.get("error").is_some() {
            return Err(CoordinatorError(match name {
                "submit_task" => "automation_submission_refused",
                "request_changes" => "automation_revision_refused",
                "accept_task" => "automation_acceptance_refused",
                "close_task" => "automation_close_refused",
                _ => "automation_status_unavailable",
            }));
        }
        Ok(result)
    }
    fn control(&mut self) -> Result<Option<Tick>> {
        let run = self
            .store
            .load(Some(self.id))
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?;
        if run.status().is_terminal() {
            return Ok(Some(Tick::Done));
        }
        match run.control() {
            RunControl::Pause => {
                self.save(RunStatus::Paused)?;
                Ok(Some(Tick::Paused))
            }
            RunControl::Stop => {
                let items = self.document["steps"]
                    .as_array()
                    .ok_or(CoordinatorError("automation_state_corrupt"))?
                    .clone();
                for item in items {
                    let known = item["task_id"].as_str().map(str::to_owned);
                    let step = item["step"]["id"]
                        .as_str()
                        .ok_or(CoordinatorError("automation_state_corrupt"))?;
                    let replay = self
                        .layout
                        .open_readonly()
                        .map_err(|_| CoordinatorError("automation_state_unavailable"))?
                        .connection()
                        .query_row(
                            "SELECT task_id FROM rounds WHERE request_id=?1 AND project_id=?2",
                            rusqlite::params![
                                format!("auto:{}:{step}:submit", self.id),
                                self.project.id().as_str()
                            ],
                            |r| r.get::<_, String>(0),
                        )
                        .optional()
                        .map_err(|_| CoordinatorError("automation_state_unavailable"))?;
                    if let Some(id) = known.or(replay) {
                        let id = id
                            .parse()
                            .map_err(|_| CoordinatorError("automation_task_invalid"))?;
                        let task = self.task(id)?;
                        if !task.status.is_terminal() {
                            self.call(
                                "close_task",
                                &json!({"task_id":id,"reason":"automatic run stopped"}),
                            )?;
                            if self.task(id)?.status != TaskStatus::Closed {
                                return Ok(Some(Tick::Continue));
                            }
                        }
                    }
                }
                self.save(RunStatus::Stopped)?;
                Ok(Some(Tick::Stopped))
            }
            RunControl::Run
                if run.status() == RunStatus::Blocked || run.status() == RunStatus::Paused =>
            {
                Ok(Some(Tick::Blocked))
            }
            _ => Ok(None),
        }
    }
    pub fn tick(&mut self) -> Result<Tick> {
        self.tick_with_fault(|_| false)
    }
    /// Fault hooks simulate abrupt process loss; the crash is never persisted
    /// as a blocker, so the next owner replays the already durable intent.
    pub fn tick_with_fault(&mut self, mut fault: impl FnMut(&str) -> bool) -> Result<Tick> {
        self.document = self
            .store
            .load(Some(self.id))
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?
            .document()
            .clone();
        if let Some(tick) = self.control()? {
            return Ok(tick);
        }
        let outcome = self.step(&mut fault);
        match outcome {
            Ok(tick) => Ok(tick),
            Err(e) if e.0 == "simulated_crash" => Err(e),
            Err(e) if e.0 == "automation_cancelled" => self
                .control()?
                .ok_or(CoordinatorError("automation_control_changed")),
            Err(e) => {
                self.document["blocker"] = json!({"code":e.0});
                self.save(RunStatus::Blocked)?;
                Ok(Tick::Blocked)
            }
        }
    }
    fn step(&mut self, fault: &mut impl FnMut(&str) -> bool) -> Result<Tick> {
        let run = self
            .store
            .load(Some(self.id))
            .map_err(|_| CoordinatorError("automation_state_unavailable"))?;
        if self.elapsed + self.started.elapsed().as_secs_f64()
            >= self.document["plan"]["max_seconds"]
                .as_f64()
                .ok_or(CoordinatorError("automation_plan_invalid"))?
        {
            return Err(CoordinatorError("automation_time_limit"));
        }
        if self.document["phase"] == "deliver" {
            return Err(CoordinatorError("automation_delivery_pending"));
        }
        check_binding(&self.project, &self.layout, &run)
            .map_err(|_| CoordinatorError("automation_binding_changed"))?;
        let index = self.document["index"]
            .as_u64()
            .ok_or(CoordinatorError("automation_state_corrupt"))? as usize;
        let item = self.document["steps"]
            .as_array()
            .and_then(|v| v.get(index))
            .ok_or(CoordinatorError("automation_state_corrupt"))?
            .clone();
        let step = &item["step"];
        let phase = item["phase"]
            .as_str()
            .ok_or(CoordinatorError("automation_state_corrupt"))?;
        match phase {
            "prepare" => {
                let workspace = if index == 0 {
                    self.project.workspace().to_path_buf()
                } else {
                    self.execution_root(task_id(&self.document["steps"][index - 1])?)?
                };
                let context = json!({"goal":self.document["plan"]["goal"],"approved_plan":self.document["plan"],"step":step,"accepted_steps":index});
                let answer = self.model_call(Operation::Prepare, &workspace, &context)?;
                self.document["steps"][index]["prepared_task"] = json!(format!(
                    "{}\n\nApproved step (authoritative):\n{}\nOnly modify this step's allowed_paths. Inherited accepted files outside that scope must remain byte-identical.",
                    answer["task"]
                        .as_str()
                        .ok_or(CoordinatorError("automation_model_invalid"))?,
                    step
                ));
                self.document["steps"][index]["phase"] = json!("submit");
                self.save(RunStatus::Running)?;
            }
            "submit" => {
                let scopes = self.document["steps"]
                    .as_array()
                    .ok_or(CoordinatorError("automation_state_corrupt"))?[..=index]
                    .iter()
                    .flat_map(|i| i["step"]["allowed_paths"].as_array().into_iter().flatten())
                    .map(|p| {
                        p.as_str()
                            .ok_or(CoordinatorError("automation_state_corrupt"))
                    })
                    .collect::<Result<std::collections::BTreeSet<_>>>()?;
                let mut args = json!({"request_id":format!("auto:{}:{}:submit",self.id,step["id"].as_str().ok_or(CoordinatorError("automation_state_corrupt"))?),"task":item["prepared_task"],"allowed_paths":scopes,"test_commands":step["test_commands"],"workflow_id":self.id.to_string(),"profile":step["profile"]});
                if index > 0 {
                    let parent = &self.document["steps"][index - 1];
                    let parent_id = task_id(parent)?;
                    args["depends_on"] =
                        json!([{"project_id":self.project.id(),"task_id":parent_id}]);
                    args["inherit_task_id"] = json!(parent_id);
                    args["inherit_fingerprint"] = parent["review"]["fingerprint"].clone();
                }
                let result = self.call("submit_task", &args)?;
                let id = result["task_id"]
                    .as_str()
                    .ok_or(CoordinatorError("automation_submission_invalid"))?;
                if fault("after_submit") {
                    return Err(CoordinatorError("simulated_crash"));
                }
                self.document["steps"][index]["task_id"] = json!(id);
                self.document["steps"][index]["phase"] = json!("active");
                self.save(RunStatus::Running)?;
            }
            "revise" => {
                let result=self.call("request_changes",&json!({"task_id":task_id(&item)?,"request_id":item["pending_revision"]["request_id"],"findings":item["pending_revision"]["findings"]}))?;
                if result["status"] == "needs_user" {
                    return Err(CoordinatorError("automation_revision_blocked"));
                }
                if fault("after_revision") {
                    return Err(CoordinatorError("simulated_crash"));
                }
                self.document["steps"][index]["revisions"] = json!(
                    item["revisions"]
                        .as_u64()
                        .ok_or(CoordinatorError("automation_state_corrupt"))?
                        + 1
                );
                self.document["steps"][index]["phase"] = json!("active");
                self.document["steps"][index]
                    .as_object_mut()
                    .ok_or(CoordinatorError("automation_state_corrupt"))?
                    .remove("review");
                self.save(RunStatus::Running)?;
            }
            "active" => {
                let id = task_id(&item)?;
                let result = self.call("task_status", &json!({"task_id":id,"wait_seconds":1}))?;
                if matches!(result["status"].as_str(), Some("implementing" | "revising")) {
                    self.save(RunStatus::Running)?;
                    return Ok(Tick::Continue);
                }
                if result["status"] != "awaiting_review" {
                    return Err(CoordinatorError("automation_task_blocked"));
                }
                let task = self.task(id)?;
                let evidence = match bridge_worker::acceptance::acceptance_checks(
                    &self.layout,
                    &self.project,
                    &task,
                    step,
                ) {
                    Ok(e) => e,
                    Err(code) => {
                        self.revision(
                            index,
                            format!("Automatic checks require correction: {code}"),
                        )?;
                        return Ok(Tick::Continue);
                    }
                };
                let context = json!({"goal":self.document["plan"]["goal"],"step":step,"approved_plan":self.document["plan"],"task_id":id,"round":evidence.round,"verification":evidence.verification,"baseline":evidence.baseline,"changes":evidence.changed_paths});
                let answer = self.model_call(Operation::Review, &evidence.root, &context)?;
                if bridge_artifact::fingerprint(
                    &bridge_git::take_snapshot(&evidence.root)
                        .map_err(|_| CoordinatorError("automation_checkout_invalid"))?,
                ) != evidence.fingerprint
                {
                    return Err(CoordinatorError("automation_review_changed_checkout"));
                }
                match answer["decision"].as_str() {
                    Some("blocked") => return Err(CoordinatorError("automation_review_blocked")),
                    Some("request_changes") => self.revision(
                        index,
                        format!(
                            "{}\n{}",
                            answer["summary"].as_str().unwrap(),
                            answer["findings"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|v| v.as_str().unwrap())
                                .collect::<Vec<_>>()
                                .join("\n")
                        ),
                    )?,
                    Some("accept") => {
                        let mut review = answer;
                        review["fingerprint"] = evidence.fingerprint;
                        review["round"] = json!(evidence.round);
                        self.document["steps"][index]["review"] = review;
                        self.document["steps"][index]["phase"] = json!("accept");
                        self.save(RunStatus::Running)?;
                    }
                    _ => return Err(CoordinatorError("automation_model_invalid")),
                }
            }
            "accept" => {
                let id = task_id(&item)?;
                let task = self.task(id)?;
                bridge_worker::acceptance::positive_review_gate(&self.layout, &self.project, &task)
                    .map_err(|_| CoordinatorError("automation_acceptance_refused"))?;
                self.call("accept_task", &json!({"task_id":id}))?;
                if fault("after_accept") {
                    return Err(CoordinatorError("simulated_crash"));
                }
                bridge_delivery::build(&self.layout, &self.project, id)
                    .map_err(|_| CoordinatorError("automation_artifact_failed"))?;
                self.document["steps"][index]["phase"] = json!("accepted");
                self.document["index"] = json!(index + 1);
                if index + 1
                    == self.document["steps"]
                        .as_array()
                        .ok_or(CoordinatorError("automation_state_corrupt"))?
                        .len()
                {
                    self.document["phase"] = json!("deliver");
                }
                self.save(RunStatus::Running)?;
            }
            _ => return Err(CoordinatorError("automation_phase_invalid")),
        }
        Ok(Tick::Continue)
    }
    pub fn run(&mut self) -> Result<Value> {
        loop {
            if self.tick()? != Tick::Continue {
                return Ok(self.document.clone());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}
pub(crate) fn task_id(item: &Value) -> Result<TaskId> {
    item["task_id"]
        .as_str()
        .ok_or(CoordinatorError("automation_task_missing"))?
        .parse()
        .map_err(|_| CoordinatorError("automation_task_invalid"))
}
fn validate_document(project: &ProjectEntry, doc: &Value) -> Result<()> {
    let plan = validate_plan(project, &doc["plan"])
        .map_err(|_| CoordinatorError("automation_plan_invalid"))?;
    let items = doc["steps"]
        .as_array()
        .ok_or(CoordinatorError("automation_state_corrupt"))?;
    if items.len() != plan.steps().len() + 1
        || doc["index"].as_u64().is_none_or(|i| i > items.len() as u64)
        || !matches!(doc["phase"].as_str(), Some("steps" | "deliver"))
    {
        return Err(CoordinatorError("automation_state_corrupt"));
    }
    let index = doc["index"].as_u64().unwrap() as usize;
    if (doc["phase"] == "deliver") != (index == items.len()) {
        return Err(CoordinatorError("automation_state_corrupt"));
    }
    for (position, item) in items.iter().enumerate() {
        if !matches!(
            item["phase"].as_str(),
            Some("prepare" | "submit" | "active" | "revise" | "accept" | "accepted")
        ) || item["revisions"]
            .as_u64()
            .is_none_or(|n| n > doc["plan"]["max_revisions"].as_u64().unwrap())
            || (position < index && item["phase"] != "accepted")
            || (position > index && item["phase"] != "prepare")
        {
            return Err(CoordinatorError("automation_state_corrupt"));
        }
    }
    for (item, step) in items.iter().zip(plan.steps()) {
        if item["step"] != json!(step)
            || !matches!(
                item["phase"].as_str(),
                Some("prepare" | "submit" | "active" | "revise" | "accept" | "accepted")
            )
            || item["revisions"].as_u64().is_none()
        {
            return Err(CoordinatorError("automation_state_corrupt"));
        }
    }
    let scopes = plan
        .steps()
        .iter()
        .flat_map(|s| s.allowed_paths.iter())
        .collect::<std::collections::BTreeSet<_>>();
    let final_step = &items
        .last()
        .ok_or(CoordinatorError("automation_state_corrupt"))?["step"];
    if final_step["id"] != "__final__"
        || final_step["allowed_paths"] != json!(scopes)
        || final_step["test_commands"] != json!(plan.final_test_commands())
        || final_step["profile"] != Value::Null
    {
        return Err(CoordinatorError("automation_state_corrupt"));
    }
    Ok(())
}
