//! Authenticated through SSH, bound to a configured remote project. JSON RPC is
//! deliberately operation-specific rather than an arbitrary shell-command API.
use super::LaunchArgs;
use bridge_automation::{
    codex::{CodexClient, Operation, read_bounded, validate_answer},
    lifecycle,
    remote::{fetch_review, rpc},
    run::{check_binding, create_bound_run},
};
use bridge_config::{ProjectEntry, remote::RemoteExecution};
use bridge_storage::{
    RustStateLayout,
    automation::{AutomationRunStore, AutomationStoreError, RunControl, RunId, RunStatus},
};
use serde_json::{Value, json};
use std::{io::Read, process::ExitCode, time::Duration};
pub fn serve(args: LaunchArgs) -> Result<ExitCode, String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(1_000_001)
        .read_to_end(&mut bytes)
        .map_err(|_| "remote_input_invalid")?;
    if bytes.len() > 1_000_000 {
        return Err("remote_input_limit".into());
    }
    let request: Value = serde_json::from_slice(&bytes).map_err(|_| "remote_json_invalid")?;
    let result = dispatch(args, request);
    println!(
        "{}",
        match result {
            Ok(value) => value,
            Err(error) => json!({"error":error}),
        }
    );
    Ok(ExitCode::SUCCESS)
}
fn dispatch(args: LaunchArgs, req: Value) -> Result<Value, String> {
    let service = bridge_desktop::projects::ProjectService::new(
        args.config.clone(),
        args.state_root.clone(),
    )?;
    let (project, layout) = service.project(&args.project)?;
    if project.remote_execution().is_some() {
        return Err("remote_executor_must_use_local_mode".into());
    }
    let op = req["op"].as_str().ok_or("remote_operation_missing")?;
    match op {
        "stop_opencode" => {
            let task = match &req["task"] {
                Value::Null => None,
                Value::String(task) => Some(task.as_str()),
                _ => return Err("remote_task_invalid".into()),
            };
            return service
                .stop_opencode(&args.project, task)
                .map_err(Into::into);
        }
        "shutdown" => return service.shutdown_project(&args.project),
        "lifecycle" => {
            return service
                .lifecycle(
                    &args.project,
                    req["command"].as_str().ok_or("remote_command_missing")?,
                )
                .map_err(Into::into);
        }
        "dashboard" => {
            let mut query = req["query"].clone();
            query["project"] = json!(args.project);
            return service
                .dashboard(serde_json::from_value(query).map_err(|_| "remote_query_invalid")?)
                .map_err(Into::into);
        }
        "revision" => {
            return service
                .dashboard_revision(&args.project, false)
                .map(|v| json!(v))
                .map_err(Into::into);
        }
        "detail" => {
            return service
                .task_detail(
                    &args.project,
                    req["task"].as_str().ok_or("remote_task_missing")?,
                )
                .map_err(Into::into);
        }
        "rounds" => {
            return service
                .task_rounds(
                    &args.project,
                    req["task"].as_str().ok_or("remote_task_missing")?,
                    u32::try_from(req["before"].as_u64().ok_or("remote_cursor_invalid")?)
                        .map_err(|_| "remote_cursor_invalid")?,
                    req["revision"].as_str().ok_or("remote_revision_missing")?,
                )
                .map_err(Into::into);
        }
        "set_status" => {
            return service.set_task_status(
                &args.project,
                req["task"].as_str().ok_or("remote_task_missing")?,
                req["expected"].as_str().ok_or("remote_status_missing")?,
                req["target"].as_str().ok_or("remote_status_missing")?,
                req["reason"].as_str().ok_or("remote_reason_missing")?,
            );
        }
        "recover" => {
            return service.recover_failed_task(
                &args.project,
                req["task"].as_str().ok_or("remote_task_missing")?,
            );
        }
        "snapshot" => return task_snapshot(&project, &layout, &req),
        _ => {}
    }
    let id: RunId = req["run_id"]
        .as_str()
        .ok_or("remote_run_missing")?
        .parse()
        .map_err(|_| "remote_run_invalid")?;
    let store = AutomationRunStore::new(layout.clone());
    match op {
        "start" => {
            let repository = req["repository"]
                .as_str()
                .ok_or("remote_repository_missing")?;
            // Reuse the same Git URL validation, independently on the executor.
            let settings = RemoteExecution {
                host: "localhost".into(),
                user: "executor".into(),
                port: 22,
                executable: "/bin/bridge".into(),
                config: "/config".into(),
                state_root: "/state".into(),
                project: args.project.clone(),
                repository: repository.into(),
            };
            settings
                .validate()
                .map_err(|_| "remote_repository_invalid")?;
            let base = req["base"].as_str().ok_or("remote_base_missing")?;
            let metadata = json!({"repository":repository,"base":base,"client_plan":req["plan"],"source":req["source"]});
            let mut plan = req["plan"].clone();
            plan["delivery"] = json!("manual");
            match store.load(Some(id)) {
                Ok(run) if run.document()["remote_executor"] == metadata => {}
                Ok(_) => return Err("remote_run_binding_changed".into()),
                Err(AutomationStoreError::NotFound) => {
                    let head = bridge_git::take_snapshot(project.workspace())
                        .map_err(|_| "remote_repository_invalid")?;
                    if head.head().map(|h| h.as_str()) != Some(base) && req["source"].is_null() {
                        return Err("remote_base_mismatch: fetch the source repository and check out the same clean commit on both PCs".into());
                    }
                    create_bound_run(&project, &layout, &plan, id, metadata)
                        .map_err(|e| e.to_string())?;
                }
                Err(e) => return Err(e.to_string()),
            }
            let run = store.load(Some(id)).map_err(|e| e.to_string())?;
            if !run.status().is_terminal()
                && !lifecycle::supervisor_running(&layout, &project, id)
                    .map_err(|e| e.to_string())?
            {
                lifecycle::launch(
                    &layout,
                    &project,
                    id,
                    &std::env::current_exe().map_err(|_| "remote_executable_unavailable")?,
                    run.status() != RunStatus::Running,
                )
                .map_err(|e| e.to_string())?;
            }
        }
        "control" => {
            let control: RunControl = req["control"]
                .as_str()
                .ok_or("remote_control_missing")?
                .parse()
                .map_err(|_| "remote_control_invalid")?;
            let run = store.load(Some(id)).map_err(|e| e.to_string())?;
            if !run.status().is_terminal() {
                store.set_control(id, control).map_err(|e| e.to_string())?;
                if !lifecycle::supervisor_running(&layout, &project, id)
                    .map_err(|e| e.to_string())?
                {
                    let mode = match control {
                        RunControl::Run => Some(lifecycle::LaunchMode::Resume),
                        RunControl::Stop => Some(lifecycle::LaunchMode::Stop),
                        RunControl::Pause => None,
                    };
                    if let Some(mode) = mode {
                        lifecycle::launch_command_mode(
                            &layout,
                            &project,
                            id,
                            &std::env::current_exe()
                                .map_err(|_| "remote_executable_unavailable")?,
                            vec![],
                            mode,
                        )
                        .map_err(|e| e.to_string())?;
                    }
                }
            }
        }
        "answer" => {
            let dir = lifecycle::directory(&layout, id).join("remote");
            let request: Value = serde_json::from_slice(
                &read_bounded(&dir.join("request.json"), 1_000_000)
                    .map_err(|_| "remote_review_expired")?,
            )
            .map_err(|_| "remote_review_invalid")?;
            let nonce = req["nonce"].as_str().ok_or("remote_nonce_missing")?;
            if request["nonce"] != nonce {
                return Err("remote_review_expired".into());
            }
            let operation = operation(&request)?;
            let answer = validate_answer(operation, req["answer"].clone())
                .map_err(|_| "remote_answer_invalid")?;
            let path = dir.join(format!("answer-{nonce}.json"));
            if path.exists() {
                let previous: Value = serde_json::from_slice(
                    &read_bounded(&path, 1_000_000).map_err(|_| "remote_answer_invalid")?,
                )
                .map_err(|_| "remote_answer_invalid")?;
                if previous != answer {
                    return Err("remote_answer_conflict".into());
                }
            } else {
                bridge_artifact::artifact::atomic_write(
                    &path,
                    answer.to_string().as_bytes(),
                    0o600,
                )
                .map_err(|_| "remote_answer_unwritable")?;
            }
        }
        "poll" => {}
        _ => return Err("remote_operation_unsupported".into()),
    }
    let run = store.load(Some(id)).map_err(|e| e.to_string())?;
    if run.document()["remote_executor"].is_null() {
        return Err("remote_run_binding_missing".into());
    }
    let dir = lifecycle::directory(&layout, id).join("remote");
    let read = |name| {
        read_bounded(&dir.join(name), 1_000_000)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or(Value::Null)
    };
    Ok(
        json!({"run_id":id.to_string(),"document":run.document(),"status":run.status().as_str(),"control":run.control().as_str(),"supervisor_running":lifecycle::supervisor_running(&layout,&project,id).map_err(|e|e.to_string())?,"request":read("request.json"),"result":read("accepted.json")}),
    )
}
fn operation(request: &Value) -> Result<Operation, String> {
    match request["operation"].as_str() {
        Some("prepare") => Ok(Operation::Prepare),
        Some("review") => Ok(Operation::Review),
        _ => Err("remote_operation_invalid".into()),
    }
}
pub fn controller(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    id: RunId,
    settings: &RemoteExecution,
    client: &mut CodexClient,
) -> Result<Value, String> {
    let store = AutomationRunStore::new(layout.clone());
    let local = store.load(Some(id)).map_err(|e| e.to_string())?;
    if local.document()["phase"] == "deliver"
        && !local.document()["remote_result"].is_null()
        && local.control() != RunControl::Run
    {
        let status = if local.control() == RunControl::Stop {
            RunStatus::Stopped
        } else {
            RunStatus::Paused
        };
        store
            .save(local.document(), status)
            .map_err(|e| e.to_string())?;
        return Ok(json!({"status":status.as_str()}));
    }
    if local.document()["phase"] == "deliver"
        && !local.document()["remote_result"].is_null()
        && local.document()["plan"]["delivery"] == "apply"
    {
        bridge_automation::remote::apply_result(project, layout, id, settings)?;
        let doc = local.document().clone();
        store
            .save(&doc, RunStatus::Completed)
            .map_err(|e| e.to_string())?;
        return Ok(json!({"status":"completed"}));
    }
    let base = local.document()["origin"]["snapshot"]["head"]
        .as_str()
        .ok_or("remote_base_missing")?;
    let plan = local.document()["plan"].clone();
    let mut response = if local.control() != RunControl::Run {
        match rpc(
            settings,
            &json!({"op":"control","run_id":id.to_string(),"control":local.control().as_str()}),
        ) {
            Ok(response) => response,
            Err(error) if error == "automation run not found" => {
                let status = if local.control() == RunControl::Stop {
                    RunStatus::Stopped
                } else {
                    RunStatus::Paused
                };
                store
                    .save(local.document(), status)
                    .map_err(|e| e.to_string())?;
                return Ok(json!({"status":status.as_str()}));
            }
            Err(error) => return Err(error),
        }
    } else {
        check_binding(project, layout, &local).map_err(|e| e.to_string())?;
        let source = bridge_automation::remote::publish(
            project.workspace(),
            &lifecycle::directory(layout, id).join("source"),
            &settings.repository,
            id,
        )?;
        rpc(
            settings,
            &json!({"op":"start","run_id":id.to_string(),"base":base,"plan":plan,"repository":settings.repository,"source":source}),
        )?
    };
    let mut handled = std::collections::HashSet::new();
    loop {
        let local = store.load(Some(id)).map_err(|e| e.to_string())?;
        // Copy progress only: local immutable plan, binding and origin stay bound to PC A.
        if response["run_id"] != id.to_string() {
            return Err("remote_run_mismatch".into());
        }
        let mut expected_plan = plan.clone();
        expected_plan["delivery"] = json!("manual");
        if response["document"]["plan"] != expected_plan
            || response["document"]["remote_executor"]["client_plan"] != plan
            || response["document"]["remote_executor"]["repository"] != settings.repository
            || response["document"]["remote_executor"]["base"] != base
            || response["document"]["steps"].as_array().map(Vec::len)
                != local.document()["steps"].as_array().map(Vec::len)
        {
            return Err("remote_approved_plan_changed".into());
        }
        for (remote, expected) in response["document"]["steps"]
            .as_array()
            .ok_or("remote_steps_invalid")?
            .iter()
            .zip(
                local.document()["steps"]
                    .as_array()
                    .ok_or("remote_steps_invalid")?,
            )
        {
            if remote["step"] != expected["step"] {
                return Err("remote_approved_step_changed".into());
            }
        }
        let mut doc = local.document().clone();
        for field in ["steps", "phase", "index", "elapsed", "blocker"] {
            doc[field] = response["document"][field].clone();
        }
        doc["remote_status"] = response["status"].clone();
        doc["remote_result"] = response["result"].clone();
        let status: RunStatus = response["status"]
            .as_str()
            .ok_or("remote_status_invalid")?
            .parse()
            .map_err(|_| "remote_status_invalid")?;
        if status == RunStatus::Ready {
            let final_item = doc["steps"]
                .as_array()
                .and_then(|s| s.last())
                .ok_or("remote_final_missing")?;
            if doc["remote_result"]["task_id"] != final_item["task_id"]
                || doc["remote_result"]["round"] != final_item["review"]["round"]
                || doc["remote_result"]["fingerprint"] != final_item["review"]["fingerprint"]
                || doc["remote_result"]["base"] != base
            {
                return Err("remote_final_review_stale".into());
            }
        }
        if status == RunStatus::Ready && local.control() != RunControl::Run {
            doc["phase"] = json!("deliver");
            let status = if local.control() == RunControl::Stop {
                RunStatus::Stopped
            } else {
                RunStatus::Paused
            };
            store.save(&doc, status).map_err(|e| e.to_string())?;
            return Ok(json!({"status":status.as_str()}));
        }
        if status == RunStatus::Ready && plan["delivery"] == "apply" {
            doc["phase"] = json!("deliver");
            store
                .save(&doc, RunStatus::Running)
                .map_err(|e| e.to_string())?;
            bridge_automation::remote::apply_result(project, layout, id, settings)?;
            store
                .save(&doc, RunStatus::Completed)
                .map_err(|e| e.to_string())?;
            return Ok(json!({"status":"completed"}));
        }
        store.save(&doc, status).map_err(|e| e.to_string())?;
        if status.is_terminal() {
            return Ok(response);
        }
        let remote_control: RunControl = response["control"]
            .as_str()
            .ok_or("remote_control_invalid")?
            .parse()
            .map_err(|_| "remote_control_invalid")?;
        if local.control() != remote_control {
            response = rpc(
                settings,
                &json!({"op":"control","run_id":id.to_string(),"control":local.control().as_str()}),
            )?;
            continue;
        }
        if matches!(status, RunStatus::Paused | RunStatus::Blocked) {
            return Ok(response);
        }
        let request = response["request"].clone();
        if let Some(nonce) = request["nonce"].as_str().filter(|n| !handled.contains(*n)) {
            let kind = operation(&request)?;
            let index = doc["index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .ok_or("remote_index_invalid")?;
            if request["run_id"] != id.to_string()
                || request["context"]["approved_plan"] != expected_plan
                || request["context"]["step"] != doc["steps"][index]["step"]
                || request["timeout"]
                    .as_u64()
                    .is_none_or(|t| t == 0 || t > plan["codex_timeout"].as_u64().unwrap_or(0))
            {
                return Err("remote_review_binding_changed".into());
            }
            let workspace = fetch_review(
                &lifecycle::directory(layout, id).join("reviews"),
                &settings.repository,
                &request["snapshot"],
            )?;
            client
                .set_timeout(Duration::from_secs(
                    request["timeout"]
                        .as_u64()
                        .ok_or("remote_timeout_invalid")?,
                ))
                .map_err(|e| e.to_string())?;
            let before = bridge_git::take_snapshot(&workspace)
                .map_err(|_| "remote_review_checkout_invalid")?;
            let answer = client
                .call(kind, &workspace, &request["context"], || {
                    store
                        .load(Some(id))
                        .is_ok_and(|r| r.control() != RunControl::Run)
                })
                .map_err(|e| e.to_string())?;
            if before
                != bridge_git::take_snapshot(&workspace)
                    .map_err(|_| "remote_review_checkout_invalid")?
            {
                return Err("remote_review_changed_checkout".into());
            }
            response = rpc(
                settings,
                &json!({"op":"answer","run_id":id.to_string(),"nonce":nonce,"answer":answer}),
            )?;
            handled.insert(nonce.to_owned());
        } else {
            std::thread::sleep(Duration::from_secs(1));
            response = rpc(settings, &json!({"op":"poll","run_id":id.to_string()}))?;
        }
    }
}
fn task_snapshot(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    req: &Value,
) -> Result<Value, String> {
    let id: bridge_domain::TaskId = req["task"]
        .as_str()
        .ok_or("remote_task_missing")?
        .parse()
        .map_err(|_| "remote_task_invalid")?;
    let storage = layout
        .open_readonly()
        .map_err(|_| "remote_state_unavailable")?;
    let task = storage
        .get_task(id)
        .map_err(|_| "remote_task_invalid")?
        .ok_or("remote_task_missing")?;
    if task.project_id != *project.id() {
        return Err("remote_task_binding_changed".into());
    }
    let step = json!({"allowed_paths":task.allowed_paths,"test_commands":task.test_commands});
    let evidence = bridge_worker::acceptance::acceptance_checks(layout, project, &task, &step)
        .map_err(str::to_owned)?;
    let run: RunId = id.to_string().parse().map_err(|_| "remote_task_invalid")?;
    let repository = req["repository"]
        .as_str()
        .ok_or("remote_repository_missing")?;
    // task snapshots use the same validated SSH binding URL syntax.
    let snapshot = bridge_automation::remote::publish(
        &evidence.root,
        &layout.project_dir().join("remote-snapshots"),
        repository,
        run,
    )?;
    Ok(json!({"snapshot":snapshot,"round":evidence.round,"fingerprint":evidence.fingerprint}))
}

/// Local Codex talks to the ordinary remote stdio MCP. Awaiting-review replies
/// are enriched with a fetched checkout, and acceptance is fenced by its receipt.
pub fn mcp_proxy(
    project: &ProjectEntry,
    layout: &RustStateLayout,
    settings: &RemoteExecution,
) -> Result<ExitCode, String> {
    use std::io::{BufRead, Write};
    use std::process::Stdio;
    let mut command = bridge_automation::remote::ssh(settings, "mcp")?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| "remote_mcp_unavailable")?;
    let result = (|| {
        let mut remote_input = child.stdin.take().ok_or("remote_mcp_unavailable")?;
        let mut remote_output =
            std::io::BufReader::new(child.stdout.take().ok_or("remote_mcp_unavailable")?);
        let mut input = std::io::stdin().lock();
        let mut output = std::io::stdout().lock();
        let mut receipts = std::collections::HashMap::<String, Value>::new();
        loop {
            let mut frame = Vec::new();
            if Read::take(&mut input, 1_000_001)
                .read_until(b'\n', &mut frame)
                .map_err(|_| "remote_mcp_io")?
                == 0
            {
                break;
            }
            if frame.len() > 1_000_000 {
                return Err("remote_mcp_limit".into());
            }
            let message: Value = serde_json::from_slice(&frame).map_err(|_| "remote_mcp_json")?;
            if message["params"]["name"] == "accept_task" {
                let task = message["params"]["arguments"]["task_id"]
                    .as_str()
                    .ok_or("remote_task_missing")?;
                let checked = rpc(
                    settings,
                    &json!({"op":"snapshot","task":task,"repository":settings.repository}),
                );
                if checked
                    .as_ref()
                    .ok()
                    .is_none_or(|snapshot| receipts.get(task).is_none_or(|old| old != snapshot))
                {
                    writeln!(output,"{}",json!({"jsonrpc":"2.0","id":message["id"],"result":{"isError":true,"content":[{"type":"text","text":"remote_review_stale: obtain task_status and independently review review_workspace before accepting"}]}})).map_err(|_|"remote_mcp_io")?;
                    output.flush().map_err(|_| "remote_mcp_io")?;
                    continue;
                }
            }
            remote_input
                .write_all(&frame)
                .and_then(|()| remote_input.flush())
                .map_err(|_| "remote_mcp_io")?;
            if message.get("id").is_none() {
                continue;
            }
            let mut reply = Vec::new();
            if Read::take(&mut remote_output, 8_000_001)
                .read_until(b'\n', &mut reply)
                .map_err(|_| "remote_mcp_io")?
                == 0
                || reply.len() > 8_000_000
            {
                return Err("remote_mcp_connection_lost".into());
            }
            let mut response: Value =
                serde_json::from_slice(&reply).map_err(|_| "remote_mcp_json")?;
            if message["params"]["name"] == "project_info"
                && let Some(info) = response["result"]["structuredContent"].as_object_mut()
            {
                info.insert(
                    "remote_workspace".into(),
                    info.get("workspace").cloned().unwrap_or(Value::Null),
                );
                info.insert("workspace".into(), json!(project.workspace()));
                info.insert("remote_execution".into(), json!(true));
            }
            if message["params"]["name"] == "task_status" {
                let detail = &mut response["result"]["structuredContent"];
                if matches!(
                    detail["status"].as_str(),
                    Some("awaiting_review" | "accepted")
                ) && let Some(task) = detail["task_id"].as_str().map(str::to_owned)
                {
                    match rpc(
                        settings,
                        &json!({"op":"snapshot","task":task,"repository":settings.repository}),
                    )
                    .and_then(|receipt| {
                        fetch_review(
                            &layout.project_dir().join("remote-reviews"),
                            &settings.repository,
                            &receipt["snapshot"],
                        )
                        .map(|path| (receipt, path))
                    }) {
                        Ok((receipt, path)) => {
                            detail["review_workspace"] = json!(path);
                            detail["git_snapshot"] = receipt["snapshot"].clone();
                            receipts.insert(task, receipt);
                        }
                        Err(error) => {
                            detail["review_blocker"] = json!(error);
                            receipts.remove(&task);
                        }
                    }
                }
            }
            if response["result"]["structuredContent"].is_object() {
                response["result"]["content"] = json!([{"type":"text","text":response["result"]["structuredContent"].to_string()}]);
            }
            writeln!(output, "{response}")
                .and_then(|()| output.flush())
                .map_err(|_| "remote_mcp_io")?;
        }
        Ok(ExitCode::SUCCESS)
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}
