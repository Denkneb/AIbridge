//! Interactive Codex controller: process-local MCP wiring, rules and read-only hooks.
use crate::controller::{
    CONTROLLER_PROMPT, ControllerError, check_bindings, controller_env, lock_codex_controller,
};
use bridge_config::ProjectEntry;
use bridge_storage::RustStateLayout;
use std::{
    ffi::OsString,
    path::Path,
    process::{Command, ExitStatus},
};
type Result<T> = std::result::Result<T, ControllerError>;
const TOOLS: [&str; 7] = [
    "project_info",
    "submit_task",
    "task_status",
    "set_task_status",
    "request_changes",
    "accept_task",
    "close_task",
];
fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization")
}
fn path(value: &Path) -> Result<&str> {
    value.to_str().ok_or(ControllerError::Binding)
}
fn shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
/// All values are individual argv elements; credentials occur only in child env.
/// # Errors
/// Rejects unsafe state/project bindings and non-absolute executable/config paths.
pub fn build_codex_args(
    primary: &ProjectEntry,
    linked: &[&ProjectEntry],
    layout: &RustStateLayout,
    bridge_exe: &Path,
    config_path: &Path,
) -> Result<Vec<OsString>> {
    check_bindings(primary, linked, layout)?;
    if !bridge_exe.is_absolute() || !config_path.is_absolute() {
        return Err(ControllerError::Binding);
    }
    let executable = path(bridge_exe)?;
    let config = path(config_path)?;
    let state = path(layout.state_root())?;
    let mut args: Vec<OsString> = vec![
        "-C".into(),
        primary.workspace().as_os_str().into(),
        "--disable".into(),
        "multi_agent".into(),
    ];
    let mut overrides = vec![
        format!(
            "developer_instructions={}",
            quoted(&format!(
                "Правила постоянны (включая /new).\n{}",
                CONTROLLER_PROMPT.replace(
                    "тебе запрещены конфигурацией",
                    "тебе запрещены этими постоянными правилами"
                )
            ))
        ),
        "sandbox_mode=\"read-only\"".to_owned(),
    ];
    let tools = TOOLS
        .iter()
        .map(|name| format!("{name}={{approval_mode=\"approve\"}}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut hooks = Vec::new();
    for (index, project) in std::iter::once(primary)
        .chain(linked.iter().copied())
        .enumerate()
    {
        let name = if index == 0 {
            "agent_bridge".to_owned()
        } else {
            format!("agent_bridge_{}", project.id())
        };
        let entry = if let Some(endpoint) = project.mcp_endpoint() {
            let var = if index == 0 {
                "AGENT_BRIDGE_MCP_TOKEN".to_owned()
            } else {
                format!(
                    "AGENT_BRIDGE_MCP_TOKEN_{}",
                    project.id().as_str().to_ascii_uppercase().replace('-', "_")
                )
            };
            format!(
                "{{url={},bearer_token_env_var={},required=true,tool_timeout_sec=330,tools={{{tools}}}}}",
                quoted(&endpoint.url()),
                quoted(&var)
            )
        } else {
            let argv = [
                "mcp",
                "--project",
                project.id().as_str(),
                "--config",
                config,
                "--state-root",
                state,
            ];
            format!(
                "{{command={},args={},required=true,tool_timeout_sec=330,tools={{{tools}}}}}",
                quoted(executable),
                serde_json::to_string(&argv).expect("argv serialization")
            )
        };
        overrides.push(format!("mcp_servers.{name}={entry}"));
        let argv = [
            executable,
            "hook-status",
            "--project",
            project.id().as_str(),
            "--config",
            config,
            "--state-root",
            state,
        ];
        let command = argv.iter().map(|s| shell(s)).collect::<Vec<_>>().join(" ");
        hooks.push(format!(
            "{{type=\"command\",command={},timeout=5}}",
            quoted(&command)
        ));
    }
    overrides.push(format!(
        "hooks.UserPromptSubmit=[{{hooks=[{}]}}]",
        hooks.join(",")
    ));
    for value in overrides {
        args.extend(["-c".into(), value.into()]);
    }
    let console = [
        executable,
        "console",
        "--project",
        primary.id().as_str(),
        "--config",
        config,
        "--state-root",
        state,
    ]
    .iter()
    .map(|s| shell(s))
    .collect::<Vec<_>>()
    .join(" ");
    let dashboard = [
        "aibridge-desktop",
        "--config",
        config,
        "--state-root",
        state,
    ]
    .iter()
    .map(|s| shell(s))
    .collect::<Vec<_>>()
    .join(" ");
    args.push(format!("Начни с agent_bridge.project_info для проекта {}, сверь workspace, затем task_status без ID. Покажи состояние и команды подключения:\nconsole:\n{console}\ndashboard (read-only AIbridge desktop):\n{dashboard}\nПока нет task/session, показывай console без --task. Предложи конкретный план; реализацию передавай через мост после согласования.", primary.id()).into());
    Ok(args)
}
/// Inherits provider environment and stdio, keeps the controller fence until exit.
/// # Errors
/// Missing tokens and invalid bindings fail before state initialization. Foreign state is refused.
pub fn launch_codex(
    primary: &ProjectEntry,
    linked: &[&ProjectEntry],
    layout: &RustStateLayout,
    bridge_exe: &Path,
    config_path: &Path,
) -> Result<ExitStatus> {
    let args = build_codex_args(primary, linked, layout, bridge_exe, config_path)?;
    let env = controller_env(primary, linked, None, std::env::vars_os())?;
    let _guard = lock_codex_controller(layout)?;
    Command::new("codex")
        .args(args)
        .current_dir(primary.workspace())
        .env_clear()
        .envs(env)
        .status()
        .map_err(|_| ControllerError::Spawn)
}
