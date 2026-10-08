//! Approved sequential automation with isolated implementation and read-only review.
pub mod codex;
pub mod coordinator;
mod delivery;
pub mod lifecycle;
pub mod plan;
pub mod run;

/// Fixed errors never expose plan text, commands, credentials or local paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationError {
    InvalidPlan,
    Scope,
    Commands,
    Profile,
    Dependencies,
    Binding,
    Repository,
    UnfinishedTasks,
    Busy,
    State,
}
impl std::fmt::Display for AutomationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidPlan => "invalid approved plan",
            Self::Scope => "invalid automatic worktree scope",
            Self::Commands => "unsafe approved test commands",
            Self::Profile => "unknown approved profile",
            Self::Dependencies => "invalid plan dependency graph",
            Self::Binding => "automatic run binding changed",
            Self::Repository => "automatic run requires a clean supported main repository",
            Self::UnfinishedTasks => "project has unfinished tasks",
            Self::Busy => "automatic run or admission is busy",
            Self::State => "automatic state unavailable",
        })
    }
}
impl std::error::Error for AutomationError {}

pub mod remote;
