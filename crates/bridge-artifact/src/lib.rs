//! Shared byte-complete artifacts and safe file operations. No task lifecycle.
pub mod artifact;
pub mod materialize;
pub use artifact::{Artifact, Entry, build_entries, load_artifact};
use std::fmt;
/// Same compact fingerprint triple persisted by the authoritative verifier.
pub fn fingerprint(snapshot: &bridge_git::RepositorySnapshot) -> serde_json::Value {
    serde_json::json!({"head":snapshot.head().map_or("",|h|h.as_str()),"index_fingerprint":snapshot.index_fingerprint().to_hex(),"worktree_fingerprint":snapshot.worktree_fingerprint().to_hex()})
}
pub type Result<T> = std::result::Result<T, DeliveryError>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryError {
    pub code: &'static str,
    pub paths: Vec<String>,
}
impl DeliveryError {
    pub fn new(code: &'static str) -> Self {
        Self {
            code,
            paths: vec![],
        }
    }
    pub fn paths(code: &'static str, paths: Vec<String>) -> Self {
        Self { code, paths }
    }
}
impl fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for DeliveryError {}
