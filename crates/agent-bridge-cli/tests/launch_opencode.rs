use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new(remote: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-cli-controller-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(root.join("primary.token"), "primary-fixture-token").unwrap();
        fs::set_permissions(
            root.join("primary.token"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let mcp = if remote {
            "mcp_url=\"http://127.0.0.1:4201/mcp\"\nmcp_token_file=\"primary.token\""
        } else {
            ""
        };
        fs::write(root.join("projects.toml"),format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file=\"missing-executor-password\"\nmax_rounds=3\nauto_approve_state_directory=true\n{mcp}\n",json!(root.join("workspace")))).unwrap();
        Self { root }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        command
            .current_dir(&self.root)
            .env("PATH", self.root.join("bin"))
            .env("OPENCODE_SERVER_PASSWORD", "executor-fixture-secret")
            .env("OPENCODE_SERVER_USERNAME", "executor-fixture-user")
            .env("OPENCODE_CONFIG", "inherited-config")
            .env("UNCHANGED_PROVIDER", "provider-fixture")
            .env("AGENT_BRIDGE_MCP_TOKEN", "stale-inherited-token")
            .env("NO_PROXY", "example.local,localhost")
            .env("no_proxy", "lower.local")
            .env("EXPECTED_WORKSPACE", self.root.join("workspace"));
        command
    }
    fn launch(&self) -> Output {
        self.command()
            .args([
                "launch-opencode",
                "--project",
                "proj",
                "--config",
                "projects.toml",
                "--state-root",
            ])
            .arg(self.root.join("state"))
            .output()
            .unwrap()
    }
    fn script(&self, body: &str) {
        fs::write(
            self.root.join("bin/opencode"),
            format!("#!/bin/sh\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(
            self.root.join("bin/opencode"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn cli_launch_preserves_stdio_provider_env_and_workspace_but_scrubs_executor_auth() {
    let f = Fixture::new(true);
    f.script(
        r#"
[ "$#" -eq 0 ] || exit 71
[ "$AGENT_BRIDGE_MCP_TOKEN" = "primary-fixture-token" ] || exit 72
[ "${OPENCODE_SERVER_PASSWORD+x}" != x ] || exit 73
[ "${OPENCODE_SERVER_USERNAME+x}" != x ] || exit 74
[ "$UNCHANGED_PROVIDER" = "provider-fixture" ] || exit 75
[ "$NO_PROXY" = "example.local,localhost,127.0.0.1" ] || exit 76
[ "$no_proxy" = "lower.local,127.0.0.1,localhost" ] || exit 77
[ "$PWD" = "$EXPECTED_WORKSPACE" ] || exit 78
[ -f "$OPENCODE_CONFIG" ] || exit 79
[ "$OPENCODE_CONFIG" != "inherited-config" ] || exit 80
printf 'controller-fixture-ok\n'
exit 17
"#,
    );
    let output = f.launch();
    assert_eq!(output.status.code(), Some(17), "{:?}", output);
    assert_eq!(output.stdout, b"controller-fixture-ok\n");
    assert!(output.stderr.is_empty());
    let path = f.root.join("state/proj/controller-opencode.json");
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("fixture-token"));
    assert!(!text.contains("executor-fixture"));
    let config: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        config["agent"]["bridge-controller"]["permission"]["edit"],
        "deny"
    );
    assert_eq!(
        config["agent"]["bridge-controller"]["permission"]["task"],
        "deny"
    );
    assert_eq!(
        config["agent"]["bridge-controller"]["permission"]["external_directory"]
            [f.root.join("state/*").to_str().unwrap()],
        "allow"
    );
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read_dir(f.root.join("workspace")).unwrap().count(), 0);
}
#[test]
fn inline_flags_work_and_signal_exit_is_propagated() {
    let f = Fixture::new(true);
    f.script("kill -TERM $$");
    let output = f
        .command()
        .args([
            "launch-opencode",
            "--project=proj",
            "--config=projects.toml",
        ])
        .arg(format!("--state-root={}", f.root.join("state").display()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(143));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}
#[test]
fn parser_rejects_missing_duplicate_unknown_and_relative_state_flags_without_writes() {
    let f = Fixture::new(true);
    for args in [
        vec![],
        vec!["unknown"],
        vec!["launch-opencode"],
        vec!["launch-opencode", "--project", "proj"],
        vec![
            "launch-opencode",
            "--project",
            "proj",
            "--config",
            "projects.toml",
        ],
        vec!["launch-opencode", "--project", "proj", "--project", "other"],
        vec!["launch-opencode", "--secret=do-not-print-this"],
        vec!["launch-opencode", "--project", "--config"],
        vec![
            "launch-opencode",
            "--project",
            "proj",
            "--config",
            "projects.toml",
            "--state-root",
            "relative",
        ],
    ] {
        let output = f.command().args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("do-not-print-this"));
        assert!(!f.root.join("state").exists());
    }
}
#[test]
fn help_and_version_are_available_without_config_or_state() {
    let f = Fixture::new(true);
    for args in [
        vec!["--help"],
        vec!["launch-opencode", "--help"],
        vec!["--version"],
    ] {
        let output = f.command().args(args).output().unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert!(!output.stdout.is_empty());
    }
    assert!(!f.root.join("state").exists());
}
#[test]
fn local_transport_conflict_and_missing_token_fail_before_launch_and_state() {
    let f = Fixture::new(false);
    f.script("printf 'must-not-launch\\n'");
    let output = f.launch();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("controller_local_mcp_unavailable"));
    assert!(!f.root.join("state").exists());
    let f = Fixture::new(true);
    f.script("printf 'must-not-launch\\n'");
    fs::remove_file(f.root.join("primary.token")).unwrap();
    fs::write(
        f.root.join("workspace/opencode.jsonc"),
        r#"{"agent":{"bridge-controller":{"permission":{"edit":"allow"}}}}"#,
    )
    .unwrap();
    let output = f.launch();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("controller_workspace_config_conflict")
    );
    assert!(!f.root.join("state").exists());
    fs::remove_file(f.root.join("workspace/opencode.jsonc")).unwrap();
    let output = f.launch();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("controller_credentials_unavailable"));
    assert!(!f.root.join("state").exists());
}
#[test]
fn missing_opencode_reports_safe_error_and_releases_controller_lock() {
    let f = Fixture::new(true);
    let output = f.launch();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"agent-bridge: controller_spawn_failed\n");
    f.script("exit 0");
    assert!(f.launch().status.success());
}
