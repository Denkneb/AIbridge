mod launch_env;
use bridge_desktop::{
    dashboard::Query,
    projects::{ProjectDraft, ProjectEndpoints, ProjectService},
    terminal::{Event, Terminals},
};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};
use tauri::{Manager, State};
struct AppState {
    projects: Arc<ProjectService>,
    terminals: Arc<Terminals>,
    shutdown: Arc<AtomicU8>,
}
fn request_shutdown(app: &tauri::AppHandle, code: i32) {
    let state = app.state::<AppState>();
    if state
        .shutdown
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let projects = state.projects.clone();
    let terminals = state.terminals.clone();
    projects.begin_shutdown();
    terminals.begin_shutdown();
    let shutdown = state.shutdown.clone();
    let app = app.clone();
    std::thread::spawn(move || {
        let terminal_failed = if let Err(error) = terminals.shutdown() {
            eprintln!("desktop shutdown: {error}");
            true
        } else {
            false
        };
        let report = projects.shutdown_all();
        let failed = match report {
            Ok(report) if report["status"] == "stopped" => false,
            Ok(report) => {
                eprintln!("desktop shutdown: {report}");
                true
            }
            Err(error) => {
                eprintln!("desktop shutdown: {error}");
                true
            }
        };
        shutdown.store(2, Ordering::Release);
        app.exit(if (failed || terminal_failed) && code == 0 {
            1
        } else {
            code
        });
    });
}
#[tauri::command]
async fn projects(state: State<'_, AppState>) -> Result<Vec<ProjectDraft>, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.projects().map_err(str::to_owned))
        .await
        .map_err(|_| "project query failed".to_owned())?
}
#[tauri::command]
async fn project_suggest_endpoints(state: State<'_, AppState>) -> Result<ProjectEndpoints, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.suggest_endpoints().map_err(str::to_owned))
        .await
        .map_err(|_| "Не удалось подобрать свободные порты".to_owned())?
}
#[tauri::command]
async fn codex_env_read(state: State<'_, AppState>, project: String) -> Result<String, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.read_codex_env(&project))
        .await
        .map_err(|_| "Не удалось загрузить переменные Codex".to_owned())?
}
#[tauri::command]
async fn codex_env_save(
    state: State<'_, AppState>,
    project: String,
    text: String,
) -> Result<(), String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.save_codex_env(&project, &text))
        .await
        .map_err(|_| "Не удалось сохранить переменные Codex".to_owned())?
}
#[tauri::command]
async fn opencode_keys_read(state: State<'_, AppState>, project: String) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.opencode_keys(&project))
        .await
        .map_err(|_| "Не удалось загрузить список ключей".to_owned())?
}
#[tauri::command]
async fn opencode_key_save(
    state: State<'_, AppState>,
    project: String,
    name: String,
    value: Option<String>,
    expected_file: Option<String>,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        p.save_opencode_key(&project, &name, value.as_deref(), expected_file.as_deref())
    })
    .await
    .map_err(|_| "Не удалось сохранить ключ".to_owned())?
}
#[tauri::command]
async fn project_branches(state: State<'_, AppState>, project: String) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.project_branches(&project))
        .await
        .map_err(|_| "Не удалось загрузить ветки проекта".to_owned())?
}
#[tauri::command]
async fn project_branch_switch(
    state: State<'_, AppState>,
    project: String,
    reference: String,
    expected_current: Option<String>,
    expected_head: Option<String>,
    expected_workspace: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        p.switch_project_branch(
            &project,
            &reference,
            expected_current.as_deref(),
            expected_head.as_deref(),
            &expected_workspace,
        )
    })
    .await
    .map_err(|_| "Не удалось переключить ветку проекта".to_owned())?
}
#[tauri::command]
async fn task_set_status(
    state: State<'_, AppState>,
    project: String,
    task: String,
    expected: String,
    target: String,
    reason: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        p.set_task_status(&project, &task, &expected, &target, &reason)
    })
    .await
    .map_err(|_| "Смена статуса завершилась ошибкой".to_owned())?
}
#[tauri::command]
async fn task_recover(
    state: State<'_, AppState>,
    project: String,
    task: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.recover_failed_task(&project, &task))
        .await
        .map_err(|_| "Проверка продолжения сессии завершилась ошибкой".to_owned())?
}
#[tauri::command]
async fn dashboard(state: State<'_, AppState>, query: Query) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.dashboard(query).map_err(str::to_owned))
        .await
        .map_err(|_| "dashboard query failed".to_owned())?
}
#[tauri::command]
async fn task_detail(
    state: State<'_, AppState>,
    project: String,
    task: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        p.task_detail(&project, &task).map_err(str::to_owned)
    })
    .await
    .map_err(|_| "Не удалось загрузить карточку задачи".to_owned())?
}
#[tauri::command]
async fn task_rounds(
    state: State<'_, AppState>,
    project: String,
    task: String,
    before: u32,
    expected_revision: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        p.task_rounds(&project, &task, before, &expected_revision)
            .map_err(str::to_owned)
    })
    .await
    .map_err(|_| "Не удалось загрузить историю раундов".to_owned())?
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
async fn project_remove_preview(
    state: State<'_, AppState>,
    project: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.preview_remove(&project))
        .await
        .map_err(|_| "Не удалось подготовить удаление проекта".to_owned())?
}
#[tauri::command]
async fn project_apply(state: State<'_, AppState>, review_id: String) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.apply(&review_id))
        .await
        .map_err(|_| "save failed".to_owned())?
}
#[tauri::command]
fn project_cancel(state: State<'_, AppState>, review_id: String) {
    state.projects.cancel(&review_id);
}
#[tauri::command]
async fn opencode_config_read(
    state: State<'_, AppState>,
    project: String,
    file: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        p.read_opencode_config(&project, &file)
            .map_err(str::to_owned)
    })
    .await
    .map_err(|_| "OpenCode config read failed".to_owned())?
}
#[tauri::command]
async fn opencode_config_preview(
    state: State<'_, AppState>,
    project: String,
    file: String,
    content: String,
    original_content: Option<String>,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || {
        p.preview_opencode_config(&project, &file, &content, original_content.as_deref())
            .map_err(str::to_owned)
    })
    .await
    .map_err(|_| "OpenCode config preview failed".to_owned())?
}
#[tauri::command]
async fn opencode_config_apply(
    state: State<'_, AppState>,
    review_id: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.apply_opencode_config(&review_id))
        .await
        .map_err(|_| "OpenCode config save failed".to_owned())?
}
#[tauri::command]
fn opencode_config_cancel(state: State<'_, AppState>, review_id: String) {
    state.projects.cancel_opencode_config(&review_id);
}
#[tauri::command]
async fn automation_preview(
    state: State<'_, AppState>,
    project: String,
    content: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.automation_preview(&project, &content))
        .await
        .map_err(|_| "Не удалось проверить план".to_owned())?
}
#[tauri::command]
fn automation_cancel(state: State<'_, AppState>, review_id: String) {
    state.projects.automation_cancel(&review_id);
}
#[tauri::command]
async fn automation_start(
    state: State<'_, AppState>,
    project: String,
    review_id: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.automation_start(&project, &review_id))
        .await
        .map_err(|_| "Не удалось запустить план".to_owned())?
}
#[tauri::command]
async fn automation_status(state: State<'_, AppState>, project: String) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.automation_status(&project))
        .await
        .map_err(|_| "Не удалось загрузить запуск".to_owned())?
}
#[tauri::command]
async fn automation_control(
    state: State<'_, AppState>,
    project: String,
    run: String,
    action: String,
) -> Result<Value, String> {
    let p = state.projects.clone();
    tauri::async_runtime::spawn_blocking(move || p.automation_control(&project, &run, &action))
        .await
        .map_err(|_| "Не удалось изменить состояние запуска".to_owned())?
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
    launch_env: Option<String>,
) -> Result<portable_pty::CommandBuilder, String> {
    let (entry, _) = p.project(project).map_err(str::to_owned)?;
    let executable = std::env::var_os("AIBRIDGE_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/agent-bridge")
        });
    let variables = launch_env::parse(launch_env.as_deref().unwrap_or(""))?;
    if !variables.is_empty() && !matches!(profile, "codex" | "codex-standalone") {
        return Err("Переменные запуска доступны для Codex".into());
    }
    let mut cmd = match profile {
        "shell" => {
            if task.is_some() {
                return Err("shell task binding is unsupported".into());
            }
            let mut c = portable_pty::CommandBuilder::new("/bin/bash");
            c.args(["--noprofile", "--norc"]);
            c
        }
        "codex-standalone" => {
            if task.is_some() {
                return Err("Обычный Codex не привязан к задаче моста".into());
            }
            portable_pty::CommandBuilder::new("codex")
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
    for (name, value) in variables {
        cmd.env(name, value);
    }
    Ok(cmd)
}
#[tauri::command]
async fn terminal_external(
    state: State<'_, AppState>,
    project: String,
    profile: String,
    task: Option<String>,
    launch_env: Option<String>,
) -> Result<(), String> {
    let p = state.projects.clone();
    let t = state.terminals.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cmd = terminal_command(&p, &project, &profile, task, launch_env.clone())?;
        let emulator = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .find_map(|dir| std::fs::canonicalize(dir.join("x-terminal-emulator")).ok())
            .unwrap_or_else(|| PathBuf::from("x-terminal-emulator"));
        let mut external = std::process::Command::new(&emulator);
        // Konsole otherwise hands the window to an existing process over D-Bus.
        if emulator.file_name().is_some_and(|name| name == "konsole") {
            external.arg("--separate");
        }
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
        for (name, value) in launch_env::parse(launch_env.as_deref().unwrap_or(""))? {
            external.env(name, value);
        }
        t.open_external(external).map_err(str::to_owned)
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
    launch_env: Option<String>,
) -> Result<String, String> {
    let p = state.projects.clone();
    let t = state.terminals.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cmd = terminal_command(&p, &project, &profile, task, launch_env.clone())?;
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
async fn clipboard_read(app: tauri::AppHandle) -> Result<String, String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        if gtk::gdk::Display::default()
            .and_then(|d| d.default_seat())
            .is_none()
        {
            let _ = tx.send(Err("clipboard input seat unavailable".to_owned()));
            return;
        }
        gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).request_text(move |_, text| {
            let result = match text {
                Some(text) if text.len() <= 1024 * 1024 => Ok(text.to_owned()),
                None => Ok(String::new()),
                _ => Err("clipboard text too large".to_owned()),
            };
            let _ = tx.send(result);
        });
    })
    .map_err(|_| "clipboard unavailable")?;
    tauri::async_runtime::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| "clipboard read timed out".to_owned())
    })
    .await
    .map_err(|_| "clipboard read failed".to_owned())??
}
#[tauri::command]
async fn clipboard_write(app: tauri::AppHandle, text: String) -> Result<(), String> {
    if text.len() > 1024 * 1024 {
        return Err("clipboard text too large".into());
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        if gtk::gdk::Display::default()
            .and_then(|d| d.default_seat())
            .is_none()
        {
            let _ = tx.send(Err("clipboard input seat unavailable".to_owned()));
            return;
        }
        gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(&text);
        let _ = tx.send(Ok(()));
    })
    .map_err(|_| "clipboard unavailable")?;
    tauri::async_runtime::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| "clipboard write timed out".to_owned())
    })
    .await
    .map_err(|_| "clipboard write failed".to_owned())??
}
#[tauri::command]
fn smoke_options() -> Result<Value, String> {
    if !cfg!(feature = "desktop-smoke") {
        return Err("smoke build required".into());
    }
    Ok(serde_json::json!({"live_tui":std::env::var_os("AIBRIDGE_DESKTOP_LIVE_TUI").is_some()}))
}
#[tauri::command]
async fn smoke_native_keyboard() -> Result<bool, String> {
    if !cfg!(feature = "desktop-smoke") {
        return Err("smoke build required".into());
    }
    let Some(script) = std::env::var_os("AIBRIDGE_DESKTOP_NATIVE_KEYS") else {
        return Ok(false);
    };
    tauri::async_runtime::spawn_blocking(move || {
        std::process::Command::new("python3")
            .arg(script)
            .status()
            .map_err(|_| "native keyboard unavailable".to_owned())?
            .success()
            .then_some(true)
            .ok_or_else(|| "native keyboard failed".to_owned())
    })
    .await
    .map_err(|_| "native keyboard failed".to_owned())?
}
#[tauri::command]
async fn smoke_complete(app: tauri::AppHandle, passed: bool, checks: Value) -> Result<(), String> {
    if !cfg!(feature = "desktop-smoke") {
        return Err("smoke build required".into());
    }
    let path = std::env::var_os("AIBRIDGE_DESKTOP_SMOKE_RESULT").ok_or("smoke output required")?;
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({"passed":passed,"checks":checks}))
            .map_err(|_| "smoke serialization failed")?,
    )
    .map_err(|_| "smoke output failed")?;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(3));
        if passed {
            if let Some(window) = app.get_webview_window("main") {
                if window.close().is_err() { app.exit(1); }
            } else { app.exit(1); }
        } else { app.exit(1); }
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
            shutdown: Arc::new(AtomicU8::new(0)),
        })
        .invoke_handler(tauri::generate_handler![
            projects,
            project_suggest_endpoints,
            project_branches,
            project_branch_switch,
            codex_env_read,
            codex_env_save,
            opencode_keys_read,
            opencode_key_save,
            task_set_status,
            task_recover,
            dashboard,
            task_detail,
            task_rounds,
            dashboard_revision,
            project_preview,
            project_remove_preview,
            project_apply,
            project_cancel,
            opencode_config_read,
            opencode_config_preview,
            opencode_config_apply,
            opencode_config_cancel,
            automation_preview,
            automation_cancel,
            automation_start,
            automation_status,
            automation_control,
            lifecycle,
            terminal_open,
            terminal_external,
            terminal_read,
            terminal_write,
            terminal_resize,
            terminal_close,
            clipboard_read,
            clipboard_write,
            smoke_options,
            smoke_native_keyboard,
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
                request_shutdown(window.app_handle(), 0);
            }
        })
        .build(tauri::generate_context!())
        .expect("desktop launch failed")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event
                && app.state::<AppState>().shutdown.load(Ordering::Acquire) != 2 {
                api.prevent_exit();
                request_shutdown(app, code.unwrap_or(0));
            }
        });
}
