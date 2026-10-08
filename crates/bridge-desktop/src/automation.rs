//! Reviewed immutable plans and fixed CLI controls for the desktop.
use crate::projects::ProjectService;
use bridge_automation::{lifecycle, plan::validate_plan};
use bridge_storage::automation::{AutomationRunStore, AutomationStoreError, RunId};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};

pub(crate) struct PendingPlan {
    project: String,
    config: Vec<u8>,
    plan: Value,
}
fn executable() -> Result<PathBuf, String> {
    let path = std::env::var_os("AIBRIDGE_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/agent-bridge")
        });
    if !path.is_absolute() || !path.is_file() {
        return Err("Исполняемый файл agent-bridge недоступен".into());
    }
    Ok(path)
}
impl ProjectService {
    pub fn automation_preview(&self, project: &str, content: &str) -> Result<Value, String> {
        if content.len() > 1_000_000 {
            return Err("План превышает 1 МБ".into());
        }
        let config = self.config_bytes()?;
        let (entry, _) = self.project(project)?;
        let raw: Value =
            serde_json::from_str(content).map_err(|_| "План должен быть корректным JSON")?;
        let plan = validate_plan(&entry, &raw).map_err(|e| e.to_string())?;
        let plan = serde_json::to_value(plan).map_err(|_| "Не удалось подготовить план")?;
        if self.config_bytes()? != config {
            return Err("Конфигурация изменилась; проверьте план заново".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let _activity = self.activity_guard()?;
        let mut pending = self
            .pending_automation
            .lock()
            .map_err(|_| "Просмотр плана недоступен")?;
        pending.retain(|_, p| p.project != project);
        pending.insert(
            id.clone(),
            PendingPlan {
                project: project.into(),
                config,
                plan: plan.clone(),
            },
        );
        Ok(json!({"review_id":id,"plan":plan}))
    }
    pub fn automation_cancel(&self, review_id: &str) {
        if let Ok(mut pending) = self.pending_automation.lock() {
            pending.remove(review_id);
        }
    }
    pub fn automation_start(&self, project: &str, review_id: &str) -> Result<Value, String> {
        self.automation_start_with_cli(project, review_id, &executable()?)
    }
    fn automation_start_with_cli(
        &self,
        project: &str,
        review_id: &str,
        cli: &std::path::Path,
    ) -> Result<Value, String> {
        let _activity = self.activity_guard()?;
        let mut pending = self
            .pending_automation
            .lock()
            .map_err(|_| "Просмотр плана недоступен")?;
        let preview = pending
            .get(review_id)
            .ok_or("Проверьте план перед запуском")?;
        if preview.project != project {
            return Err("План относится к другому проекту".into());
        }
        let _config_lock = self.config_file_guard()?;
        if self.config_bytes()? != preview.config {
            return Err("Конфигурация изменилась; проверьте план заново".into());
        }
        let (_, layout) = self.project(project)?;
        let env = crate::codex_env::parse(&self.read_codex_env(project)?)?;
        let mut command = self.automation_command(project, cli, "launch-codex")?;
        // Store only the reviewed bytes in private Rust state, never a caller path.
        layout.initialize().map_err(|_| "Rust state недоступен")?;
        let path = layout
            .project_dir()
            .join(format!("desktop-plan-{review_id}.json"));
        bridge_config::migration::atomic_write(&path, preview.plan.to_string().as_bytes(), 0o600)?;
        // Consume before spawning: a transport retry cannot submit this plan twice.
        pending.remove(review_id);
        drop(pending);
        command.args(["--auto", "--plan"]).arg(&path).envs(env);
        let result = command.output();
        let _ = fs::remove_file(path);
        parse_output(result)
    }
    fn automation_command(
        &self,
        project: &str,
        cli: &std::path::Path,
        action: &str,
    ) -> Result<Command, String> {
        let (entry, _) = self.project(project)?;
        let mut command = Command::new(cli);
        command
            .args([action, "--project", project, "--config"])
            .arg(&self.config)
            .arg("--state-root")
            .arg(&self.state)
            .current_dir(entry.workspace())
            .stdin(std::process::Stdio::null());
        for key in [
            "OPENCODE_SERVER_PASSWORD",
            "OPENCODE_SERVER_USERNAME",
            "AGENT_BRIDGE_MCP_TOKEN",
        ] {
            command.env_remove(key);
        }
        Ok(command)
    }
    pub fn automation_control(
        &self,
        project: &str,
        run: &str,
        action: &str,
    ) -> Result<Value, String> {
        self.automation_control_with_cli(project, run, action, &executable()?)
    }
    fn automation_control_with_cli(
        &self,
        project: &str,
        run: &str,
        action: &str,
        cli: &std::path::Path,
    ) -> Result<Value, String> {
        let _activity = self.activity_guard()?;
        let run: RunId = run.parse().map_err(|_| "Некорректный запуск")?;
        let action = match action {
            "pause" => "automation-pause",
            "resume" => "automation-resume",
            "stop" => "automation-stop",
            _ => return Err("Неизвестное действие автоматизации".into()),
        };
        let mut command = self.automation_command(project, cli, action)?;
        command.args(["--run", &run.to_string()]);
        if action == "automation-resume" {
            command.envs(crate::codex_env::parse(&self.read_codex_env(project)?)?);
        }
        parse_output(command.output())
    }
    pub fn automation_status(&self, project: &str) -> Result<Value, String> {
        let (entry, layout) = self.project(project)?;
        // A project with no initialized database has no automatic run.
        if !layout.database().exists() {
            return Ok(Value::Null);
        }
        let store = AutomationRunStore::new(layout.clone());
        let run = match store.load(None) {
            Ok(run) => run,
            Err(AutomationStoreError::NotFound) => return Ok(Value::Null),
            Err(_) => return Err("Состояние автоматизации недоступно".into()),
        };
        let live =
            lifecycle::supervisor_running(&layout, &entry, run.id()).map_err(|e| e.to_string())?;
        let doc = run.document();
        let steps: Vec<Value> = doc["steps"].as_array().ok_or("Состояние плана повреждено")?.iter().map(|item|
            json!({"step":item["step"],"phase":item["phase"],"task_id":item["task_id"],"revisions":item["revisions"],"review":item["review"],"findings":item["pending_revision"]["findings"]})
        ).collect();
        Ok(crate::dashboard::sanitize(
            &json!({"run_id":run.id().to_string(),"status":run.status().as_str(),"control":run.control().as_str(),"supervisor_running":live,"goal":doc["plan"]["goal"],"delivery":doc["plan"]["delivery"],"phase":doc["phase"],"index":doc["index"],"steps":steps,"blocker_code":doc["blocker"]["code"],"remote_result":doc["remote_result"]}),
        ))
    }
}
fn parse_output(output: std::io::Result<std::process::Output>) -> Result<Value, String> {
    let output = output.map_err(|_| "Не удалось выполнить команду автоматизации")?;
    if !output.status.success() {
        // CLI automation errors are fixed labels; still apply dashboard redaction.
        let message = String::from_utf8_lossy(&output.stderr);
        let safe = crate::dashboard::sanitize(&json!(message.trim()));
        return Err(safe
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("Ошибка автоматизации; обновите статус запуска")
            .into());
    }
    serde_json::from_slice(&output.stdout).map_err(|_| "Ответ автоматизации недоступен".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    struct Fixture {
        root: PathBuf,
        service: ProjectService,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("desktop-automation-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("repo")).unwrap();
            let config = root.join("projects.toml");
            fs::write(&config, format!("[projects.proof]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file={}\nmax_rounds=3\n", json!(root.join("repo")), json!(root.join("password")))).unwrap();
            let service = ProjectService::new(config, root.join("state")).unwrap();
            Self { root, service }
        }
        fn plan(&self) -> Value {
            json!({"version":1,"goal":"Approved goal","steps":[{"id":"one","task":"Create file","allowed_paths":["file.txt"],"test_commands":["true"],"acceptance_criteria":["File exists"]}],"final_test_commands":["true"],"delivery":"manual"})
        }
        fn preview(&self) -> Value {
            self.service
                .automation_preview("proof", &self.plan().to_string())
                .unwrap()
        }
        fn cli(&self, script: &str) -> PathBuf {
            let path = self.root.join("cli");
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn preview_is_readonly_and_validates_scope_dependencies_and_commands() {
        let f = Fixture::new();
        assert_eq!(f.service.automation_status("proof").unwrap(), Value::Null);
        let preview = f.preview();
        assert_eq!(preview["plan"]["max_seconds"], 86400);
        assert_eq!(preview["plan"]["delivery"], "manual");
        assert!(!f.service.state.exists());
        for (key, value) in [
            ("allowed_paths", json!(["../outside"])),
            ("test_commands", json!(["cargo test; git push"])),
            ("depends_on", json!(["missing"])),
        ] {
            let mut plan = f.plan();
            plan["steps"][0][key] = value;
            assert!(
                f.service
                    .automation_preview("proof", &plan.to_string())
                    .is_err()
            );
        }
        assert!(f.service.automation_preview("proof", "{}").is_err());
        assert!(!f.service.state.exists());
    }
    #[test]
    fn previews_are_bound_cancelable_and_invalidated_by_config_or_replacement() {
        let f = Fixture::new();
        let cli = f.cli("#!/bin/sh\nexit 99\n");
        let first = f.preview();
        let second = f.preview();
        assert!(
            f.service
                .automation_start_with_cli("proof", first["review_id"].as_str().unwrap(), &cli)
                .is_err()
        );
        assert!(
            f.service
                .automation_start_with_cli("other", second["review_id"].as_str().unwrap(), &cli)
                .is_err()
        );
        f.service
            .automation_cancel(second["review_id"].as_str().unwrap());
        assert!(
            f.service
                .automation_start_with_cli("proof", second["review_id"].as_str().unwrap(), &cli)
                .is_err()
        );
        let next = f.preview();
        fs::write(
            &f.service.config,
            format!(
                "{}\n# changed\n",
                fs::read_to_string(&f.service.config).unwrap()
            ),
        )
        .unwrap();
        assert!(
            f.service
                .automation_start_with_cli("proof", next["review_id"].as_str().unwrap(), &cli)
                .unwrap_err()
                .contains("Конфигурация")
        );
        assert!(!f.service.state.exists());
    }
    #[test]
    fn launch_uses_reviewed_private_plan_and_saved_env_once_and_cleans_temp_file() {
        let f = Fixture::new();
        f.service
            .save_codex_env("proof", "DESKTOP_TEST_VALUE='literal $HOME'\n")
            .unwrap();
        let cli = f.cli(
            r#"#!/usr/bin/python3
import json, os, pathlib, stat, sys
args = sys.argv[1:]
assert args[:3] == ['launch-codex', '--project', 'proof']
assert '--auto' in args
plan_path = pathlib.Path(args[args.index('--plan')+1])
assert stat.S_IMODE(plan_path.stat().st_mode) == 0o600
plan = json.loads(plan_path.read_text())
assert plan['goal'] == 'Approved goal'
assert plan['delivery'] == 'manual'
assert os.environ['DESKTOP_TEST_VALUE'] == 'literal $HOME'
pathlib.Path('launch-count').write_text('one')
print(json.dumps({'status':'starting'}))
"#,
        );
        let preview = f.preview();
        let id = preview["review_id"].as_str().unwrap();
        assert_eq!(
            f.service
                .automation_start_with_cli("proof", id, &cli)
                .unwrap()["status"],
            "starting"
        );
        assert!(
            f.service
                .automation_start_with_cli("proof", id, &cli)
                .is_err()
        );
        let (_, layout) = f.service.project("proof").unwrap();
        assert!(
            !layout
                .project_dir()
                .join(format!("desktop-plan-{id}.json"))
                .exists()
        );
        assert_eq!(
            fs::read_to_string(f.root.join("repo/launch-count")).unwrap(),
            "one"
        );
    }
    #[test]
    fn launch_failure_consumes_review_and_removes_private_file() {
        let f = Fixture::new();
        let cli =
            f.cli("#!/bin/sh\necho 'agent-bridge: project has unfinished tasks' >&2\nexit 1\n");
        let preview = f.preview();
        let id = preview["review_id"].as_str().unwrap();
        assert!(
            f.service
                .automation_start_with_cli("proof", id, &cli)
                .unwrap_err()
                .contains("unfinished tasks")
        );
        assert!(
            f.service
                .automation_start_with_cli("proof", id, &cli)
                .is_err()
        );
        let (_, layout) = f.service.project("proof").unwrap();
        assert!(
            !layout
                .project_dir()
                .join(format!("desktop-plan-{id}.json"))
                .exists()
        );
    }
    #[test]
    fn status_returns_bounded_review_evidence_without_private_model_context() {
        let f = Fixture::new();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Proof",
                "-c",
                "user.email=proof@example.test",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(f.root.join("repo"))
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let (entry, layout) = f.service.project("proof").unwrap();
        let run = bridge_automation::run::create_run(&entry, &layout, &f.plan()).unwrap();
        let store = AutomationRunStore::new(layout);
        let mut doc = run.document().clone();
        doc["steps"][0]["prepared_task"] = json!("private model prompt");
        doc["steps"][0]["review"] = json!({"summary":"Needs coverage","findings":["Missing test"],"workspace":"/private/worktree"});
        store
            .save(&doc, bridge_storage::automation::RunStatus::Blocked)
            .unwrap();
        let status = f.service.automation_status("proof").unwrap();
        assert_eq!(status["run_id"], run.id().to_string());
        assert_eq!(status["supervisor_running"], false);
        assert_eq!(status["steps"][0]["review"]["findings"][0], "Missing test");
        assert!(!status.to_string().contains("private model prompt"));
        assert!(!status.to_string().contains("/private/worktree"));
        assert_eq!(status["steps"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn controls_use_explicit_run_and_pause_stop_ignore_invalid_launch_env() {
        let f = Fixture::new();
        let id = uuid::Uuid::new_v4().to_string();
        let cli = f.cli(
            r#"#!/usr/bin/python3
import json, sys
args = sys.argv[1:]
assert args[0] in ['automation-pause', 'automation-resume', 'automation-stop']
assert args[1:3] == ['--project', 'proof']
assert '--run' in args
print(json.dumps({'action':args[0], 'run':args[args.index('--run')+1]}))
"#,
        );
        f.service.save_codex_env("proof", "VALID=value").unwrap();
        let (_, layout) = f.service.project("proof").unwrap();
        fs::write(
            layout.project_dir().join("desktop-codex.env"),
            "1INVALID=secret",
        )
        .unwrap();
        for action in ["pause", "stop"] {
            let result = f
                .service
                .automation_control_with_cli("proof", &id, action, &cli)
                .unwrap();
            assert_eq!(result["run"], id);
            assert_eq!(result["action"], format!("automation-{action}"));
        }
        assert!(
            f.service
                .automation_control_with_cli("proof", &id, "resume", &cli)
                .is_err()
        );
        assert!(
            f.service
                .automation_control_with_cli("proof", &id, "arbitrary", &cli)
                .is_err()
        );
        assert!(
            f.service
                .automation_control_with_cli("proof", "not-a-uuid", "stop", &cli)
                .is_err()
        );
        f.service
            .save_codex_env("proof", "VALID=value")
            .unwrap_err();
        fs::write(
            layout.project_dir().join("desktop-codex.env"),
            "VALID=value",
        )
        .unwrap();
        assert_eq!(
            f.service
                .automation_control_with_cli("proof", &id, "resume", &cli)
                .unwrap()["action"],
            "automation-resume"
        );
    }
}
