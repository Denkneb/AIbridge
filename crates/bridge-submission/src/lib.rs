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
    Storage(CreateTaskError),
}
impl fmt::Display for SubmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidProfile => "invalid_profile",
            Self::UnknownProfile => "unknown_profile",
            Self::ProjectMismatch => "project_mismatch",
            Self::InvalidSettings => "invalid_admission_settings",
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
    mut input: ProfileSubmissionInput,
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
        "allowed_paths":input.task.allowed_paths, "test_commands":input.task.test_commands,
        "allow_dirty":input.allow_dirty, "allow_commit":input.allow_commit,
    });
    if let Some(budget) = &input.budget {
        payload["budget"] = budget.as_json().clone();
    }
    if profile.source != ProfileDefinitionSource::Builtin || profile.id != "implementer" {
        payload["profile"] = json!({"id":profile.id,"definition_hash":profile.definition_hash,"model":profile.model});
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
    .map_err(|_| SubmissionError::InvalidSettings)?;
    storage
        .create_task_with_profile(
            input.task,
            &settings,
            input.initial_status,
            input.budget.as_ref(),
            &profile,
        )
        .map_err(SubmissionError::Storage)
}
