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

#[test]
fn hook_missing_unknown_config_and_invalid_argv_fail_open_without_output() {
    for args in [
        vec!["hook-status"],
        vec!["hook-status", "--invalid"],
        vec![
            "hook-status",
            "--project",
            "proj",
            "--config",
            "/nonexistent/config.toml",
            "--state-root",
            "/nonexistent/rust-state",
        ],
    ] {
        let o = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success());
        assert!(o.stdout.is_empty());
        assert!(o.stderr.is_empty());
    }
}

#[test]
fn hook_reads_owned_state_without_prompt_text_and_silences_corrupt_timestamp() {
    let root = std::env::temp_dir().join(format!("bridge-hook-{}", uuid::Uuid::new_v4()));
    let main = root.join("main");
    fs::create_dir_all(&main).unwrap();
    let path = root.join("projects.toml");
    fs::write(&path,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4999\"\npassword_file=\"unused\"\nmax_rounds=3\n",serde_json::json!(main))).unwrap();
    let config = bridge_config::load_config_with_state_root(&path, &root.join("state")).unwrap();
    let project = config.project("proj").unwrap();
    let layout =
        bridge_storage::RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
    layout.initialize().unwrap();
    let mut s = layout.open().unwrap();
    s.create_task(bridge_storage::CreateTaskInput {
        task_id: uuid::Uuid::new_v4().to_string().parse().unwrap(),
        project_id: project.id().clone(),
        workspace: main.to_str().unwrap().into(),
        task: "SECRET PROMPT".into(),
        request_id: "hook".into(),
        payload_hash: "hook".into(),
        base_head: None,
        allowed_paths: vec!["file".into()],
        test_commands: vec![],
        snapshot: None,
    })
    .unwrap();
    let call = || {
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .args(["hook-status", "--project", "proj", "--config"])
            .arg(&path)
            .arg("--state-root")
            .arg(layout.state_root())
            .output()
            .unwrap()
    };
    let o = call();
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert!(
        !String::from_utf8(o.stdout)
            .unwrap()
            .contains("SECRET PROMPT")
    );
    assert!(o.stderr.is_empty());
    assert!(!layout.state_root().join("runtime.lock").exists());
    s.connection()
        .execute("UPDATE tasks SET updated_at='bad INJECTION'", [])
        .unwrap();
    let o = call();
    assert!(o.status.success());
    assert!(o.stdout.is_empty() && o.stderr.is_empty());
    drop(s);
    fs::remove_dir_all(root).unwrap();
}
