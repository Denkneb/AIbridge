#[path = "../../bridge-delivery/tests/common/mod.rs"]
mod common;
use serde_json::Value;
use std::process::Command;
#[test]
fn delivery_cli_defaults_to_dry_run_then_builds_applies_and_refuses_repeat() {
    let f = common::Fixture::new();
    f.changes();
    let run = |mode: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        command
            .args([
                "deliver-task",
                "--project",
                "proj",
                "--task",
                &f.id.to_string(),
                "--config",
            ])
            .arg(f.root.join("projects.toml"))
            .arg("--state-root")
            .arg(f.layout.state_root());
        if let Some(mode) = mode {
            command.arg(mode);
        }
        command.output().unwrap()
    };
    let before = bridge_git::take_snapshot(f.project.workspace())
        .unwrap()
        .to_json()
        .unwrap();
    let result = run(None);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap()["status"],
        "validated"
    );
    assert!(!f.dest().exists());
    assert_eq!(
        bridge_git::take_snapshot(f.project.workspace())
            .unwrap()
            .to_json()
            .unwrap(),
        before
    );
    assert!(run(Some("--build")).status.success());
    assert!(f.dest().join("manifest.json").is_file());
    let result = run(Some("--apply"));
    assert!(
        result.status.success(),
        "{} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap()["status"],
        "delivered"
    );
    assert_eq!(
        std::fs::read(f.project.workspace().join("src/a")).unwrap(),
        b"result\xff\0"
    );
    let result = run(Some("--apply"));
    assert!(!result.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap()["code"],
        "already_delivered"
    );
}
