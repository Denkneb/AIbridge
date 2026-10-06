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
    Status { json: bool, all: bool },
}
pub fn is_command(s: &OsStr) -> bool {
    s == "status"
}
pub fn parse(
    _: &OsStr,
    _: Option<OsString>,
    json: bool,
    all: bool,
) -> Result<Action, &'static str> {
    Ok(Action::Status { json, all })
}
pub fn run(args: LaunchArgs, action: Action) -> Result<ExitCode, String> {
    let config = load_config_with_state_root(&args.config, &args.state_root)
        .map_err(|_| "config unavailable")?;
    let Action::Status { json, all } = action;
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
