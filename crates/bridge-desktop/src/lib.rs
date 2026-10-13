//! UI-independent project settings, read-only dashboard and bounded PTY services.
mod automation;
mod branches;
mod console_worktrees;
pub use console_worktrees::{ConsoleCheckout, ConsoleCheckoutView};
mod diff;
mod overview;
pub use branches::{GitAction, GitRequest};
pub mod codex_env;
pub mod dashboard;
mod opencode_config;
mod opencode_keys;
pub mod projects;
mod task_recovery;
pub mod terminal;

mod shutdown;
