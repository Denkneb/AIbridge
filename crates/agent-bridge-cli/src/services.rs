use super::LaunchArgs;
use bridge_config::load_config_with_state_root;
use bridge_storage::RustStateLayout;
use serde_json::json;
use std::{
    ffi::{OsStr, OsString},
    process::ExitCode,
    time::Duration,
};
pub enum Action {
    Hook,
    Attach {
        task: Option<bridge_domain::TaskId>,
        session: bool,
    },
    Status {
        json: bool,
        all: bool,
    },
}
pub fn is_command(s: &OsStr) -> bool {
    matches!(
        s.to_str(),
        Some("status" | "hook-status" | "console" | "attach-opencode")
    )
}
pub fn parse(
    command: &OsStr,
    task: Option<OsString>,
    json: bool,
    all: bool,
) -> Result<Action, &'static str> {
    if command == "console" || command == "attach-opencode" {
        if json || all {
            return Err("unsupported attach flag");
        }
        let task = task
            .map(|s| {
                s.into_string()
                    .map_err(|_| "invalid task")
                    .and_then(|s| s.parse().map_err(|_| "invalid task"))
            })
            .transpose()?;
        let session = command == "attach-opencode";
        if session && task.is_none() {
            return Err("--task required");
        }
        return Ok(Action::Attach { task, session });
    }
    if command == "hook-status" {
        if json || all {
            return Err("unsupported hook flag");
        }
        return Ok(Action::Hook);
    }
    Ok(Action::Status { json, all })
}
pub fn run(args: LaunchArgs, action: Action) -> Result<ExitCode, String> {
    if matches!(action, Action::Hook) {
        let result = (|| -> Result<(), String> {
            let config = load_config_with_state_root(&args.config, &args.state_root)
                .map_err(|_| "config unavailable")?;
            let project = config
                .project(&args.project)
                .ok_or("project not configured")?;
            let layout = RustStateLayout::new(args.state_root, project.id().clone())
                .map_err(|_| "state invalid")?;
            let briefs =
                bridge_runtime::diagnostics::briefs(&layout).map_err(|_| "state unavailable")?;
            if let Some(context) =
                bridge_runtime::hook::context(&briefs).map_err(|_| "hook unavailable")?
            {
                println!("{context}");
            }
            Ok(())
        })();
        let _ = result;
        return Ok(ExitCode::SUCCESS);
    }
    let config = load_config_with_state_root(&args.config, &args.state_root)
        .map_err(|_| "config unavailable")?;
    if let Action::Attach { task, session } = action {
        use std::os::unix::process::ExitStatusExt;
        let project = config
            .project(&args.project)
            .ok_or("project not configured")?;
        let layout = RustStateLayout::new(args.state_root, project.id().clone())
            .map_err(|_| "state invalid")?;
        let target = bridge_runtime::attachment::resolve(
            project,
            &layout,
            task,
            session,
            Duration::from_secs(2),
        )
        .map_err(|e| e.to_string())?;
        let status = target
            .launch(&bridge_runtime::ServerCommand::opencode())
            .map_err(|e| e.to_string())?;
        return Ok(ExitCode::from(
            u8::try_from(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
            )
            .unwrap_or(1),
        ));
    }
    let Action::Status { json, all } = action else {
        unreachable!()
    };
    let projects = if all {
        config.projects().values().collect::<Vec<_>>()
    } else {
        vec![
            config
                .project(&args.project)
                .ok_or("project not configured")?,
        ]
    };
    let mut reports = vec![];
    for p in projects {
        let layout = RustStateLayout::new(args.state_root.clone(), p.id().clone())
            .map_err(|_| "state binding invalid")?;
        reports.push(bridge_runtime::diagnostics::status(
            p,
            &layout,
            Duration::from_millis(300),
        ));
    }
    let ready = reports.iter().all(|r| r["ready"] == true);
    if json {
        println!("{}", json!({"schema_version":1,"projects":reports}));
    } else {
        for r in reports {
            println!(
                "{}: {}",
                r["project_id"].as_str().unwrap(),
                if r["ready"] == true {
                    "ready"
                } else {
                    "not ready"
                }
            );
        }
    }
    Ok(if ready {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
