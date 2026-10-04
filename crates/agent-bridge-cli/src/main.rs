//! Runtime CLI. Implemented commands require explicit Rust configuration/state.
use bridge_config::load_config_with_state_root;
use bridge_domain::TaskId;
use bridge_runtime::controller::{ControllerCommand, launch_controller};
use bridge_storage::{RoundRef, RustStateLayout};
use bridge_worker::runner::{WorkerOutcome, WorkerSettings, run_worker};
use std::os::unix::process::ExitStatusExt;
use std::{env, ffi::OsString, path::PathBuf, process::ExitCode};

const HELP: &str = "agent-bridge COMMAND --project ID --config PATH --state-root ABSOLUTE_PATH

Commands:
  worker            Run an existing task round (--task ID --round N required)
  launch-opencode   Launch an OpenCode controller using existing HTTP MCP servers
  mcp               Serve MCP over stdin/stdout
  serve-mcp         Serve authenticated MCP on the configured loopback HTTP endpoint

Rust MCP exposes project_info and standalone delegated tasks with manual review.
The state root must belong to Rust and be outside the bound project workspace.
Local controller delegation is not yet enabled in launch-opencode.

Options:
  --project ID        Configured project id (required)
  --config PATH       projects.toml path (required)
  --state-root PATH   Separate absolute Rust state root (required)
  --task ID           Task UUID (worker only, required)
  --round N           Positive round number (worker only, required)
  -h, --help          Show help
  -V, --version       Show version";

struct LaunchArgs {
    project: String,
    config: PathBuf,
    state_root: PathBuf,
}
enum Action {
    Help,
    Version,
    Launch(LaunchArgs),
    Mcp(LaunchArgs),
    ServeMcp(LaunchArgs),
    Worker(LaunchArgs, TaskId, u32),
}

fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Action, &'static str> {
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Err("command required; use --help");
    };
    if command == "--help" || command == "-h" {
        return if args.next().is_none() {
            Ok(Action::Help)
        } else {
            Err("unexpected arguments after help")
        };
    }
    if command == "--version" || command == "-V" {
        return if args.next().is_none() {
            Ok(Action::Version)
        } else {
            Err("unexpected arguments after version")
        };
    }
    if command != "launch-opencode"
        && command != "mcp"
        && command != "serve-mcp"
        && command != "worker"
    {
        return Err("unsupported command; use --help");
    }
    let (mut project, mut config, mut state_root, mut task, mut round) =
        (None, None, None, None, None);
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(Action::Help);
        }
        // Preserve non-UTF-8 path values in separated form. Option names and
        // project ids are text; no caller input is echoed in diagnostics.
        let (flag, inline) = match arg.to_str() {
            Some(text) => match text.split_once('=') {
                Some((flag, value)) => (flag, Some(OsString::from(value))),
                None => (text, None),
            },
            None => return Err("invalid option encoding"),
        };
        let slot = match flag {
            "--project" => &mut project,
            "--config" => &mut config,
            "--state-root" => &mut state_root,
            "--task" if command == "worker" => &mut task,
            "--round" if command == "worker" => &mut round,
            _ => return Err("unknown option; use --help"),
        };
        if slot.is_some() {
            return Err("duplicate option");
        }
        let value = inline
            .or_else(|| args.next())
            .ok_or("option value required")?;
        if value.is_empty() || value.as_encoded_bytes().starts_with(b"--") {
            return Err("option value required");
        }
        *slot = Some(value);
    }
    let project = project
        .ok_or("--project required")?
        .into_string()
        .map_err(|_| "invalid project encoding")?;
    let config = PathBuf::from(config.ok_or("--config required")?);
    let state_root =
        PathBuf::from(state_root.ok_or("--state-root required; use a separate Rust state root")?);
    if !state_root.is_absolute() {
        return Err("--state-root must be absolute");
    }
    let common = LaunchArgs {
        project,
        config,
        state_root,
    };
    Ok(if command == "worker" {
        let task = task
            .ok_or("--task required")?
            .into_string()
            .map_err(|_| "invalid task encoding")?
            .parse::<TaskId>()
            .map_err(|_| "invalid task id")?;
        let round = round
            .ok_or("--round required")?
            .into_string()
            .map_err(|_| "invalid round encoding")?
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or("invalid round number")?;
        Action::Worker(common, task, round)
    } else if command == "mcp" {
        Action::Mcp(common)
    } else if command == "serve-mcp" {
        Action::ServeMcp(common)
    } else {
        Action::Launch(common)
    })
}
fn launch(args: LaunchArgs) -> Result<ExitCode, String> {
    let config_path =
        std::fs::canonicalize(&args.config).map_err(|_| "config unavailable".to_owned())?;
    let config = load_config_with_state_root(&config_path, &args.state_root)
        .map_err(|error| error.to_string())?;
    let primary = config
        .project(&args.project)
        .ok_or("project not configured")?;
    let layout =
        RustStateLayout::new(args.state_root, primary.id().clone()).map_err(|e| e.to_string())?;
    let executable = env::current_exe().map_err(|_| "bridge executable unavailable")?;
    let status = launch_controller(
        primary,
        &config.linked_projects(&args.project),
        &layout,
        &executable,
        &config_path,
        &ControllerCommand::opencode(),
    )
    .map_err(|error| error.to_string())?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}
fn mcp(args: LaunchArgs, http: bool) -> Result<ExitCode, String> {
    let config_path = std::fs::canonicalize(&args.config).map_err(|_| "config unavailable")?;
    let config =
        load_config_with_state_root(&config_path, &args.state_root).map_err(|e| e.to_string())?;
    let project = config
        .project(&args.project)
        .ok_or("project not configured")?;
    let layout =
        RustStateLayout::new(args.state_root, project.id().clone()).map_err(|e| e.to_string())?;
    let executable = env::current_exe().map_err(|_| "bridge executable unavailable")?;
    let worker_layout = layout.clone();
    let workspace = project.workspace().to_owned();
    let worker_project = project.id().clone();
    let spawner: bridge_mcp::WorkerSpawner = std::sync::Arc::new(move |round| {
        let invocation = bridge_worker::WorkerInvocation::new(
            &executable,
            worker_project.clone(),
            &config_path,
            worker_layout.state_root(),
            round.task_id,
            round.round_number,
        )
        .map_err(|_| ())?;
        let launch_layout = worker_layout.clone();
        let launch_workspace = workspace.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("bridge-worker-reaper".into())
            .spawn(move || {
                match bridge_worker::spawn_worker(&invocation, &launch_layout, &launch_workspace) {
                    Ok(mut child) => {
                        let _ = tx.send(Ok(()));
                        let _ = child.wait();
                    }
                    Err(_) => {
                        let _ = tx.send(Err(()));
                    }
                }
            })
            .map_err(|_| ())?;
        rx.recv().map_err(|_| ())??;
        Ok(())
    });
    let registry = config.projects().values().cloned().collect();
    if http {
        bridge_mcp::http::HttpServer::bind_with_workers(project.clone(), layout, spawner, registry)
            .and_then(|server| server.run())
            .map_err(|e| e.to_string())?;
        return Ok(ExitCode::SUCCESS);
    }
    let server = bridge_mcp::McpServer::open(project.clone(), layout)
        .and_then(|server| server.with_workers(spawner, registry))
        .map_err(|e| e.to_string())?;
    bridge_mcp::stdio::run(&server, std::io::stdin().lock(), std::io::stdout().lock())
        .map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}
fn worker(args: LaunchArgs, task_id: TaskId, round_number: u32) -> Result<ExitCode, String> {
    let config_path = std::fs::canonicalize(&args.config).map_err(|_| "config unavailable")?;
    let config =
        load_config_with_state_root(&config_path, &args.state_root).map_err(|e| e.to_string())?;
    let project = config
        .project(&args.project)
        .ok_or("project not configured")?;
    let layout = RustStateLayout::new(args.state_root.clone(), project.id().clone())
        .map_err(|e| e.to_string())?;
    let projects = config.projects().values().collect::<Vec<_>>();
    let all_layouts = projects
        .iter()
        .map(|p| RustStateLayout::new(args.state_root.clone(), p.id().clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let layouts = all_layouts.iter().collect::<Vec<_>>();
    let settings =
        WorkerSettings::from_lookup(|name| env::var(name).ok()).map_err(|e| e.to_string())?;
    match run_worker(
        &layout,
        project,
        RoundRef {
            task_id,
            project_id: project.id().clone(),
            round_number,
        },
        &layouts,
        &projects,
        &bridge_runtime::ServerCommand::opencode(),
        bridge_runtime::RuntimeOptions::default(),
        settings,
    ) {
        Ok(WorkerOutcome::Busy) => {
            eprintln!("agent-bridge: worker lock busy");
            Ok(ExitCode::from(3))
        }
        Ok(_) => Ok(ExitCode::SUCCESS),
        Err(e) => {
            eprintln!("agent-bridge: {e}");
            Ok(ExitCode::from(e.exit_code()))
        }
    }
}
fn main() -> ExitCode {
    match parse(env::args_os().skip(1)) {
        Ok(Action::Help) => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Action::Version) => {
            println!("agent-bridge {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Action::Launch(args)) => finish(launch(args)),
        Ok(Action::Mcp(args)) => finish(mcp(args, false)),
        Ok(Action::ServeMcp(args)) => finish(mcp(args, true)),
        Ok(Action::Worker(args, task, round)) => finish(worker(args, task, round)),
        Err(error) => {
            eprintln!("agent-bridge: {error}");
            ExitCode::from(2)
        }
    }
}
fn finish(result: Result<ExitCode, String>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("agent-bridge: {error}");
            ExitCode::FAILURE
        }
    }
}
