use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output},
};
struct Fixture {
    root: PathBuf,
    config: PathBuf,
    blocked: Option<TcpListener>,
}
fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
impl Fixture {
    fn new(second: bool) -> Self {
        let root = std::env::temp_dir().join(format!("bridge-services-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("main")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        let config = root.join("projects.toml");
        let mut text = format!(
            "[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:{}\"\nmcp_url=\"http://127.0.0.1:{}/mcp\"\npassword_file={}\nmcp_token_file={}\nmax_rounds=3\n",
            json!(root.join("main")),
            port(),
            port(),
            json!(root.join("password")),
            json!(root.join("token"))
        );
        let blocked = if second {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            fs::create_dir(root.join("second")).unwrap();
            text.push_str(&format!("[projects.second]\nworkspace={}\nopencode_url=\"http://127.0.0.1:{}\"\nmcp_url=\"http://127.0.0.1:{}/mcp\"\npassword_file={}\nmcp_token_file={}\nmax_rounds=3\n",json!(root.join("second")),l.local_addr().unwrap().port(),port(),json!(root.join("second-password")),json!(root.join("second-token"))));
            Some(l)
        } else {
            None
        };
        fs::write(&config, text).unwrap();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixture = base.join("../bridge-automation/tests/fixtures/opencode.py");
        let doc = base.join("../bridge-runtime/tests/fixtures/openapi.json");
        let program = format!(
            "#!/usr/bin/python3\nimport sys,runpy,os,json\nif sys.argv[1]=='attach':\n open(os.environ['PROOF_ATTACH_OUTPUT'],'w').write(json.dumps({{'argv':sys.argv[1:],'cwd':os.getcwd(),'auth':bool(os.environ.get('OPENCODE_SERVER_PASSWORD'))}}))\n sys.exit(7)\nsys.argv=[{},'--doc',{}]+sys.argv[1:]\nrunpy.run_path({},run_name='__main__')\n",
            json!(fixture),
            json!(doc),
            json!(fixture)
        );
        fs::write(root.join("bin/opencode"), program).unwrap();
        fs::set_permissions(root.join("bin/opencode"), fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            root,
            config,
            blocked,
        }
    }
    fn cli(&self, cmd: &str, all: bool, flags: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        c.arg(cmd);
        if all {
            c.arg("--all");
        } else {
            c.args(["--project", "proj"]);
        }
        c.arg("--config")
            .arg(&self.config)
            .arg("--state-root")
            .arg(self.root.join("state"))
            .args(flags)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("PROOF_ATTACH_OUTPUT", self.root.join("attach.json"));
        c.output().unwrap()
    }
    fn record(&self, kind: &str) -> PathBuf {
        self.root
            .join("state/proj")
            .join(format!("{kind}.process.json"))
    }
    fn ok(&self, cmd: &str, all: bool, flags: &[&str]) -> Value {
        let o = self.cli(cmd, all, flags);
        assert!(
            o.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        serde_json::from_slice(&o.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.cli("stop", true, &[]);
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn setup_start_readonly_status_doctor_attach_stop_and_foreign_record_guards() {
    let f = Fixture::new(false);
    assert_eq!(f.cli("doctor", false, &["--json"]).status.code(), Some(1));
    assert!(!f.root.join("state").exists());
    f.ok("setup", false, &[]);
    let pwd = fs::read(f.root.join("password")).unwrap();
    assert_eq!(
        fs::metadata(f.root.join("password"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    f.ok("setup", false, &[]);
    assert_eq!(fs::read(f.root.join("password")).unwrap(), pwd);
    f.ok("start", false, &[]);
    let old = fs::read(f.record("opencode")).unwrap();
    f.ok("start", false, &[]);
    assert_eq!(fs::read(f.record("opencode")).unwrap(), old);
    let report = f.ok("status", false, &["--json"]);
    assert_eq!(report["schema_version"], 1);
    for kind in ["opencode", "mcp"] {
        assert_eq!(report["projects"][0]["servers"][kind]["ready"], true);
        assert_eq!(report["projects"][0]["servers"][kind]["managed"], true);
    }
    f.ok("doctor", false, &["--json"]);
    assert_eq!(f.cli("console", false, &[]).status.code(), Some(7));
    let capture: Value =
        serde_json::from_slice(&fs::read(f.root.join("attach.json")).unwrap()).unwrap();
    assert_eq!(capture["cwd"], json!(f.root.join("main")));
    assert_eq!(capture["auth"], true);
    assert_eq!(capture["argv"][0], "attach");
    let mut record: Value = serde_json::from_slice(&old).unwrap();
    record["project_id"] = json!("foreign");
    fs::write(f.record("opencode"), record.to_string()).unwrap();
    assert!(!f.cli("stop", false, &[]).status.success());
    assert!(f.record("opencode").exists());
    assert_eq!(f.cli("status", false, &["--json"]).status.code(), Some(1));
    fs::write(f.record("opencode"), old).unwrap();
    f.ok("stop", false, &[]);
    f.ok("stop", false, &[]);
    assert!(!f.record("opencode").exists());
    assert_eq!(f.cli("status", false, &["--json"]).status.code(), Some(1));
}
#[test]
fn multi_project_failure_rolls_back_new_services_and_preserves_existing_ones() {
    let f = Fixture::new(true);
    assert!(f.blocked.is_some());
    f.ok("setup", true, &[]);
    assert!(!f.cli("start", true, &[]).status.success());
    assert!(!f.record("opencode").exists());
    assert!(!f.record("mcp").exists());
    f.ok("start", false, &[]);
    let before = fs::read(f.record("opencode")).unwrap();
    assert!(!f.cli("start", true, &[]).status.success());
    assert_eq!(fs::read(f.record("opencode")).unwrap(), before);
    f.ok("status", false, &["--json"]);
    let all = f.cli("status", true, &["--json"]);
    assert_eq!(all.status.code(), Some(1));
    let reports: Value = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(reports["projects"].as_array().unwrap().len(), 2);
    assert_eq!(reports["projects"][0]["ready"], true);
    assert_eq!(reports["projects"][1]["ready"], false);
    f.ok("stop", true, &[]);
}

#[test]
fn setup_refuses_symlink_credentials_and_foreign_state_without_writes() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new(false);
    let target = f.root.join("untouched");
    fs::write(&target, "unchanged").unwrap();
    symlink(&target, f.root.join("password")).unwrap();
    assert!(!f.cli("setup", false, &[]).status.success());
    assert!(!f.root.join("state").exists());
    assert!(!f.root.join("token").exists());
    assert_eq!(fs::read_to_string(&target).unwrap(), "unchanged");
    fs::remove_file(f.root.join("password")).unwrap();
    fs::create_dir_all(f.root.join("state/proj")).unwrap();
    fs::write(f.root.join("state/proj/foreign"), "unchanged").unwrap();
    assert!(!f.cli("setup", false, &[]).status.success());
    assert!(!f.root.join("password").exists());
    assert!(!f.root.join("token").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("state/proj/foreign")).unwrap(),
        "unchanged"
    );
    assert!(fs::read_dir(f.root.join("main")).unwrap().next().is_none());
}
