use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-cli-mcp-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::write(root.join("projects.toml"),format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file=\"missing-executor-password\"\nmax_rounds=3\n",json!(root.join("workspace")))).unwrap();
        Self { root }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        command
            .current_dir(&self.root)
            .args([
                "mcp",
                "--project",
                "proj",
                "--config",
                "projects.toml",
                "--state-root",
            ])
            .arg(self.root.join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn run(&self, body: &str) -> std::process::Output {
        let mut child = self.command().spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn initialize() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}})
}
#[test]
fn mcp_cli_serves_a_real_stdio_session_without_banner_or_executor_credentials() {
    let f = Fixture::new();
    let input=[initialize(),json!({"jsonrpc":"2.0","method":"notifications/initialized"}),json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"project_info","arguments":{}}})].iter().map(|m|format!("{m}\n")).collect::<String>();
    let output = f.run(&input);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    let messages: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["result"]["tools"][0]["name"], "project_info");
    assert_eq!(
        messages[2]["result"]["structuredContent"]["project_id"],
        "proj"
    );
    assert_eq!(
        messages[2]["result"]["structuredContent"]["active_writer_count"],
        0
    );
    assert!(f.root.join("state/proj/state.sqlite").is_file());
    assert_eq!(fs::read_dir(f.root.join("workspace")).unwrap().count(), 0);
}
#[test]
fn second_mcp_process_cannot_claim_namespace_and_exit_releases_flock() {
    let f = Fixture::new();
    let mut first = Running(f.command().spawn().unwrap());
    writeln!(first.0.stdin.as_mut().unwrap(), "{}", initialize()).unwrap();
    first.0.stdin.as_mut().unwrap().flush().unwrap();
    let stdout = first.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let result = BufReader::new(stdout).read_line(&mut text);
        let _ = tx.send((result, text));
    });
    let (result, text) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    result.unwrap();
    assert!(
        serde_json::from_str::<Value>(&text)
            .unwrap()
            .get("result")
            .is_some()
    );
    let output = f.run("");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"agent-bridge: mcp_already_running\n");
    drop(first);
    assert!(f.run("").status.success());
}
#[test]
fn malformed_input_is_redacted_and_foreign_state_does_not_produce_protocol_stdout() {
    let f = Fixture::new();
    let output = f.run("private-token invalid-json\n");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let message: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(message["error"]["code"], -32700);
    assert!(!message.to_string().contains("private-token"));
    let marker = f.root.join("state/proj/.agent-bridge-state.json");
    fs::write(&marker, r#"{"implementation":"python","format_version":1}"#).unwrap();
    let before = fs::read(f.root.join("state/proj/state.sqlite")).unwrap();
    let output = f.run("");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"agent-bridge: mcp_state_unavailable\n");
    assert_eq!(
        fs::read(f.root.join("state/proj/state.sqlite")).unwrap(),
        before
    );
}
