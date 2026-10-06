use std::{fs, process::Command};
#[test]
fn missing_status_is_readonly_versioned_and_not_ready() {
    let root = std::env::temp_dir().join(format!("bridge-status-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let config = root.join("projects.toml");
    fs::write(&config,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4999\"\npassword_file=\"unused\"\nmax_rounds=3\n",serde_json::json!(root))).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["status", "--json", "--project", "proj", "--config"])
        .arg(config)
        .arg("--state-root")
        .arg(root.join("state"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["projects"][0]["ready"], false);
    assert_eq!(v["projects"][0]["snapshot"]["error"], "state_unavailable");
    assert!(!root.join("state").exists());
    fs::remove_dir_all(root).unwrap();
}
