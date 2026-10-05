use bridge_automation::run::create_run;
use bridge_config::load_config_with_state_root;
use bridge_storage::{
    RustStateLayout,
    automation::{AutomationRunStore, RunControl, RunStatus},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{Duration, Instant},
};
struct Fixture {
    root: PathBuf,
    project: bridge_config::ProjectEntry,
    layout: RustStateLayout,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bridge-cli-auto-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let path = root.join("projects.toml");
        fs::write(&path,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4999\"\npassword_file=\"unused\"\nmax_rounds=3\n",json!(workspace))).unwrap();
        let config = load_config_with_state_root(&path, &root.join("state")).unwrap();
        let project = config.project("proj").unwrap().clone();
        let layout = RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=f@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&workspace)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        Self {
            root,
            project,
            layout,
        }
    }
    fn cli(&self, command: &str, extra: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .arg(command)
            .args(["--project", "proj", "--config"])
            .arg(self.project.source_path())
            .arg("--state-root")
            .arg(self.layout.state_root())
            .args(extra)
            .output()
            .unwrap()
    }
    fn plan(&self) -> Value {
        json!({"version":1,"goal":"approved SECRET_GOAL","steps":[{"id":"one","task":"approved SECRET_TASK","allowed_paths":["module.py"],"test_commands":["python3 -B module.py"],"acceptance_criteria":["works"]}],"final_test_commands":["python3 -B module.py"]})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn flags_and_plan_file_are_validated_before_runtime_mutation() {
    let f = Fixture::new();
    for args in [
        vec!["--auto"],
        vec!["--plan", "missing"],
        vec!["--auto", "--auto", "--plan", "missing"],
        vec!["--auto", "--plan", "missing"],
    ] {
        assert!(!f.cli("launch-codex", &args).status.success());
        assert!(!f.layout.state_root().exists());
    }
    let path = f.root.join("plan.json");
    fs::write(&path, vec![b' '; 1_000_001]).unwrap();
    assert!(
        !f.cli(
            "launch-codex",
            &["--auto", "--plan", path.to_str().unwrap()]
        )
        .status
        .success()
    );
    fs::write(&path, "{}").unwrap();
    assert!(
        !f.cli(
            "launch-codex",
            &["--auto", "--plan", path.to_str().unwrap()]
        )
        .status
        .success()
    );
    assert!(!f.layout.state_root().exists());
    assert!(!f.cli("automation-worker", &[]).status.success());
    assert!(!f.cli("automation-status", &[]).status.success());
    assert!(!f.layout.state_root().exists());
}
#[test]
fn readonly_status_pause_and_detached_stop_without_model_invocation() {
    let f = Fixture::new();
    let run = create_run(&f.project, &f.layout, &f.plan()).unwrap();
    let store = AutomationRunStore::new(f.layout.clone());
    let output = f.cli("automation-status", &[]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("SECRET"));
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["run_id"], run.id().to_string());
    assert!(f.cli("automation-pause", &[]).status.success());
    assert_eq!(
        store.load(Some(run.id())).unwrap().control(),
        RunControl::Pause
    );
    assert!(
        !f.cli("automation-worker", &["--run", &run.id().to_string()])
            .status
            .success()
    );
    let started = Instant::now();
    let output = f.cli("automation-stop", &[]);
    assert!(output.status.success(), "{:?}", output);
    assert!(started.elapsed() < Duration::from_secs(5));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if store.load(Some(run.id())).unwrap().status() == RunStatus::Stopped {
            break;
        }
        assert!(Instant::now() < deadline, "supervisor did not stop");
        std::thread::sleep(Duration::from_millis(30));
    }
    for command in ["automation-resume", "automation-pause", "automation-stop"] {
        assert!(!f.cli(command, &[]).status.success());
    }
    assert!(
        !bridge_automation::lifecycle::directory(&f.layout, run.id())
            .join("codex")
            .exists()
    );
}
