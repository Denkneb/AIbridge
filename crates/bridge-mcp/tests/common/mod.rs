#![allow(dead_code)]
use bridge_config::{ProjectEntry, load_config_with_state_root};
use bridge_mcp::McpServer;
use bridge_storage::RustStateLayout;
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
pub struct Fixture {
    pub root: PathBuf,
    pub project: ProjectEntry,
    pub layout: RustStateLayout,
}
impl Fixture {
    pub fn new(extra: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-mcp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("workspace")).unwrap();
        let path = root.join("projects.toml");
        fs::write(&path,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file=\"missing-executor-password\"\nmax_rounds=3\n{extra}\n",json!(root.join("workspace")))).unwrap();
        let project = load_config_with_state_root(&path, &root.join("state"))
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
        let layout = RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
        Self {
            root,
            project,
            layout,
        }
    }
    pub fn server(&self) -> McpServer {
        McpServer::open(self.project.clone(), self.layout.clone()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
pub fn initialize() -> serde_json::Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"offline-fixture","version":"1"}}})
}
