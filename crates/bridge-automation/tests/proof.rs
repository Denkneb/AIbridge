use bridge_automation::{
    codex::{CodexError, Operation},
    coordinator::{Coordinator, ReviewClient, Tick},
    run::create_run,
};
use bridge_config::{ProjectEntry, load_config_with_state_root};
use bridge_runtime::{RuntimeOptions, ServerCommand};
use bridge_storage::{
    RustStateLayout,
    automation::{AutomationRunStore, RunStatus},
};
use bridge_worker::runner::{WorkerSettings, run_worker};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
// Both tests allocate from the same OS port range using independent Rust roots.
// Serialize fixture roots; the parallel proof still runs its two workers together.
static PROOF_LOCK: Mutex<()> = Mutex::new(());
struct Fixture {
    root: PathBuf,
    project: ProjectEntry,
    layout: RustStateLayout,
    workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
}
impl Fixture {
    fn new(parallel: bool) -> Self {
        let root = std::env::temp_dir().join(format!("bridge-proof-{}", uuid::Uuid::new_v4()));
        let main = root.join("main");
        fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        fs::write(main.join("module.py"), "print(1)\n").unwrap();
        git(&main, &["add", "module.py"]);
        git(
            &main,
            &[
                "-c",
                "user.name=Proof",
                "-c",
                "user.email=p@example.invalid",
                "commit",
                "-qm",
                "initial",
            ],
        );
        fs::write(root.join("password"), "proof\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("password"), fs::Permissions::from_mode(0o600)).unwrap();
        let config = root.join("projects.toml");
        fs::write(&config,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4999\"\npassword_file={}\nmax_rounds=3\nexecution_mode=\"worktree\"\nallow_parallel_writers={parallel}\nmax_active_tasks={}\n",json!(main),json!(root.join("password")),if parallel{2}else{1})).unwrap();
        let project = load_config_with_state_root(&config, &root.join("state"))
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
        let layout = RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
        layout.initialize().unwrap();
        Self {
            root,
            project,
            layout,
            workers: Arc::new(Mutex::new(vec![])),
        }
    }
    fn spawner(&self, rendezvous: Option<PathBuf>) -> bridge_mcp::WorkerSpawner {
        let layout = self.layout.clone();
        let project = self.project.clone();
        let workers = self.workers.clone();
        Arc::new(move |round| {
            let round = round.clone();
            let layout = layout.clone();
            let project = project.clone();
            let rendezvous = rendezvous.clone();
            let handle = thread::spawn(move || {
                let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                let mut args = vec![
                    base.join("tests/fixtures/opencode.py").into_os_string(),
                    "--doc".into(),
                    base.join("../bridge-runtime/tests/fixtures/openapi.json")
                        .into_os_string(),
                ];
                if let Some(gate) = rendezvous {
                    args.extend(["--rendezvous".into(), gate.into_os_string()]);
                }
                let command =
                    ServerCommand::executable(Path::new("/usr/bin/python3"), args).unwrap();
                let settings = WorkerSettings {
                    deadline: Duration::from_secs(20),
                    http_timeout: Duration::from_secs(3),
                    observation: bridge_worker::observation_loop::ObservationSettings {
                        poll_interval: Duration::from_millis(10),
                        verification_timeout: Duration::from_secs(5),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                run_worker(
                    &layout,
                    &project,
                    round,
                    &[&layout],
                    &[&project],
                    &command,
                    RuntimeOptions {
                        lock_wait: Duration::from_secs(10),
                        ready_timeout: Duration::from_secs(3),
                        request_timeout: Duration::from_millis(300),
                    },
                    settings,
                )
                .unwrap();
            });
            workers.lock().unwrap().push(handle);
            Ok(())
        })
    }
    fn join(&self) {
        let workers = self.workers.lock().unwrap().drain(..).collect::<Vec<_>>();
        for h in workers {
            h.join().unwrap();
        }
    }
    fn trace(&self, id: bridge_domain::TaskId) -> Vec<Value> {
        let p = bridge_git::checkout::CheckoutPaths::new(&self.layout.project_dir(), id)
            .unwrap()
            .runtime_dir
            .join("proof-prompts.jsonl");
        fs::read_to_string(p)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect::<Vec<_>>();
        for worker in workers {
            let _ = worker.join();
        }
        if let Ok(s) = self.layout.open_readonly() {
            for t in s
                .list_tasks(self.project.id(), false, 100, 0)
                .unwrap_or_default()
            {
                let _ = bridge_runtime::stop_worktree_server(
                    &self.layout,
                    &self.project,
                    t.task_id,
                    &[],
                );
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn git(root: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
}
struct Model;
impl ReviewClient for Model {
    fn call(
        &mut self,
        kind: Operation,
        _: &Path,
        ctx: &Value,
        _: Duration,
        _: &mut dyn FnMut() -> bool,
    ) -> Result<Value, CodexError> {
        Ok(match kind {
            Operation::Prepare => {
                json!({"task":format!("PROOF:{}",if ctx["step"]["id"]=="__final__"{"final"}else{ctx["step"]["id"].as_str().unwrap()})})
            }
            Operation::Review => {
                json!({"decision":"accept","summary":"Criteria verified","findings":[]})
            }
        })
    }
}
#[test]
fn production_worker_multistep_workflow_proves_inheritance_verification_and_delivery() {
    let _serial = PROOF_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new(false);
    let plan: Value = serde_json::from_str(include_str!(
        "../../../docs/fixtures/runtime-automation-v17.json"
    ))
    .unwrap();
    let original =
        bridge_artifact::fingerprint(&bridge_git::take_snapshot(f.project.workspace()).unwrap());
    let run = create_run(&f.project, &f.layout, &plan["base_plan"]).unwrap();
    let mut c = Coordinator::open(
        f.project.clone(),
        f.layout.clone(),
        run.id(),
        f.spawner(None),
        vec![f.project.clone()],
        Model,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(50);
    let (mut crashed_submit, mut crashed_accept) = (false, false);
    loop {
        assert!(Instant::now() < deadline, "{}", c.document());
        if c.document()["steps"][c.document()["index"].as_u64().unwrap() as usize]["phase"]
            == "accept"
        {
            f.join();
        }
        let index = c.document()["index"].as_u64().unwrap() as usize;
        let phase = c.document()["steps"][index]["phase"].as_str().unwrap_or("");
        let boundary = if phase == "submit" && !crashed_submit {
            crashed_submit = true;
            Some("after_submit")
        } else if phase == "accept" && !crashed_accept {
            crashed_accept = true;
            Some("after_accept")
        } else {
            None
        };
        if let Some(boundary) = boundary {
            assert_eq!(
                c.tick_with_fault(|p| p == boundary).unwrap_err().0,
                "simulated_crash"
            );
            f.join();
            drop(c);
            c = Coordinator::open(
                f.project.clone(),
                f.layout.clone(),
                run.id(),
                f.spawner(None),
                vec![f.project.clone()],
                Model,
            )
            .unwrap();
            continue;
        }
        let result = c.tick().unwrap();
        assert_ne!(result, Tick::Blocked, "{}", c.document());
        if result == Tick::Done {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        AutomationRunStore::new(f.layout.clone())
            .load(Some(run.id()))
            .unwrap()
            .status(),
        RunStatus::Completed
    );
    f.join();
    for item in c.document()["steps"].as_array().unwrap() {
        let id = item["task_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(f.trace(id).len(), 1);
    }
    assert_eq!(
        fs::read_to_string(f.project.workspace().join("consumer.py")).unwrap(),
        "from module import add\nassert add(4,5)==9\n"
    );
    let after =
        bridge_artifact::fingerprint(&bridge_git::take_snapshot(f.project.workspace()).unwrap());
    assert_eq!(original["head"], after["head"]);
    assert_eq!(original["index_fingerprint"], after["index_fingerprint"]);
}
fn tool(server: &bridge_mcp::McpServer, name: &str, args: Value) -> Value {
    bridge_mcp::protocol::Protocol::stateless_http().handle(server,json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).unwrap()["result"]["structuredContent"].clone()
}
#[test]
fn parallel_workers_rendezvous_proves_overlap_and_independent_acceptance() {
    let _serial = PROOF_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new(true);
    fs::write(f.project.workspace().join("check.py"),"import sys\nfrom pathlib import Path\np=Path(sys.argv[1])\nassert p.read_text()==p.stem+'\\n'\n").unwrap();
    git(f.project.workspace(), &["add", "check.py"]);
    git(
        f.project.workspace(),
        &[
            "-c",
            "user.name=Proof",
            "-c",
            "user.email=p@example.invalid",
            "commit",
            "-qm",
            "checker",
        ],
    );
    let main = bridge_git::take_snapshot(f.project.workspace()).unwrap();
    let gate = f.root.join("rendezvous");
    fs::create_dir(&gate).unwrap();
    let server = bridge_mcp::McpServer::open(f.project.clone(), f.layout.clone())
        .unwrap()
        .with_workers(f.spawner(Some(gate.clone())), vec![f.project.clone()])
        .unwrap();
    let mut ids = vec![];
    for step in ["left", "right"] {
        let response = tool(
            &server,
            "submit_task",
            json!({"request_id":step,"task":format!("PROOF:{step}"),"allowed_paths":[format!("{step}.txt")],"test_commands":[format!("python3 -B check.py {step}.txt")]}),
        );
        ids.push(
            response["task_id"]
                .as_str()
                .unwrap()
                .parse::<bridge_domain::TaskId>()
                .unwrap(),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(12);
    while !gate.join("left").exists() || !gate.join("right").exists() {
        assert!(Instant::now() < deadline, "models did not rendezvous");
        thread::sleep(Duration::from_millis(10));
    }
    let refused = tool(
        &server,
        "submit_task",
        json!({"request_id":"overlap","task":"PROOF:left","allowed_paths":["left.txt"],"test_commands":["python3 -B check.py left.txt"]}),
    );
    assert!(refused.get("error").is_some(), "{refused}");
    assert_eq!(
        f.layout
            .open_readonly()
            .unwrap()
            .list_tasks(f.project.id(), false, 100, 0)
            .unwrap()
            .len(),
        2
    );
    fs::write(gate.join("release"), "go").unwrap();
    f.join();
    let traces = ids.iter().map(|id| f.trace(*id)).collect::<Vec<_>>();
    assert!(traces.iter().all(|t| t.len() == 1));
    let a = &traces[0][0];
    let b = &traces[1][0];
    assert!(
        a["started"].as_f64().unwrap() < b["finished"].as_f64().unwrap()
            && b["started"].as_f64().unwrap() < a["finished"].as_f64().unwrap()
    );
    for id in &ids {
        assert_eq!(
            tool(&server, "task_status", json!({"task_id":id}))["status"],
            "awaiting_review"
        );
        assert_eq!(
            tool(&server, "accept_task", json!({"task_id":id}))["status"],
            "accepted"
        );
    }
    let s = f.layout.open_readonly().unwrap();
    let count: i64 = s
        .connection()
        .query_row("SELECT COUNT(*) FROM active_writers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        bridge_git::take_snapshot(f.project.workspace()).unwrap(),
        main
    );
}
