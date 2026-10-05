//! Submit-time profile selection and atomic persistence (8.18).
//!
//! This service is shared by future MCP handlers. The caller owns workspace
//! collection, request normalization, workflow validation and worker startup.
//! No transport or executor is started here.
use bridge_config::ProjectEntry;
use bridge_domain::{ProfileDefinitionSource, TaskStatus, request_payload_hash};
use bridge_storage::{
    AdmissionSettings, CreateTaskError, CreateTaskInput, CreateTaskOutcome, StorageConnection,
    TaskBudget,
};
use serde_json::{Value, json};
use std::{error::Error, fmt};

/// Validated submission data plus the raw public profile argument.
pub struct ProfileSubmissionInput {
    pub task: CreateTaskInput,
    pub profile: Option<Value>,
    pub allow_dirty: bool,
    pub allow_commit: bool,
    pub budget: Option<TaskBudget>,
    pub initial_status: TaskStatus,
}

/// Safe errors; request/config data is never included in their rendering.
pub enum SubmissionError {
    InvalidProfile,
    UnknownProfile,
    ProjectMismatch,
    InvalidSettings,
    WorktreeUnsupported,
    Storage(CreateTaskError),
}
impl fmt::Display for SubmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidProfile => "invalid_profile",
            Self::UnknownProfile => "unknown_profile",
            Self::ProjectMismatch => "project_mismatch",
            Self::InvalidSettings => "invalid_admission_settings",
            Self::WorktreeUnsupported => "worktree_submission_unsupported",
            Self::Storage(_) => "submission_storage_error",
        })
    }
}
impl fmt::Debug for SubmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl Error for SubmissionError {}

/// Resolves once, hashes the effective identity and pins it in the create
/// transaction. Replay returns the original task without updating its profile.
/// Built-in implementer retains the pre-profile request hash, even when its
/// project model changes; its saved effective model still remains immutable.
///
/// # Errors
/// Rejects malformed/unknown profiles and mismatched project/workspace before
/// touching storage; admission and persistence errors propagate safely.
pub fn submit_task_with_profile(
    storage: &mut StorageConnection,
    project: &ProjectEntry,
    input: ProfileSubmissionInput,
) -> Result<CreateTaskOutcome, SubmissionError> {
    let raw_paths = input.task.allowed_paths.clone();
    submit_task_with_profile_raw_paths(storage, project, input, &raw_paths)
}

/// Keeps the request hash on raw public paths while persisting proven normalized
/// scopes. Caller validates these raw entries before invoking this service.
pub fn submit_task_with_profile_raw_paths(
    storage: &mut StorageConnection,
    project: &ProjectEntry,
    input: ProfileSubmissionInput,
    raw_paths: &[String],
) -> Result<CreateTaskOutcome, SubmissionError> {
    submit_task_with_workflow(
        storage,
        project,
        input,
        raw_paths,
        &bridge_domain::WorkflowMetadata::default(),
    )
}

/// Submission with structurally and referentially validated workflow metadata.
pub fn submit_task_with_workflow(
    storage: &mut StorageConnection,
    project: &ProjectEntry,
    mut input: ProfileSubmissionInput,
    raw_paths: &[String],
    workflow: &bridge_domain::WorkflowMetadata,
) -> Result<CreateTaskOutcome, SubmissionError> {
    if &input.task.project_id != project.id()
        || std::fs::canonicalize(&input.task.workspace).ok().as_deref() != Some(project.workspace())
    {
        return Err(SubmissionError::ProjectMismatch);
    }
    let requested = match input.profile.as_ref() {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if !id.is_empty() && id.trim() == id => Some(id.as_str()),
        _ => return Err(SubmissionError::InvalidProfile),
    };
    let profile = project
        .profile_snapshot(requested)
        .map_err(|_| SubmissionError::UnknownProfile)?;
    let mut payload = json!({
        "kind":"implement", "task":input.task.task,
        "allowed_paths":raw_paths, "test_commands":input.task.test_commands,
        "allow_dirty":input.allow_dirty, "allow_commit":input.allow_commit,
    });
    if let Some(budget) = &input.budget {
        payload["budget"] = budget.as_json().clone();
    }
    if profile.source != ProfileDefinitionSource::Builtin || profile.id != "implementer" {
        payload["profile"] = json!({"id":profile.id,"definition_hash":profile.definition_hash,"model":profile.model});
    }
    if workflow.workflow_id.is_some() {
        payload["workflow_id"] = json!(workflow.workflow_id);
    }
    if !workflow.depends_on.is_empty() {
        payload["depends_on"] = json!(workflow.depends_on);
    }
    if let Some(snapshot) = &input.task.snapshot {
        for key in [
            "automation_run_id",
            "automation_parent",
            "automation_parent_fingerprint",
        ] {
            if let Some(value) = snapshot.get(key) {
                payload[key] = value.clone();
            }
        }
    }
    input.task.payload_hash = request_payload_hash(&payload);
    let snapshot = input.task.snapshot.get_or_insert_with(|| json!({}));
    let object = snapshot
        .as_object_mut()
        .ok_or(SubmissionError::InvalidSettings)?;
    object.insert("allow_dirty".into(), Value::Bool(input.allow_dirty));
    object.insert("allow_commit".into(), Value::Bool(input.allow_commit));
    let settings = AdmissionSettings::new(
        project.max_active_tasks(),
        project.allow_parallel_writers(),
        project.execution_mode(),
    )
    .and_then(|settings| settings.with_delivery_mode(project.delivery_mode()))
    .map_err(|_| SubmissionError::InvalidSettings)?;
    if project.execution_mode() == bridge_domain::ExecutionMode::Worktree {
        let reject = SubmissionError::WorktreeUnsupported;
        if input.allow_dirty
            || input
                .task
                .allowed_paths
                .iter()
                .any(|p| std::path::Path::new(p).is_absolute())
            || input
                .task
                .snapshot
                .as_ref()
                .and_then(|v| v.get("external_repositories"))
                .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        {
            return Err(reject);
        }
        bridge_git::checkout::main_common_dir(project.workspace())
            .map_err(|_| SubmissionError::WorktreeUnsupported)?;
        bridge_git::checkout::check_worktree_scopes(project.workspace(), &input.task.allowed_paths)
            .map_err(|_| SubmissionError::WorktreeUnsupported)?;
        let base = bridge_git::head(project.workspace())
            .map_err(|_| SubmissionError::WorktreeUnsupported)?
            .ok_or(SubmissionError::WorktreeUnsupported)?
            .as_str()
            .to_owned();
        bridge_git::checkout::check_supported(project.workspace(), &base)
            .map_err(|_| SubmissionError::WorktreeUnsupported)?;
        let collected = bridge_git::take_snapshot(project.workspace())
            .map_err(|_| SubmissionError::WorktreeUnsupported)?;
        if !collected.dirty_paths().is_empty() {
            return Err(SubmissionError::WorktreeUnsupported);
        }
        let actual = collected
            .to_json()
            .map_err(|_| SubmissionError::WorktreeUnsupported)?;
        let object = input
            .task
            .snapshot
            .as_mut()
            .and_then(Value::as_object_mut)
            .ok_or(SubmissionError::InvalidSettings)?;
        for (key, value) in actual.as_object().ok_or(SubmissionError::InvalidSettings)? {
            object.insert(key.clone(), value.clone());
        }
        if input
            .task
            .base_head
            .as_ref()
            .is_some_and(|head| head != &base)
        {
            return Err(SubmissionError::WorktreeUnsupported);
        }
        input.task.base_head = Some(base.clone());
        let db = storage
            .connection()
            .path()
            .ok_or(SubmissionError::InvalidSettings)?;
        let project_dir = std::path::Path::new(db)
            .parent()
            .ok_or(SubmissionError::InvalidSettings)?;
        let root = project_dir
            .parent()
            .ok_or(SubmissionError::InvalidSettings)?;
        let layout = bridge_storage::RustStateLayout::new(root, project.id().clone())
            .map_err(|_| SubmissionError::InvalidSettings)?;
        layout
            .open()
            .map_err(|_| SubmissionError::InvalidSettings)?;
        let paths =
            bridge_git::checkout::CheckoutPaths::new(&layout.project_dir(), input.task.task_id)
                .map_err(|_| SubmissionError::WorktreeUnsupported)?;
        let checkout = bridge_storage::PendingCheckout {
            path: paths
                .checkout
                .to_str()
                .ok_or(SubmissionError::WorktreeUnsupported)?
                .into(),
            runtime_dir: paths
                .runtime_dir
                .to_str()
                .ok_or(SubmissionError::WorktreeUnsupported)?
                .into(),
            base_head: base,
        };
        return storage
            .create_task_with_workflow(
                input.task,
                &settings,
                input.initial_status,
                input.budget.as_ref(),
                &profile,
                Some(&checkout),
                workflow,
            )
            .map_err(SubmissionError::Storage);
    }
    storage
        .create_task_with_workflow(
            input.task,
            &settings,
            input.initial_status,
            input.budget.as_ref(),
            &profile,
            None,
            workflow,
        )
        .map_err(SubmissionError::Storage)
}
