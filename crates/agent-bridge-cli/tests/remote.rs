//! Two isolated PCs and a Git server, using protocol doubles only for SSH/models.
//! All bridge admission, workers, verification, review and delivery are production.
use bridge_config::load_config_with_state_root;
use bridge_storage::{
    RustStateLayout,
    automation::{AutomationRunStore, RunStatus},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
struct Fixture {
    root: PathBuf,
    local: RustStateLayout,
    remote: RustStateLayout,
}
fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bridge-remote-{}", uuid::Uuid::new_v4()));
        for dir in ["a/repo", "b", "bin"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        git(&root.join("a/repo"), &["init", "-q"]);
        fs::write(root.join("a/repo/check.py"),"import pathlib,sys\nfor name in sys.argv[1:]: assert pathlib.Path(name).read_text().strip()==pathlib.Path(name).stem\n").unwrap();
        git(&root.join("a/repo"), &["add", "check.py"]);
        git(
            &root.join("a/repo"),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=f@example.invalid",
                "commit",
                "-qm",
                "base",
            ],
        );
        git(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                root.join("a/repo").to_str().unwrap(),
                "repo.git",
            ],
        );
        git(
            &root,
            &[
                "clone",
                "-q",
                root.join("repo.git").to_str().unwrap(),
                "b/repo",
            ],
        );
        let cli = env!("CARGO_BIN_EXE_agent-bridge");
        let cfg = |machine: &str, remote: bool| {
            format!(
                "[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4999\"\npassword_file=\"password\"\nexecution_mode=\"worktree\"\ndelivery_mode=\"manual\"\nmax_rounds=3\n{}",
                json!(root.join(machine).join("repo")),
                if remote {
                    format!(
                        "[projects.proj.remote_execution]\nhost=\"executor.local\"\nuser=\"executor\"\nport=22\nexecutable={}\nconfig={}\nstate_root={}\nproject=\"proj\"\nrepository=\"ssh://fixture/repo.git\"\n",
                        json!(cli),
                        json!(root.join("b/projects.toml")),
                        json!(root.join("b/state"))
                    )
                } else {
                    String::new()
                }
            )
        };
        for machine in ["a", "b"] {
            fs::write(
                root.join(machine).join("projects.toml"),
                cfg(machine, machine == "a"),
            )
            .unwrap();
            fs::write(root.join(machine).join("password"), "fixture-password\n").unwrap();
            fs::set_permissions(
                root.join(machine).join("password"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let script = |name: &str, text: String| {
            let path = root.join("bin").join(name);
            fs::write(&path, text).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        };
        script(
            "ssh",
            r#"#!/usr/bin/python3
import json,sys,subprocess,shlex,os,pathlib
args=shlex.split(sys.argv[-1]); data=sys.stdin.buffer.read() if args[1]=='remote-rpc' else b''
if data:
 req=json.loads(data)
 if req.get('op')=='poll' and os.environ.get('REMOTE_TEST_DROP')=='1':
  dropped=pathlib.Path(os.environ['REMOTE_TEST_ROOT'])/'dropped'
  if not dropped.exists(): dropped.write_text('once'); sys.exit(255)
env=dict(os.environ);env['REMOTE_TEST_MACHINE']='b'
if args[1]=='mcp': os.execvpe(args[0],args,env)
p=subprocess.run(args,input=data,env=env);sys.exit(p.returncode)
"#
            .into(),
        );
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        script(
            "opencode",
            format!(
                "#!/bin/sh\nexec /usr/bin/python3 '{}' --doc '{}' \"$@\"\n",
                base.join("../bridge-automation/tests/fixtures/opencode.py")
                    .display(),
                base.join("../bridge-runtime/tests/fixtures/openapi.json")
                    .display()
            ),
        );
        script("codex",r#"#!/usr/bin/python3
import json,sys,os,pathlib,subprocess
assert os.environ.get('REMOTE_TEST_MACHINE')!='b', 'Codex must execute on PC A'
args=sys.argv[1:]; context=json.load(sys.stdin); root=pathlib.Path(os.environ['REMOTE_TEST_ROOT']); workspace=pathlib.Path(args[args.index('-C')+1])
assert '/a/state/' in str(workspace), 'Review must use an isolated fetched checkout'
step=context['step']['id'];kind=context['operation']
with (root/'codex-trace').open('a') as trace:trace.write(kind+':'+step+'\n')
if kind=='prepare': answer={'task':'PROOF:'+('final' if step=='__final__' else step)}
else:
 if os.environ.get('REMOTE_TEST_HOLD_REVIEW')=='1':
  import time
  time.sleep(90)
 if step!='__final__': assert (workspace/(step+'.txt')).read_text().strip()==step
 revised=root/'revised'
 if step=='left' and not revised.exists():
  revised.write_text('once');answer={'decision':'request_changes','summary':'Exercise revision cycle','findings':['Repeat approved checks']}
 else: answer={'decision':'accept','summary':'Independently inspected fetched files','findings':[]}
json.dump(answer,open(args[args.index('-o')+1],'w'))
"#.into());
        let layout = |machine: &str| {
            let cfg = load_config_with_state_root(
                &root.join(machine).join("projects.toml"),
                &root.join(machine).join("state"),
            )
            .unwrap();
            RustStateLayout::new(
                root.join(machine).join("state"),
                cfg.project("proj").unwrap().id().clone(),
            )
            .unwrap()
        };
        let local = layout("a");
        let remote = layout("b");
        Self {
            root,
            local,
            remote,
        }
    }
    fn cmd(&self, machine: &str, command: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        cmd.args([command, "--project", "proj", "--config"])
            .arg(self.root.join(machine).join("projects.toml"))
            .arg("--state-root")
            .arg(self.root.join(machine).join("state"));
        cmd.env(
            "PATH",
            format!(
                "{}:{}",
                self.root.join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("REMOTE_TEST_ROOT", &self.root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "1")
        .env(
            "GIT_CONFIG_KEY_0",
            format!("url.file://{}/.insteadOf", self.root.display()),
        )
        .env("GIT_CONFIG_VALUE_0", "ssh://fixture/");
        cmd
    }
    fn wait(&self, status: RunStatus) {
        let deadline = Instant::now() + Duration::from_secs(100);
        loop {
            let run = AutomationRunStore::new(self.local.clone())
                .load(None)
                .unwrap();
            if run.status() == status {
                break;
            }
            assert!(Instant::now() < deadline, "{}", run.document());
            if run.status() == RunStatus::Blocked
                && !matches!(status, RunStatus::Blocked | RunStatus::Stopped)
            {
                panic!(
                    "{}\nlocal logs: {}",
                    run.document(),
                    fs::read_to_string(
                        bridge_automation::lifecycle::directory(&self.local, run.id())
                            .join("supervisor.log")
                    )
                    .unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn plan(&self) -> PathBuf {
        let value = json!({"version":1,"goal":"Remote workflow","steps":[{"id":"left","task":"Create left","allowed_paths":["left.txt"],"test_commands":["python3 -B check.py left.txt"],"acceptance_criteria":["left file exists"]},{"id":"right","task":"Create right","allowed_paths":["right.txt"],"test_commands":["python3 -B check.py right.txt"],"acceptance_criteria":["right file exists"]}],"final_test_commands":["python3 -B check.py left.txt right.txt"],"delivery":"apply","codex_timeout":30});
        let path = self.root.join("plan.json");
        fs::write(&path, value.to_string()).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(run) = AutomationRunStore::new(self.local.clone()).load(None) {
            let _ = self
                .cmd("a", "automation-stop")
                .args(["--run", &run.id().to_string()])
                .output();
        }
        if let Ok(storage) = self.remote.open_readonly() {
            let project = load_config_with_state_root(
                &self.root.join("b/projects.toml"),
                self.remote.state_root(),
            )
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
            for task in storage
                .list_tasks(project.id(), false, 100, 0)
                .unwrap_or_default()
            {
                let _ =
                    bridge_runtime::stop_worktree_server(&self.remote, &project, task.task_id, &[]);
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn remote_workflow_reconnects_revises_and_delivers_without_duplicate_tasks() {
    let f = Fixture::new();
    // PC A advances after PC B was cloned. Admission must fetch the pinned source.
    fs::write(f.root.join("a/repo/source.txt"), "updated source\n").unwrap();
    git(&f.root.join("a/repo"), &["add", "source.txt"]);
    git(
        &f.root.join("a/repo"),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=f@example.invalid",
            "commit",
            "-qm",
            "source update",
        ],
    );
    let original = bridge_git::take_snapshot(&f.root.join("a/repo")).unwrap();
    let output = f
        .cmd("a", "launch-codex")
        .env("REMOTE_TEST_DROP", "1")
        .args(["--auto", "--plan"])
        .arg(f.plan())
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    f.wait(RunStatus::Blocked);
    assert!(f.root.join("dropped").exists());
    let id = AutomationRunStore::new(f.local.clone())
        .load(None)
        .unwrap()
        .id();
    let deadline = Instant::now() + Duration::from_secs(10);
    while bridge_automation::lifecycle::supervisor_running(
        &f.local,
        load_config_with_state_root(&f.root.join("a/projects.toml"), f.local.state_root())
            .unwrap()
            .project("proj")
            .unwrap(),
        id,
    )
    .unwrap()
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = f.cmd("a", "automation-status").output().unwrap();
    assert!(status.status.success());
    assert_eq!(
        AutomationRunStore::new(f.remote.clone())
            .load(Some(id))
            .unwrap()
            .control(),
        bridge_storage::automation::RunControl::Run
    );
    let resumed = f
        .cmd("a", "automation-resume")
        .args(["--run", &id.to_string()])
        .output()
        .unwrap();
    assert!(resumed.status.success(), "{:?}", resumed);
    f.wait(RunStatus::Completed);
    assert_eq!(
        fs::read_to_string(f.root.join("a/repo/left.txt")).unwrap(),
        "left\n"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("a/repo/right.txt")).unwrap(),
        "right\n"
    );
    assert!(!f.root.join("b/repo/left.txt").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("b/repo/source.txt")).unwrap(),
        "updated source\n"
    );
    let after = bridge_git::take_snapshot(&f.root.join("a/repo")).unwrap();
    assert_eq!(original.head(), after.head());
    assert_eq!(original.index_fingerprint(), after.index_fingerprint());
    let tasks = f
        .remote
        .open_readonly()
        .unwrap()
        .list_tasks(f.remote.project_id(), false, 100, 0)
        .unwrap();
    assert_eq!(tasks.len(), 3);
    assert!(
        tasks
            .iter()
            .all(|t| t.status == bridge_domain::TaskStatus::Accepted)
    );
    // Interactive Codex obtains a local checkout, then stale acceptance is refused.
    {
        use std::io::{BufRead, BufReader};
        let task = tasks
            .iter()
            .find(|t| t.allowed_paths == vec!["left.txt"])
            .unwrap();
        let checkout = f
            .remote
            .open_readonly()
            .unwrap()
            .get_worktree(task.task_id, f.remote.project_id())
            .unwrap()
            .unwrap()
            .path;
        let mut child = f
            .cmd("a", "mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut call = |id: u64, method: &str, params: Value| {
            writeln!(
                input,
                "{}",
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            )
            .unwrap();
            input.flush().unwrap();
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            serde_json::from_str::<Value>(&line).unwrap()
        };
        assert!(call(1,"initialize",json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}})).get("result").is_some());
        // The protocol transitions to ready only after its initialization notification.

        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}})
        )
        .unwrap();
        input.flush().unwrap();
        let mut call = |id: u64, method: &str, params: Value| {
            writeln!(
                input,
                "{}",
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
            )
            .unwrap();
            input.flush().unwrap();
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            serde_json::from_str::<Value>(&line).unwrap()
        };
        let info = call(
            2,
            "tools/call",
            json!({"name":"project_info","arguments":{}}),
        );
        assert_eq!(
            info["result"]["structuredContent"]["remote_execution"],
            true
        );
        assert_eq!(
            info["result"]["structuredContent"]["workspace"],
            json!(f.root.join("a/repo"))
        );
        let status = call(
            3,
            "tools/call",
            json!({"name":"task_status","arguments":{"task_id":task.task_id.to_string()}}),
        );
        let review = status["result"]["structuredContent"]["review_workspace"]
            .as_str()
            .unwrap();
        assert!(Path::new(review).starts_with(f.local.project_dir()));
        assert_eq!(
            fs::read_to_string(Path::new(review).join("left.txt")).unwrap(),
            "left\n"
        );
        fs::write(Path::new(&checkout).join("left.txt"), "outside change\n").unwrap();
        let rejected = call(
            4,
            "tools/call",
            json!({"name":"accept_task","arguments":{"task_id":task.task_id.to_string()}}),
        );
        assert_eq!(rejected["result"]["isError"], true);
        assert!(rejected.to_string().contains("remote_review_stale"));
        fs::write(Path::new(&checkout).join("left.txt"), "left\n").unwrap();

        drop(input);
        let _ = child.wait();
    }
    let run = AutomationRunStore::new(f.remote.clone())
        .load(Some(id))
        .unwrap();
    assert_eq!(run.status(), RunStatus::Ready);
    assert_eq!(run.document()["steps"][0]["revisions"], 1);
    let refs = git(
        &f.root.join("repo.git"),
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/aibridge/",
        ],
    );
    assert!(refs.contains(&id.to_string()));
}
#[test]
fn executor_rejects_base_mismatch_before_admitting_tasks() {
    let f = Fixture::new();
    let mut cmd = f.cmd("b", "remote-rpc");
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let req = json!({"op":"start","run_id":uuid::Uuid::new_v4().to_string(),"repository":"ssh://fixture/repo.git","base":"0".repeat(40),"plan":serde_json::from_slice::<Value>(&fs::read(f.plan()).unwrap()).unwrap()});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(req.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let response: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        response["error"]
            .as_str()
            .unwrap()
            .starts_with("remote_base_mismatch")
    );
    assert!(!f.remote.database().exists());
}

#[test]
fn remote_pause_and_stop_cleanup_survive_local_source_drift() {
    let f = Fixture::new();
    let out = f
        .cmd("a", "launch-codex")
        .env("REMOTE_TEST_DROP", "1")
        .args(["--auto", "--plan"])
        .arg(f.plan())
        .output()
        .unwrap();
    assert!(out.status.success());
    f.wait(RunStatus::Blocked);
    let run = AutomationRunStore::new(f.local.clone()).load(None).unwrap();
    let project =
        load_config_with_state_root(&f.root.join("a/projects.toml"), f.local.state_root())
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    while bridge_automation::lifecycle::supervisor_running(&f.local, &project, run.id()).unwrap() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let paused = f.cmd("a", "automation-pause").output().unwrap();
    assert!(paused.status.success(), "{:?}", paused);
    assert_eq!(
        AutomationRunStore::new(f.remote.clone())
            .load(Some(run.id()))
            .unwrap()
            .control(),
        bridge_storage::automation::RunControl::Pause
    );
    fs::write(f.root.join("a/repo/user.txt"), "preserve my change\n").unwrap();
    let stopped = f.cmd("a", "automation-stop").output().unwrap();
    assert!(stopped.status.success(), "{:?}", stopped);
    f.wait(RunStatus::Stopped);
    assert_eq!(
        fs::read_to_string(f.root.join("a/repo/user.txt")).unwrap(),
        "preserve my change\n"
    );
    assert_eq!(
        AutomationRunStore::new(f.remote.clone())
            .load(Some(run.id()))
            .unwrap()
            .status(),
        RunStatus::Stopped
    );
    assert_eq!(
        f.remote
            .open_readonly()
            .unwrap()
            .count_tasks(f.remote.project_id(), true)
            .unwrap(),
        0
    );
}

#[test]
fn remote_shutdown_pauses_executor_and_preserves_intermediate_worktrees() {
    let f = Fixture::new();
    let launched = f
        .cmd("a", "launch-codex")
        .env("REMOTE_TEST_HOLD_REVIEW", "1")
        .args(["--auto", "--plan"])
        .arg(f.plan())
        .output()
        .unwrap();
    assert!(launched.status.success());
    let project =
        load_config_with_state_root(&f.root.join("b/projects.toml"), f.remote.state_root())
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
    let deadline = Instant::now() + Duration::from_secs(20);
    let tasks = loop {
        let tasks = f
            .remote
            .open_readonly()
            .ok()
            .and_then(|storage| {
                storage
                    .list_tasks(f.remote.project_id(), false, 100, 0)
                    .ok()
            })
            .unwrap_or_default();
        if !tasks.is_empty()
            && tasks.iter().any(|task| {
                f.remote
                    .open_readonly()
                    .unwrap()
                    .get_worktree(task.task_id, f.remote.project_id())
                    .unwrap()
                    .is_some_and(|tree| tree.status == bridge_storage::WorktreeStatus::Created)
                    && bridge_runtime::worktree_server_state(&f.remote, &project, task.task_id)
                        .is_ok_and(|state| state == bridge_runtime::ServerState::Live)
            })
        {
            break tasks;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    };
    let local_project =
        load_config_with_state_root(&f.root.join("a/projects.toml"), f.local.state_root())
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
    bridge_automation::lifecycle::shutdown(&f.local, &local_project).unwrap();
    let mut command = f.cmd("b", "remote-rpc");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"op\":\"shutdown\"}\n")
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "{:?}", result);
    let reply: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(reply["status"], "stopped", "{reply}");
    let run = AutomationRunStore::new(f.remote.clone())
        .load(None)
        .unwrap();
    assert_eq!(run.status(), RunStatus::Paused);
    let project =
        load_config_with_state_root(&f.root.join("b/projects.toml"), f.remote.state_root())
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
    assert!(
        !bridge_automation::lifecycle::supervisor_running(&f.remote, &project, run.id()).unwrap()
    );
    let storage = f.remote.open_readonly().unwrap();
    for task in tasks {
        if let Some(tree) = storage
            .get_worktree(task.task_id, f.remote.project_id())
            .unwrap()
        {
            assert!(Path::new(&tree.path).exists());
            assert!(matches!(
                bridge_runtime::worktree_server_state(&f.remote, &project, task.task_id).unwrap(),
                bridge_runtime::ServerState::Missing | bridge_runtime::ServerState::Stale
            ));
        }
    }
}

#[test]
fn remote_opencode_stop_rpc_is_project_bound_and_validates_task() {
    let f = Fixture::new();
    for (machine, request, success) in [
        ("b", json!({"op":"stop_opencode","task":null}), true),
        ("b", json!({"op":"stop_opencode","task":42}), false),
        ("b", json!({"op":"stop_opencode","task":"invalid"}), false),
        ("a", json!({"op":"stop_opencode","task":null}), false),
    ] {
        let mut command = f.cmd(machine, "remote-rpc");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(reply.get("error").is_none(), success, "{reply}");
        if success {
            assert_eq!(reply["stopped"], false);
        }
    }
}
