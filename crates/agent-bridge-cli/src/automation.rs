//! Explicit automation controls. Status emits metadata, never model prompts or logs.
use super::{LaunchArgs, worker_spawner};
use bridge_automation::{
    codex::{CodexClient, read_bounded},
    coordinator::Coordinator,
    lifecycle::{self, LaunchMode},
    run::create_run,
};
use bridge_config::load_config_with_state_root;
use bridge_storage::{
    RustStateLayout,
    automation::{AutomationRunStore, RunControl, RunId},
};
use serde_json::{Value, json};
use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};
pub enum Action {
    Start(PathBuf),
    Status(Option<RunId>),
    Pause(Option<RunId>),
    Resume(Option<RunId>),
    Stop(Option<RunId>),
    Worker(RunId),
}
pub fn is_command(s: &OsStr) -> bool {
    matches!(
        s.to_str(),
        Some(
            "launch-codex"
                | "automation-status"
                | "automation-pause"
                | "automation-resume"
                | "automation-stop"
                | "automation-worker"
        )
    )
}
pub fn parse(
    command: &OsStr,
    auto: bool,
    plan: Option<OsString>,
    run: Option<OsString>,
) -> Result<Action, &'static str> {
    let id = run
        .map(|s| {
            s.into_string()
                .map_err(|_| "invalid run identity")
                .and_then(|s| s.parse().map_err(|_| "invalid run identity"))
        })
        .transpose()?;
    Ok(match command.to_str().ok_or("invalid command")? {
        "launch-codex" => match (auto, plan) {
            (true, Some(p)) => Action::Start(p.into()),
            (true, None) => return Err("--auto requires --plan"),
            (false, Some(_)) => return Err("--plan requires --auto"),
            (false, None) => {
                return Err("--auto --plan required; interactive Codex controller not implemented");
            }
        },
        "automation-status" => Action::Status(id),
        "automation-pause" => Action::Pause(id),
        "automation-resume" => Action::Resume(id),
        "automation-stop" => Action::Stop(id),
        "automation-worker" => Action::Worker(id.ok_or("--run required")?),
        _ => return Err("unsupported command"),
    })
}
pub fn run(args: LaunchArgs, action: Action) -> Result<ExitCode, String> {
    let path = std::fs::canonicalize(&args.config).map_err(|_| "config unavailable")?;
    let config = load_config_with_state_root(&path, &args.state_root).map_err(|e| e.to_string())?;
    let project = config
        .project(&args.project)
        .ok_or("project not configured")?;
    let layout =
        RustStateLayout::new(args.state_root, project.id().clone()).map_err(|e| e.to_string())?;
    let store = AutomationRunStore::new(layout.clone());
    let executable = std::env::current_exe().map_err(|_| "bridge executable unavailable")?;
    let report = match action {
        Action::Start(plan) => {
            let bytes = read_bounded(&plan, 1_000_000)
                .map_err(|_| "approved plan unavailable or exceeds 1MB")?;
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|_| "approved plan is not JSON")?;
            let run = create_run(project, &layout, &value).map_err(|e| e.to_string())?;
            lifecycle::launch(&layout, project, run.id(), &executable, false)
                .map_err(|e| e.to_string())?
        }
        Action::Worker(id) => {
            let guard =
                lifecycle::acquire_supervisor(&layout, project, id).map_err(|e| e.to_string())?;
            let run = store.load(Some(id)).map_err(|e| e.to_string())?;
            let timeout = run.document()["plan"]["codex_timeout"]
                .as_u64()
                .ok_or("automation plan invalid")?;
            let model = run.document()["plan"]["codex_model"]
                .as_str()
                .map(str::to_owned);
            let client = CodexClient::new(
                lifecycle::directory(&layout, id).join("codex"),
                Duration::from_secs(timeout),
                model,
            )
            .map_err(|e| e.to_string())?;
            let mut coordinator = Coordinator::with_guard(
                project.clone(),
                layout.clone(),
                id,
                worker_spawner(project, &layout, &path)?,
                config.projects().values().cloned().collect(),
                client,
                guard,
            )
            .map_err(|e| e.to_string())?;
            coordinator.run().map_err(|e| e.to_string())?;
            status(&store.load(Some(id)).map_err(|e| e.to_string())?, false)
        }
        Action::Status(id) => {
            let run = store.load(id).map_err(|e| e.to_string())?;
            status(
                &run,
                lifecycle::supervisor_running(&layout, project, run.id())
                    .map_err(|e| e.to_string())?,
            )
        }
        Action::Resume(id) => {
            let run = store.load(id).map_err(|e| e.to_string())?;
            lifecycle::launch(&layout, project, run.id(), &executable, true)
                .map_err(|e| e.to_string())?
        }
        Action::Pause(id) => {
            let run = store.load(id).map_err(|e| e.to_string())?;
            let run = store
                .set_control(run.id(), RunControl::Pause)
                .map_err(|e| e.to_string())?;
            status(
                &run,
                lifecycle::supervisor_running(&layout, project, run.id())
                    .map_err(|e| e.to_string())?,
            )
        }
        Action::Stop(id) => {
            let run = store.load(id).map_err(|e| e.to_string())?;
            let run = store
                .set_control(run.id(), RunControl::Stop)
                .map_err(|e| e.to_string())?;
            if lifecycle::supervisor_running(&layout, project, run.id())
                .map_err(|e| e.to_string())?
            {
                status(&run, true)
            } else {
                lifecycle::launch_command_mode(
                    &layout,
                    project,
                    run.id(),
                    &executable,
                    vec![],
                    LaunchMode::Stop,
                )
                .map_err(|e| e.to_string())?
            }
        }
    };
    println!("{report}");
    Ok(ExitCode::SUCCESS)
}
fn status(run: &bridge_storage::automation::AutomationRun, live: bool) -> Value {
    let doc = run.document();
    let i = doc["index"].as_u64().unwrap_or(0) as usize;
    json!({"run_id":run.id().to_string(),"status":run.status().as_str(),"control":run.control().as_str(),"supervisor_running":live,"phase":doc["phase"],"index":i,"steps":doc["steps"].as_array().map(Vec::len),"final_task_id":doc["steps"].as_array().and_then(|s|s.last()).map(|s|s["task_id"].clone()),"current_step":doc["steps"][i]["step"]["id"],"task_id":doc["steps"][i]["task_id"],"blocker_code":doc["blocker"]["code"],"elapsed":doc["elapsed"]})
}
