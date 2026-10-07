use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new(remote: bool) -> Self {
        let root = std::env::temp_dir().join(format!("bridge-codex-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("workspace with 'quote")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        fs::write(root.join("token"), "fixture-token").unwrap();
        fs::set_permissions(root.join("token"), fs::Permissions::from_mode(0o600)).unwrap();
        let mcp = if remote {
            "mcp_url=\"http://127.0.0.1:4201/mcp\"\nmcp_token_file=\"token\""
        } else {
            ""
        };
        fs::write(root.join("projects.toml"),format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file=\"missing-password\"\nmax_rounds=3\n{mcp}\n",json!(root.join("workspace with 'quote")))).unwrap();
        Self { root }
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        c.current_dir(&self.root)
            .args([
                "launch-codex",
                "--project",
                "proj",
                "--config",
                "projects.toml",
                "--state-root",
            ])
            .arg(self.root.join("state"))
            .env("PATH", self.root.join("bin"))
            .env("PROVIDER_SENTINEL", "preserved")
            .env("OPENCODE_SERVER_PASSWORD", "private-executor")
            .env("AGENT_BRIDGE_MCP_TOKEN_STALE", "stale")
            .env("CAPTURE", self.root.join("capture.json"));
        c
    }
    fn script(&self, body: &str) {
        fs::write(self.root.join("bin/codex"), body).unwrap();
        fs::set_permissions(
            self.root.join("bin/codex"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    fn launch(&self) -> Output {
        self.command().output().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn local_controller_argv_hooks_rules_and_exit_are_process_scoped() {
    let f = Fixture::new(false);
    f.script("#!/usr/bin/python3\nimport json,os,sys\na=sys.argv[1:]; values=[a[i+1] for i,s in enumerate(a) if s=='-c']; overrides=values; json.dump(dict(args=a,overrides=overrides,cwd=os.getcwd(),provider=os.getenv('PROVIDER_SENTINEL'),executor_absent='OPENCODE_SERVER_PASSWORD' not in os.environ,stale_absent='AGENT_BRIDGE_MCP_TOKEN_STALE' not in os.environ,token_absent='AGENT_BRIDGE_MCP_TOKEN' not in os.environ),open(os.environ['CAPTURE'],'w'));sys.exit(17)\n");
    let result = f.launch();
    assert_eq!(result.status.code(), Some(17));
    assert!(result.stdout.is_empty());
    let v: serde_json::Value =
        serde_json::from_slice(&fs::read(f.root.join("capture.json")).unwrap()).unwrap();
    assert_eq!(v["provider"], "preserved");
    assert_eq!(v["cwd"], json!(f.root.join("workspace with 'quote")));
    for key in ["executor_absent", "stale_absent", "token_absent"] {
        assert_eq!(v[key], true);
    }
    let entries = v["overrides"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| {
            serde_json::to_value(value.as_str().unwrap().parse::<toml::Value>().unwrap()).unwrap()
        })
        .collect::<Vec<_>>();
    let starter = v["args"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .as_str()
        .unwrap();
    assert!(starter.contains("console:\n") && starter.contains("'console' '--project' 'proj'"));
    assert!(starter.contains("'aibridge-desktop' '--config'"));
    let m = entries.iter().find_map(|o| o.get("mcp_servers")).unwrap();
    let server = &m["agent_bridge"];
    assert_eq!(server["command"], env!("CARGO_BIN_EXE_agent-bridge"));
    assert_eq!(server["required"], true);
    assert_eq!(server["tool_timeout_sec"], 330);
    assert_eq!(server["tools"].as_object().unwrap().len(), 6);
    assert_eq!(server["args"][2], "proj");
    assert_eq!(server["args"][4], json!(f.root.join("projects.toml")));
    assert_eq!(server["args"][6], json!(f.root.join("state")));
    let hook = entries.iter().find_map(|o| o.get("hooks")).unwrap();
    assert_eq!(hook["UserPromptSubmit"][0]["hooks"][0]["timeout"], 5);
    assert!(hook.to_string().contains("hook-status"));
    assert!(!f.root.join("workspace with 'quote/.codex").exists());
    assert!(!f.root.join("state/proj/controller-opencode.json").exists());
    let instructions = entries
        .iter()
        .find_map(|o| o["developer_instructions"].as_str())
        .unwrap();
    assert!(instructions.contains("submit_task"));
    assert!(instructions.contains("Правила постоянны (включая /new)"));
    assert!(instructions.contains(include_str!("../../../docs/delegated-task-brief.txt")));
}
#[test]
fn remote_tokens_are_only_in_child_env_and_preflight_has_no_state_writes() {
    let f = Fixture::new(true);
    f.script("#!/bin/sh\n[ \"$AGENT_BRIDGE_MCP_TOKEN\" = fixture-token ] || exit 72\n[ \"${OPENCODE_SERVER_PASSWORD+x}\" != x ] || exit 73\nexit 0\n");
    assert!(f.launch().status.success());
    fs::remove_dir_all(f.root.join("state")).unwrap();
    fs::remove_file(f.root.join("token")).unwrap();
    let r = f.launch();
    assert!(!r.status.success());
    assert!(!f.root.join("state").exists());
    assert!(!String::from_utf8_lossy(&r.stderr).contains("fixture-token"));
}
#[test]
fn shared_controller_fence_spawn_failure_and_signals() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let f = Fixture::new(false);
    f.script("#!/bin/sh\nprintf '%s' $$ > \"$CAPTURE\"\nexec /bin/sleep 30\n");
    let mut child = f.command().spawn().unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !f.root.join("state/proj/controller.lock").exists() {
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(10));
    }
    // The file precedes flock by a few instructions; retry until contention is observed.
    thread::sleep(Duration::from_millis(50));
    let blocked = f.launch();
    assert_eq!(blocked.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("controller_already_running"));
    let pid = fs::read_to_string(f.root.join("capture.json")).unwrap();
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    child.wait().unwrap();
    f.script("#!/bin/sh\nkill -TERM $$\n");
    assert_eq!(f.launch().status.code(), Some(143));
    fs::remove_file(f.root.join("bin/codex")).unwrap();
    assert_eq!(f.launch().status.code(), Some(1));
    f.script("#!/bin/sh\nexit 0\n");
    assert!(f.launch().status.success());
}
