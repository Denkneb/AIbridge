use crate::AutomationError as Error;
use bridge_config::ProjectEntry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub task: String,
    pub allowed_paths: Vec<String>,
    pub test_commands: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub profile: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlan {
    version: u64,
    goal: String,
    steps: Vec<Step>,
    final_test_commands: Vec<String>,
    max_seconds: Option<u64>,
    max_revisions: Option<u64>,
    codex_timeout: Option<u64>,
    codex_model: Option<String>,
    delivery: Option<String>,
}

/// Constructible only through validation; wire format includes explicit defaults.
#[derive(Clone, Serialize)]
pub struct ApprovedPlan {
    version: u64,
    goal: String,
    steps: Vec<Step>,
    final_test_commands: Vec<String>,
    max_seconds: u64,
    max_revisions: u64,
    codex_timeout: u64,
    codex_model: Option<String>,
    delivery: String,
}
impl ApprovedPlan {
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }
    pub fn final_test_commands(&self) -> &[String] {
        &self.final_test_commands
    }
}

fn text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= maximum
}
fn strings(values: &[String]) -> bool {
    !values.is_empty() && values.len() <= 200 && values.iter().all(|v| text(v, 4000))
}
fn commands(values: &[String]) -> Result<(), Error> {
    if !strings(values) {
        return Err(Error::InvalidPlan);
    }
    if !bridge_command_policy::validate_test_commands(
        &values.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .is_empty()
    {
        return Err(Error::Commands);
    }
    Ok(())
}
fn limit(value: Option<u64>, default: u64, maximum: u64) -> Result<u64, Error> {
    let value = value.unwrap_or(default);
    if (1..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(Error::InvalidPlan)
    }
}

/// Validate without creating state. Dependency order uses the first ready input
/// step, matching the reference's stable topological order.
pub fn validate_plan(
    project: &ProjectEntry,
    value: &serde_json::Value,
) -> Result<ApprovedPlan, Error> {
    // Optional JSON null is accepted only for profile/model in the reference.
    for key in ["max_seconds", "max_revisions", "codex_timeout", "delivery"] {
        if value.get(key).is_some_and(serde_json::Value::is_null) {
            return Err(Error::InvalidPlan);
        }
    }
    let raw: RawPlan = serde_json::from_value(value.clone()).map_err(|_| Error::InvalidPlan)?;
    if raw.version != 1 || !text(&raw.goal, 10000) || !(1..=100).contains(&raw.steps.len()) {
        return Err(Error::InvalidPlan);
    }
    let mut ids = BTreeSet::new();
    let mut pending = raw.steps;
    for step in &mut pending {
        let id = step.id.as_bytes();
        if id.is_empty()
            || id.len() > 64
            || !id[0].is_ascii_alphanumeric()
            || !id
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
            || !ids.insert(step.id.clone())
            || !text(&step.task, 60000)
            || !strings(&step.allowed_paths)
            || !strings(&step.acceptance_criteria)
        {
            return Err(Error::InvalidPlan);
        }
        step.allowed_paths = bridge_path_policy::validate_workspace_allowed_paths(
            project.workspace(),
            &step
                .allowed_paths
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        )
        .map_err(|_| Error::Scope)?;
        commands(&step.test_commands)?;
        if step
            .profile
            .as_ref()
            .is_some_and(|p| !project.profile_definitions().contains_key(p))
        {
            return Err(Error::Profile);
        }
        let deps: BTreeSet<_> = step.depends_on.iter().collect();
        if deps.len() != step.depends_on.len() || deps.contains(&step.id) {
            return Err(Error::Dependencies);
        }
    }
    let mut completed = BTreeSet::new();
    let mut steps = Vec::new();
    while !pending.is_empty() {
        let index = pending
            .iter()
            .position(|s| s.depends_on.iter().all(|d| completed.contains(d)))
            .ok_or(Error::Dependencies)?;
        let step = pending.remove(index);
        completed.insert(step.id.clone());
        steps.push(step);
    }
    commands(&raw.final_test_commands)?;
    let delivery = raw.delivery.unwrap_or_else(|| "apply".into());
    if !matches!(delivery.as_str(), "apply" | "manual")
        || raw.codex_model.as_ref().is_some_and(|m| !text(m, 200))
    {
        return Err(Error::InvalidPlan);
    }
    Ok(ApprovedPlan {
        version: 1,
        goal: raw.goal,
        steps,
        final_test_commands: raw.final_test_commands,
        max_seconds: limit(raw.max_seconds, 86400, 604800)?,
        max_revisions: limit(raw.max_revisions, project.max_rounds(), 20)?,
        codex_timeout: limit(raw.codex_timeout, 600, 3600)?,
        codex_model: raw.codex_model,
        delivery,
    })
}
