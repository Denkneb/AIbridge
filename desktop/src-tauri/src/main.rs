use bridge_desktop::{
    dashboard::Query,
    projects::{ProjectDraft, ProjectService},
    terminal::{Event, Terminals},
};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};
use tauri::{Manager, State};
struct AppState {
    projects: Arc<ProjectService>,
    terminals: Arc<Terminals>,
}
#[tauri::command]
async fn projects(state: State<'_, AppState>) -> Result<Vec<ProjectDraft>, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.projects().map_err(str::to_owned))
        .await
        .map_err(|_| "project query failed".to_owned())?
}
#[tauri::command]
async fn dashboard(state: State<'_, AppState>, query: Query) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.dashboard(query).map_err(str::to_owned))
        .await
        .map_err(|_| "dashboard query failed".to_owned())?
}
#[tauri::command]
async fn dashboard_revision(
    state: State<'_, AppState>,
    project: String,
    linked: bool,
) -> Result<String, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        p.dashboard_revision(&project, linked)
            .map_err(str::to_owned)
    })
    .await
    .map_err(|_| "dashboard revision failed".to_owned())?
}
#[tauri::command]
async fn project_preview(
    state: State<'_, AppState>,
    draft: ProjectDraft,
    password: Option<String>,
    token: Option<String>,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        p.preview(draft, password, token).map_err(str::to_owned)
    })
    .await
    .map_err(|_| "preview failed".to_owned())?
}
#[tauri::command]
async fn project_apply(state: State<'_, AppState>, review_id: String) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.apply(&review_id).map_err(str::to_owned))
        .await
        .map_err(|_| "save failed".to_owned())?
}
#[tauri::command]
fn project_cancel(state: State<'_, AppState>, review_id: String) {
    state.projects.cancel(&review_id);
}
#[tauri::command]
async fn lifecycle(
    state: State<'_, AppState>,
    project: String,
    action: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        p.lifecycle(&project, &action).map_err(str::to_owned)
    })
    .await
    .map_err(|_| "lifecycle action failed".to_owned())?
}
fn terminal_command(
    p: &ProjectService,
    project: &str,
    profile: &str,
    task: Option<String>,
) -> Result<portable_pty::CommandBuilder, String> {
    let (entry, _) = p.project(project).map_err(str::to_owned)?;
    let executable = std::env::var_os("AIBRIDGE_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/agent-bridge")
        });
    let mut cmd = match profile {
        "shell" => {
            if task.is_some() {
                return Err("shell task binding is unsupported".into());
            }
            let mut c = portable_pty::CommandBuilder::new("/bin/bash");
            c.args(["--noprofile", "--norc"]);
            c
        }
        "opencode" | "codex" | "attach" => {
            if !executable.is_absolute() || !executable.is_file() {
                return Err("agent-bridge executable unavailable".into());
            }
            let mut c = portable_pty::CommandBuilder::new(executable);
            c.arg(if profile == "attach" {
                "attach-opencode"
            } else if profile == "codex" {
                "launch-codex"
            } else {
                "launch-opencode"
            });
            c.args(["--project", project, "--config"]);
            c.arg(&p.config);
            c.arg("--state-root");
            c.arg(&p.state);
            if let Some(task) = task {
                task.parse::<bridge_domain::TaskId>()
                    .map_err(|_| "invalid task")?;
                if profile != "attach" {
                    return Err("task requires attach profile".into());
                }
                c.args(["--task", &task]);
            } else if profile == "attach" {
                return Err("task required".into());
            }
            c
        }
        _ => return Err("unsupported terminal profile".into()),
    };
    cmd.cwd(entry.workspace());
    cmd.env("TERM", "xterm-256color");
    for key in [
        "OPENCODE_SERVER_PASSWORD",
        "OPENCODE_SERVER_USERNAME",
        "AGENT_BRIDGE_MCP_TOKEN",
    ] {
        cmd.env_remove(key);
    }
    Ok(cmd)
}
#[tauri::command]
async fn terminal_external(
    state: State<'_, AppState>,
    project: String,
    profile: String,
    task: Option<String>,
) -> Result<(), String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cmd = terminal_command(&p, &project, &profile, task)?;
        let mut external = std::process::Command::new("x-terminal-emulator");
        external
            .arg("-e")
            .args(cmd.get_argv())
            .current_dir(cmd.get_cwd().ok_or("terminal directory unavailable")?)
            .env("TERM", "xterm-256color");
        for key in [
            "OPENCODE_SERVER_PASSWORD",
            "OPENCODE_SERVER_USERNAME",
            "AGENT_BRIDGE_MCP_TOKEN",
        ] {
            external.env_remove(key);
        }
        let mut child = external
            .spawn()
            .map_err(|_| "external terminal unavailable: install x-terminal-emulator".to_owned())?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    })
    .await
    .map_err(|_| "external terminal launch failed".to_owned())?
}
#[tauri::command]
async fn terminal_open(
    state: State<'_, AppState>,
    project: String,
    profile: String,
    task: Option<String>,
    rows: u16,
    cols: u16,
) -> Result<String, String> {
    let p = state.projects.clone();
    let t = state.terminals.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cmd = terminal_command(&p, &project, &profile, task)?;
        t.open(cmd, rows, cols).map_err(str::to_owned)
    })
    .await
    .map_err(|_| "terminal launch failed".to_owned())?
}
#[tauri::command]
fn terminal_read(state: State<'_, AppState>, session: String) -> Result<Vec<Event>, String> {
    state.terminals.read(&session).map_err(str::to_owned)
}
#[tauri::command]
fn terminal_write(
    state: State<'_, AppState>,
    session: String,
    bytes: Vec<u8>,
) -> Result<(), String> {
    state
        .terminals
        .write(&session, bytes)
        .map_err(str::to_owned)
}
#[tauri::command]
async fn terminal_resize(
    state: State<'_, AppState>,
    session: String,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    let t = state.terminals.clone();
    tauri::async_runtime::spawn_blocking(move || {
        t.resize(&session, rows, cols).map_err(str::to_owned)
    })
    .await
    .map_err(|_| "resize failed".to_owned())?
}
#[tauri::command]
async fn terminal_close(state: State<'_, AppState>, session: String) -> Result<(), String> {
    let t = state.terminals.clone();
    tauri::async_runtime::spawn_blocking(move || t.close(&session).map_err(str::to_owned))
        .await
        .map_err(|_| "terminal cleanup failed".to_owned())?
}
#[tauri::command]
async fn smoke_complete(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    passed: bool,
    checks: Value,
) -> Result<(), String> {
    if !cfg!(feature = "desktop-smoke") {
        return Err("smoke build required".into());
    }
    let t = state.terminals.clone();
    tauri::async_runtime::spawn_blocking(move || t.close_all())
        .await
        .map_err(|_| "smoke cleanup failed")?;
    let path = std::env::var_os("AIBRIDGE_DESKTOP_SMOKE_RESULT").ok_or("smoke output required")?;
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({"passed":passed,"checks":checks}))
            .map_err(|_| "smoke serialization failed")?,
    )
    .map_err(|_| "smoke output failed")?;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        app.exit(if passed { 0 } else { 1 });
    });
    Ok(())
}
fn main() {
    let mut args = std::env::args_os().skip(1);
    let (mut config, mut state) = (None, None);
    while let Some(flag) = args.next() {
        let value = args.next().expect("option value required");
        if flag == "--config" {
            config = Some(PathBuf::from(value));
        } else if flag == "--state-root" {
            state = Some(PathBuf::from(value));
        } else {
            eprintln!("unsupported desktop option");
            std::process::exit(2);
        }
    }
    let config = config.expect("--config required");
    let state = state.expect("--state-root required");
    let service = ProjectService::new(config, state).expect("desktop config invalid");
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            projects: Arc::new(service),
            terminals: Arc::new(Terminals::default()),
        })
        .invoke_handler(tauri::generate_handler![
            projects,
            dashboard,
            dashboard_revision,
            project_preview,
            project_apply,
            project_cancel,
            lifecycle,
            terminal_open,
            terminal_external,
            terminal_read,
            terminal_write,
            terminal_resize,
            terminal_close,
            smoke_complete
        ])
        .on_page_load(|window, payload| {
            #[cfg(feature = "desktop-smoke")]
            if std::env::var_os("AIBRIDGE_DESKTOP_SMOKE_RESULT").is_some()
                && matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
            {
                let _ = window.eval(include_str!("smoke.js"));
            }
            #[cfg(not(feature = "desktop-smoke"))]
            let _ = (window, payload);
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let terminals = window.state::<AppState>().terminals.clone();
                let app = window.app_handle().clone();
                std::thread::spawn(move || {
                    terminals.close_all();
                    app.exit(0);
                });
            }
        })
        .run(tauri::generate_context!())
        .expect("desktop launch failed");
}
