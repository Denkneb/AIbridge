//! Frozen controller security settings for the future launch-opencode consumer.
use bridge_config::{ProjectEntry, state_directory_permission_pattern};
use bridge_domain::Result;
use serde_json::{Value, json};
use std::path::Path;

/// The controller process cannot create internal subagents.
pub const CONTROLLER_SUBAGENT_DEPTH: u8 = 0;

/// Generates hard controller permissions without reading or creating state.
/// State access is a separate opt-in rule; external Git roots are unchanged.
/// # Errors
/// Rejects opted-in roots whose permission pattern cannot be scoped safely.
pub fn controller_agent_permission(project: &ProjectEntry, state_root: &Path) -> Result<Value> {
    let mut permission = json!({"edit":"deny","task":"deny","bash":"ask"});
    if project.auto_approve_state_directory() {
        let pattern = state_directory_permission_pattern(state_root)?;
        let mut external = json!({"*":"ask"});
        external[pattern] = json!("allow");
        permission["external_directory"] = external;
    }
    Ok(permission)
}
