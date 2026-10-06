use bridge_config::{Config, load_config_with_state_root};
use bridge_runtime::controller::{
    CONFIG_FILENAME, CONTROLLER_PROMPT, ControllerCommand, ControllerError,
    build_controller_config, check_workspace_config, launch_controller, parse_jsonc,
};
use bridge_storage::RustStateLayout;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-controller-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::create_dir_all(root.join("peer-workspace")).unwrap();
        for (name, token) in [
            ("primary", "primary-fixture-token"),
            ("peer", "peer-fixture-token"),
        ] {
            let path = root.join(format!("{name}.token"));
            fs::write(&path, token).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        Self { root }
    }
    fn config(&self, remote: bool, extra: &str) -> Config {
        let mcp = if remote {
            "mcp_url=\"http://127.0.0.1:4201/mcp\"\nmcp_token_file=\"primary.token\""
        } else {
            ""
        };
        fs::write(self.root.join("projects.toml"), format!(
            "[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file=\"missing-executor-password\"\nmax_rounds=3\n{mcp}\n{extra}\n",
            json!(self.root.join("workspace")))).unwrap();
        load_config_with_state_root(&self.root.join("projects.toml"), &self.root.join("state"))
            .unwrap()
    }
    fn linked_config(&self) -> Config {
        self.config(true, &format!("auto_approve_state_directory=true\nauto_approve_external_directories=[{}]\n[projects.peer]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4102\"\npassword_file=\"missing-executor-password\"\nmax_rounds=3\nmcp_url=\"http://127.0.0.1:4202/mcp\"\nmcp_token_file=\"peer.token\"",json!(self.root.join("peer-workspace")),json!(self.root.join("peer-workspace"))))
    }
    fn layout(&self, config: &Config) -> RustStateLayout {
        RustStateLayout::new(
            self.root.join("state"),
            config.project("proj").unwrap().id().clone(),
        )
        .unwrap()
    }
    fn launch(&self, config: &Config) -> Result<std::process::ExitStatus, ControllerError> {
        let project = config.project("proj").unwrap();
        launch_controller(
            project,
            &config.linked_projects("proj"),
            &self.layout(config),
            &self.root.join("agent-bridge"),
            &self.root.join("projects.toml"),
            &ControllerCommand::executable(std::path::Path::new(env!(
                "CARGO_BIN_EXE_controller_fixture"
            )))
            .unwrap(),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn jsonc_preserves_urls_escapes_unicode_and_comment_delimited_trailing_commas() {
    let value = parse_jsonc(
        r#"{
        // comment
        "url":"http://localhost/a//b/*literal*/", "quote":"\\\"", "русский":"да",
        "array":[1, /*comment*/2, // after comma
        ], /*after comma*/
    }"#,
    )
    .unwrap();
    assert_eq!(value["url"], "http://localhost/a//b/*literal*/");
    assert_eq!(value["quote"], "\\\"");
    assert_eq!(value["русский"], "да");
    assert_eq!(value["array"], json!([1, 2]));
    for malformed in [
        "{/*",
        "{\"x\":\"open",
        "{\"x\":\"escape\\",
        "[1,,]",
        "{\"n\":1/*gap*/2}",
        "{\"x\":True}",
        "{\"x\":NaN}",
    ] {
        assert_eq!(
            parse_jsonc(malformed).unwrap_err(),
            ControllerError::WorkspaceConfig
        );
    }
}
#[test]
fn generated_config_matches_controller_contract_and_contains_only_token_placeholders() {
    let f = Fixture::new();
    let config = f.linked_config();
    let primary = config.project("proj").unwrap();
    let layout = f.layout(&config);
    let value = build_controller_config(
        primary,
        &config.linked_projects("proj"),
        &layout,
        &f.root.join("agent-bridge"),
        &f.root.join("projects.toml"),
    )
    .unwrap();
    assert_eq!(value["default_agent"], "bridge-controller");
    assert_eq!(value["subagent_depth"], 0);
    assert_eq!(
        value["agent"]["bridge-controller"]["prompt"],
        CONTROLLER_PROMPT
    );
    assert_eq!(
        value["agent"]["bridge-controller"]["permission"],
        json!({"edit":"deny","task":"deny","bash":"ask","external_directory":{"*":"ask",layout.state_root().join("*").to_str().unwrap():"allow"}})
    );
    for (name, url, var) in [
        (
            "agent_bridge",
            "http://127.0.0.1:4201/mcp",
            "AGENT_BRIDGE_MCP_TOKEN",
        ),
        (
            "agent_bridge_peer",
            "http://127.0.0.1:4202/mcp",
            "AGENT_BRIDGE_MCP_TOKEN_PEER",
        ),
    ] {
        assert_eq!(
            value["mcp"][name],
            json!({"type":"remote","url":url,"enabled":true,"oauth":false,"headers":{"Authorization":format!("Bearer {{env:{var}}}")},"timeout":330000})
        );
    }
    assert!(!value.to_string().contains("fixture-token"));
    assert!(!layout.state_root().exists());
}
#[test]
fn stdio_config_uses_absolute_explicit_rust_paths_and_launches_controller() {
    let f = Fixture::new();
    let config = f.config(false, "");
    let layout = f.layout(&config);
    let value = build_controller_config(
        config.project("proj").unwrap(),
        &[],
        &layout,
        &f.root.join("agent-bridge"),
        &f.root.join("projects.toml"),
    )
    .unwrap();
    assert_eq!(
        value["mcp"]["agent_bridge"]["command"],
        json!([
            f.root.join("agent-bridge"),
            "mcp",
            "--project",
            "proj",
            "--config",
            f.root.join("projects.toml"),
            "--state-root",
            f.root.join("state")
        ])
    );
    assert_eq!(f.launch(&config).unwrap().code(), Some(17));
    assert!(layout.project_dir().join(CONFIG_FILENAME).is_file());
}
#[test]
fn conflict_and_invalid_config_fail_before_tokens_or_state_are_touched() {
    let f = Fixture::new();
    let config = f.linked_config();
    fs::remove_file(f.root.join("primary.token")).unwrap();
    for name in ["opencode.json", "opencode.jsonc"] {
        for (text, error) in [
            (
                r#"{"mcp":{"agent_bridge":null}}"#,
                ControllerError::WorkspaceConflict,
            ),
            (
                r#"{"mcp":{"agent_bridge_peer":{"enabled":false}}}"#,
                ControllerError::WorkspaceConflict,
            ),
            (
                r#"{"default_agent":null}"#,
                ControllerError::WorkspaceConflict,
            ),
            (
                r#"{"subagent_depth":99}"#,
                ControllerError::WorkspaceConflict,
            ),
            (
                r#"{"agent":{"bridge-controller":{"permission":{"edit":"allow"}}}}"#,
                ControllerError::WorkspaceConflict,
            ),
            (r#"{"mcp":[]}"#, ControllerError::WorkspaceConfig),
            (r#"{"agent":false}"#, ControllerError::WorkspaceConfig),
            ("[]", ControllerError::WorkspaceConfig),
            ("{/*", ControllerError::WorkspaceConfig),
        ] {
            fs::write(f.root.join("workspace").join(name), text).unwrap();
            assert_eq!(f.launch(&config).unwrap_err(), error, "{name}: {text}");
            assert!(!f.root.join("state").exists());
        }
        fs::remove_file(f.root.join("workspace").join(name)).unwrap();
    }
    assert_eq!(f.launch(&config).unwrap_err(), ControllerError::Credentials);
    assert!(!f.root.join("state").exists());
}
#[test]
fn benign_workspace_config_survives_unchanged_and_child_gets_private_config_exit_code_and_tokens() {
    let f = Fixture::new();
    let config = f.linked_config();
    let text = r#"{/* normal provider config */"model":"provider/model","mcp":{"other":{"enabled":false}},"agent":{"reviewer":{}},}"#;
    fs::write(f.root.join("workspace/opencode.jsonc"), text).unwrap();
    let status = f.launch(&config).unwrap();
    assert_eq!(status.code(), Some(17));
    let layout = f.layout(&config);
    layout.open().unwrap();
    let path = layout.project_dir().join(CONFIG_FILENAME);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(layout.project_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(!fs::read_to_string(path).unwrap().contains("fixture-token"));
    let evidence: Value = serde_json::from_str(
        &fs::read_to_string(
            layout
                .project_dir()
                .join("controller-fixture-observed.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(evidence["cwd"], json!(f.root.join("workspace")));
    for key in [
        "args_empty",
        "primary_token_correct",
        "linked_token_correct",
        "executor_password_absent",
        "executor_username_absent",
        "proxy_bypass",
    ] {
        assert_eq!(evidence[key], true, "{key}");
    }
    assert_eq!(
        fs::read_to_string(f.root.join("workspace/opencode.jsonc")).unwrap(),
        text
    );
    assert_eq!(fs::read_dir(f.root.join("workspace")).unwrap().count(), 1);
    assert_eq!(f.launch(&config).unwrap().code(), Some(17)); // foreground exit releases config lock
}
#[test]
fn unreadable_config_including_dangling_symlink_fails_closed() {
    let f = Fixture::new();
    let config = f.config(true, "");
    symlink(
        f.root.join("missing"),
        f.root.join("workspace/opencode.json"),
    )
    .unwrap();
    assert_eq!(
        check_workspace_config(config.project("proj").unwrap(), &[]).unwrap_err(),
        ControllerError::WorkspaceConfig
    );
    assert!(!f.root.join("state").exists());
}
#[test]
fn state_symlinks_foreign_marker_and_workspace_overlap_are_never_adopted() {
    let f = Fixture::new();
    let config = f.config(true, "");
    let layout = f.layout(&config);
    fs::create_dir_all(f.root.join("foreign")).unwrap();
    symlink(f.root.join("foreign"), layout.state_root()).unwrap();
    assert_eq!(f.launch(&config).unwrap_err(), ControllerError::Binding);
    assert_eq!(fs::read_dir(f.root.join("foreign")).unwrap().count(), 0);
    fs::remove_file(layout.state_root()).unwrap();
    fs::create_dir_all(layout.state_root()).unwrap();
    symlink(f.root.join("foreign"), layout.project_dir()).unwrap();
    assert_eq!(f.launch(&config).unwrap_err(), ControllerError::Binding);
    assert_eq!(fs::read_dir(f.root.join("foreign")).unwrap().count(), 0);
    fs::remove_file(layout.project_dir()).unwrap();
    fs::create_dir_all(layout.project_dir()).unwrap();
    fs::write(
        layout.marker(),
        r#"{"implementation":"python","format_version":1}"#,
    )
    .unwrap();
    let before = fs::read(layout.marker()).unwrap();
    assert_eq!(f.launch(&config).unwrap_err(), ControllerError::State);
    assert_eq!(fs::read(layout.marker()).unwrap(), before);
    assert!(!layout.database().exists());
    let overlap = RustStateLayout::new(
        f.root.join("workspace/state"),
        config.project("proj").unwrap().id().clone(),
    )
    .unwrap();
    assert_eq!(
        build_controller_config(
            config.project("proj").unwrap(),
            &[],
            &overlap,
            &f.root.join("bridge"),
            &f.root.join("projects.toml")
        )
        .unwrap_err(),
        ControllerError::Binding
    );
}
#[test]
fn linked_token_collision_is_detected_before_any_token_reads_or_state_writes() {
    let f = Fixture::new();
    fs::create_dir(f.root.join("other-workspace")).unwrap();
    let config = f.config(true, &format!(
        "auto_approve_external_directories=[{},{}]\n[projects.a-b]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4102\"\nmax_rounds=3\nmcp_url=\"http://127.0.0.1:4202/mcp\"\nmcp_token_file=\"missing-one\"\n[projects.a_b]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4103\"\nmax_rounds=3\nmcp_url=\"http://127.0.0.1:4203/mcp\"\nmcp_token_file=\"missing-two\"",
        json!(f.root.join("peer-workspace")), json!(f.root.join("other-workspace")),
        json!(f.root.join("peer-workspace")), json!(f.root.join("other-workspace"))));
    assert_eq!(
        f.launch(&config).unwrap_err(),
        ControllerError::TokenCollision
    );
    assert!(!f.root.join("state").exists());
}

#[test]
fn live_controller_lock_prevents_config_replacement_and_atomic_write_never_follows_target_symlink()
{
    use std::os::unix::io::AsRawFd;
    let f = Fixture::new();
    let config = f.config(true, "");
    let layout = f.layout(&config);
    layout.initialize().unwrap();
    let path = layout.project_dir().join(CONFIG_FILENAME);
    fs::write(&path, "original").unwrap();
    let guard = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(layout.project_dir().join("controller.lock"))
        .unwrap();
    nix::fcntl::flock(
        guard.as_raw_fd(),
        nix::fcntl::FlockArg::LockExclusiveNonblock,
    )
    .unwrap();
    assert_eq!(f.launch(&config).unwrap_err(), ControllerError::Busy);
    assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    assert!(
        !layout
            .project_dir()
            .join("controller-fixture-observed.json")
            .exists()
    );
    drop(guard);
    fs::remove_file(&path).unwrap();
    fs::write(f.root.join("unrelated"), "untouched").unwrap();
    symlink(f.root.join("unrelated"), &path).unwrap();
    assert_eq!(f.launch(&config).unwrap().code(), Some(17));
    assert_eq!(
        fs::read_to_string(f.root.join("unrelated")).unwrap(),
        "untouched"
    );
    assert!(!fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    assert!(!fs::read_dir(layout.project_dir()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
}
#[test]
fn failed_spawn_releases_lock_and_does_not_expose_command_or_credentials() {
    let f = Fixture::new();
    let config = f.config(true, "");
    let program = f.root.join("not-executable");
    fs::write(&program, "not a program").unwrap();
    let result = launch_controller(
        config.project("proj").unwrap(),
        &[],
        &f.layout(&config),
        &f.root.join("agent-bridge"),
        &f.root.join("projects.toml"),
        &ControllerCommand::executable(&program).unwrap(),
    );
    let error = result.unwrap_err();
    assert_eq!(error, ControllerError::Spawn);
    assert_eq!(error.to_string(), "controller_spawn_failed");
    assert_eq!(f.launch(&config).unwrap().code(), Some(17));
}

#[test]
fn codex_linked_remote_wiring_isolated_and_collisions_precede_state() {
    use bridge_runtime::codex_controller::build_codex_args;
    let f = Fixture::new();
    let config = f.linked_config();
    let primary = config.project("proj").unwrap();
    let args = build_codex_args(
        primary,
        &config.linked_projects("proj"),
        &f.layout(&config),
        &f.root.join("bridge with 'quote"),
        &f.root.join("projects.toml"),
    )
    .unwrap();
    let joined = args
        .iter()
        .map(|a| a.to_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("mcp_servers.agent_bridge_peer="));
    assert!(joined.contains("AGENT_BRIDGE_MCP_TOKEN_PEER"));
    assert!(!joined.contains("primary-fixture-token"));
    assert!(!joined.contains("peer-fixture-token"));
    assert!(joined.contains("'\\\\''"));
    assert!(!f.root.join("state").exists());
}
