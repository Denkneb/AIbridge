use bridge_desktop::projects::ProjectService;
use bridge_storage::CreateTaskInput;
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

static GIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    _lock: std::sync::MutexGuard<'static, ()>,
    root: PathBuf,
    service: ProjectService,
}
impl Fixture {
    fn new(nested_peer: bool) -> Self {
        // Concurrent fork/exec from another test can briefly inherit a held flock.
        let lock = GIT_TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("bridge-branches-{}", uuid::Uuid::new_v4()));
        let main = root.join("main");
        let peer = if nested_peer {
            main.join("nested")
        } else {
            root.join("peer")
        };
        fs::create_dir_all(&peer).unwrap();
        fs::create_dir_all(&main).unwrap();
        let config = root.join("projects.toml");
        fs::write(&config,format!("[projects.primary]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file={}\nmax_rounds=3\n[projects.peer]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4102\"\npassword_file={}\nmax_rounds=3\n",json!(main),json!(root.join("primary.password")),json!(peer),json!(root.join("peer.password")))).unwrap();
        let service = ProjectService::new(config, root.join("state")).unwrap();
        let f = Self {
            _lock: lock,
            root,
            service,
        };
        f.git(&["init", "-b", "main"]);
        f.git(&["config", "user.name", "Fixture"]);
        f.git(&["config", "user.email", "fixture@example.invalid"]);
        fs::write(f.root.join("main/tracked.txt"), "main\n").unwrap();
        f.git(&["add", "tracked.txt"]);
        f.git(&["commit", "-m", "base"]);
        f.git(&["switch", "-c", "feature"]);
        fs::write(f.root.join("main/tracked.txt"), "feature\n").unwrap();
        f.git(&["commit", "-am", "feature"]);
        f.git(&["switch", "main"]);
        f
    }
    fn git(&self, args: &[&str]) -> String {
        let result = Command::new("git")
            .arg("-C")
            .arg(self.root.join("main"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().into()
    }
    fn view(&self) -> Value {
        self.service.project_branches("primary").unwrap()
    }
    fn switch(&self, reference: &str, view: &Value) -> Result<Value, String> {
        self.service.switch_project_branch(
            "primary",
            reference,
            view["current"].as_str(),
            view["head"].as_str(),
            view["workspace"].as_str().unwrap(),
        )
    }
    fn task(&self) -> String {
        let (p, l) = self.service.project("primary").unwrap();
        l.initialize().unwrap();
        let mut storage = l.open().unwrap();
        let id = uuid::Uuid::new_v4().to_string().parse().unwrap();
        storage
            .create_task(CreateTaskInput {
                task_id: id,
                project_id: p.id().clone(),
                workspace: p.workspace().to_string_lossy().into(),
                task: "unfinished".into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                payload_hash: "hash".into(),
                base_head: None,
                allowed_paths: vec!["tracked.txt".into()],
                test_commands: vec![],
                snapshot: None,
            })
            .unwrap();
        id.to_string()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn lists_local_and_known_remote_branches_and_switches_without_hooks() {
    let f = Fixture::new(false);
    f.git(&[
        "remote",
        "add",
        "origin",
        f.root.join("unused-remote").to_str().unwrap(),
    ]);
    f.git(&["update-ref", "refs/remotes/origin/remote-topic", "HEAD"]);
    f.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/remote-topic",
    ]);
    let initial = f.view();
    assert_eq!(initial["current"], "refs/heads/main");
    let refs = initial["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["reference"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(refs.contains(&"refs/heads/feature"));
    assert!(refs.contains(&"refs/remotes/origin/remote-topic"));
    assert!(!refs.contains(&"refs/remotes/origin/HEAD"));
    assert!(!f.service.state.exists());
    let hook = f.root.join("main/.git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\nprintf hook > hook-ran\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let feature = f.switch("refs/heads/feature", &initial).unwrap();
    assert_eq!(feature["current"], "refs/heads/feature");
    assert_eq!(
        fs::read_to_string(f.root.join("main/tracked.txt")).unwrap(),
        "feature\n"
    );
    assert!(!f.root.join("main/hook-ran").exists());
    let remote = f
        .switch("refs/remotes/origin/remote-topic", &feature)
        .unwrap();
    assert_eq!(remote["current"], "refs/heads/remote-topic");
    assert_eq!(
        f.git(&["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/remote-topic"
    );
}

#[test]
fn dirty_files_stale_head_and_invalid_reference_never_switch_or_discard_changes() {
    let f = Fixture::new(false);
    let initial = f.view();
    fs::write(f.root.join("main/tracked.txt"), "user edits\n").unwrap();
    assert!(
        f.switch("refs/heads/feature", &initial)
            .unwrap_err()
            .contains("незакоммиченные")
    );
    assert_eq!(
        fs::read_to_string(f.root.join("main/tracked.txt")).unwrap(),
        "user edits\n"
    );
    assert_eq!(f.view()["current"], "refs/heads/main");
    fs::write(f.root.join("main/tracked.txt"), "main\n").unwrap();
    fs::write(f.root.join("main/untracked.txt"), "keep\n").unwrap();
    assert!(f.switch("refs/heads/feature", &initial).is_err());
    assert!(f.root.join("main/untracked.txt").exists());
    fs::remove_file(f.root.join("main/untracked.txt")).unwrap();
    assert!(f.switch("--discard-changes", &initial).is_err());
    f.git(&["commit", "--allow-empty", "-m", "new head"]);
    assert!(
        f.switch("refs/heads/feature", &initial)
            .unwrap_err()
            .contains("HEAD")
    );
    assert_eq!(f.view()["current"], "refs/heads/main");
}

#[test]
fn unfinished_task_does_not_block_but_live_worker_and_nested_project_do() {
    let f = Fixture::new(true);
    let id = f.task();
    let initial = f.view();
    let (_, layout) = f.service.project("primary").unwrap();
    let bridge_worker::WorkerLockOutcome::Acquired(worker) =
        bridge_worker::WorkerLock::try_acquire_task(&layout, id.parse().unwrap()).unwrap()
    else {
        panic!("worker lock")
    };
    assert!(
        f.switch("refs/heads/feature", &initial)
            .unwrap_err()
            .contains("исполнитель")
    );
    drop(worker);
    let (_, peer) = f.service.project("peer").unwrap();
    peer.initialize().unwrap();
    let bridge_worker::WorkerLockOutcome::Acquired(worker) =
        bridge_worker::WorkerLock::try_acquire(&peer).unwrap()
    else {
        panic!("peer lock")
    };
    assert!(
        f.switch("refs/heads/feature", &initial)
            .unwrap_err()
            .contains("peer")
    );
    drop(worker);
    f.switch("refs/heads/feature", &initial).unwrap();
    assert_eq!(
        layout
            .open()
            .unwrap()
            .get_task(id.parse().unwrap())
            .unwrap()
            .unwrap()
            .status
            .to_string(),
        "implementing"
    );
}

#[test]
fn merge_in_progress_and_branch_in_another_worktree_are_refused() {
    let f = Fixture::new(false);
    let initial = f.view();
    let merge = f.root.join("main/.git/MERGE_HEAD");
    fs::write(&merge, f.git(&["rev-parse", "HEAD"])).unwrap();
    assert!(
        f.switch("refs/heads/feature", &initial)
            .unwrap_err()
            .contains("операция Git")
    );
    fs::remove_file(merge).unwrap();
    f.git(&[
        "worktree",
        "add",
        f.root.join("occupied").to_str().unwrap(),
        "feature",
    ]);
    assert!(f.switch("refs/heads/feature", &initial).is_err());
    assert_eq!(f.view()["current"], "refs/heads/main");
}

#[test]
fn detached_and_unborn_heads_are_displayed_and_project_binding_is_checked() {
    let f = Fixture::new(false);
    f.git(&["switch", "--detach"]);
    let detached = f.view();
    assert!(detached["current"].is_null());
    assert!(detached["head"].as_str().is_some());
    assert!(
        f.service
            .switch_project_branch(
                "primary",
                "refs/heads/feature",
                None,
                detached["head"].as_str(),
                "/wrong/workspace"
            )
            .is_err()
    );
    f.switch("refs/heads/feature", &detached).unwrap();
    let peer = f.root.join("peer");
    let status = Command::new("git")
        .arg("-C")
        .arg(&peer)
        .args(["init", "-b", "fresh"])
        .status()
        .unwrap();
    assert!(status.success());
    let unborn = f.service.project_branches("peer").unwrap();
    assert_eq!(unborn["current"], "refs/heads/fresh");
    assert!(unborn["head"].is_null());
    assert!(f.service.project_branches("unknown").is_err());
}

#[test]
fn ignored_files_that_would_be_overwritten_are_preserved() {
    let f = Fixture::new(false);
    fs::write(f.root.join("main/.gitignore"), "generated.txt\n").unwrap();
    f.git(&["add", ".gitignore"]);
    f.git(&["commit", "-m", "ignore generated"]);
    f.git(&["switch", "feature"]);
    fs::write(f.root.join("main/generated.txt"), "branch content\n").unwrap();
    f.git(&["add", "generated.txt"]);
    f.git(&["commit", "-m", "add generated"]);
    f.git(&["switch", "main"]);
    fs::write(f.root.join("main/generated.txt"), "private local content\n").unwrap();
    let initial = f.view();
    assert!(f.git(&["status", "--porcelain"]).is_empty());
    assert!(f.switch("refs/heads/feature", &initial).is_err());
    assert_eq!(
        fs::read_to_string(f.root.join("main/generated.txt")).unwrap(),
        "private local content\n"
    );
    assert_eq!(f.view()["current"], "refs/heads/main");
}

impl Fixture {
    fn action(&self, operation: Value, view: &Value) -> Result<Value, String> {
        let request=serde_json::from_value(json!({"workspace":view["workspace"],"current":view["current"],"head":view["head"],"operation":operation})).unwrap();
        self.service.project_git("primary", request)
    }
    fn remote(&self) {
        let bare = self.root.join("remote.git");
        assert!(
            Command::new("git")
                .args(["init", "--bare"])
                .arg(&bare)
                .output()
                .unwrap()
                .status
                .success()
        );
        self.git(&["remote", "add", "origin", bare.to_str().unwrap()]);
    }
    fn remote_head(&self, name: &str) -> Option<String> {
        let result = Command::new("git")
            .arg("--git-dir")
            .arg(self.root.join("remote.git"))
            .args(["rev-parse", "--verify", &format!("refs/heads/{name}")])
            .output()
            .unwrap();
        result
            .status
            .success()
            .then(|| String::from_utf8(result.stdout).unwrap().trim().into())
    }
    fn push(&self, plan: &Value, view: &Value) -> Result<Value, String> {
        self.action(json!({"action":"push","remote":plan["remote"],"destination":plan["destination"],"fingerprint":plan["fingerprint"]}),view)
    }
}
#[test]
fn create_branch_from_selected_base_preserves_dirty_worktree_and_rejects_bad_names() {
    let f = Fixture::new(false);
    fs::write(f.root.join("main/tracked.txt"), "private edit").unwrap();
    let view = f.view();
    assert_eq!(view["dirty"], 1);
    assert!(
        f.action(
            json!({"action":"create","name":"topic","base":"HEAD","switch":true}),
            &view
        )
        .unwrap_err()
        .contains("незакоммиченные")
    );
    let next = f
        .action(
            json!({"action":"create","name":"topic","base":"refs/heads/feature","switch":false}),
            &view,
        )
        .unwrap();
    assert_eq!(next["current"], "refs/heads/main");
    assert_eq!(
        f.git(&["rev-parse", "topic"]),
        f.git(&["rev-parse", "feature"])
    );
    assert_eq!(
        fs::read_to_string(f.root.join("main/tracked.txt")).unwrap(),
        "private edit"
    );
    for name in ["--force", "bad..name", "topic", "@{-1}"] {
        assert!(
            f.action(
                json!({"action":"create","name":name,"base":"HEAD","switch":false}),
                &next
            )
            .is_err()
        );
    }
    fs::write(f.root.join("main/tracked.txt"), "main\n").unwrap();
    let next = f
        .action(
            json!({"action":"create","name":"new-topic","base":"HEAD","switch":true}),
            &f.view(),
        )
        .unwrap();
    assert_eq!(next["current"], "refs/heads/new-topic");
}
#[test]
fn push_preview_is_read_only_push_sets_upstream_and_fetch_preserves_files() {
    let f = Fixture::new(false);
    f.remote();
    let view = f.view();
    assert_eq!(view["remotes"], json!(["origin"]));
    let plan = f
        .action(
            json!({"action":"preview","remote":"origin","destination":"main"}),
            &view,
        )
        .unwrap();
    assert_eq!(plan["ahead"], 1);
    assert_eq!(plan["behind"], 0);
    assert_eq!(plan["new_branch"], true);
    assert_eq!(plan["set_upstream"], true);
    assert!(f.remote_head("main").is_none());
    assert!(view["upstream"].is_null());
    let result = f.push(&plan, &view).unwrap();
    assert_eq!(f.remote_head("main").as_deref(), view["head"].as_str());
    assert_eq!(
        result["upstream"],
        json!({"remote":"origin","destination":"main"})
    );
    assert_eq!(result["upstream_saved"], true);
    f.git(&["push", "origin", "feature:server-topic"]);
    fs::write(f.root.join("main/tracked.txt"), "keep me").unwrap();
    let next = f
        .action(json!({"action":"fetch","remote":"origin"}), &f.view())
        .unwrap();
    assert!(
        next["branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["reference"] == "refs/remotes/origin/server-topic")
    );
    assert_eq!(next["current"], "refs/heads/main");
    assert_eq!(next["head"], view["head"]);
    assert_eq!(
        fs::read_to_string(f.root.join("main/tracked.txt")).unwrap(),
        "keep me"
    );
    let plan = f
        .action(
            json!({"action":"preview","remote":"origin","destination":"main"}),
            &next,
        )
        .unwrap();
    assert_eq!(plan["ahead"], 0);
}
#[test]
fn stale_push_approval_changed_remote_and_divergence_are_refused() {
    let f = Fixture::new(false);
    f.remote();
    let view = f.view();
    let preview = json!({"action":"preview","remote":"origin","destination":"main"});
    let plan = f.action(preview.clone(), &view).unwrap();
    f.git(&[
        "remote",
        "set-url",
        "--push",
        "origin",
        f.root.join("missing.git").to_str().unwrap(),
    ]);
    assert!(f.push(&plan, &view).is_err());
    assert!(f.remote_head("main").is_none());
    f.git(&[
        "remote",
        "set-url",
        "--push",
        "origin",
        f.root.join("remote.git").to_str().unwrap(),
    ]);
    f.git(&["commit", "--allow-empty", "-m", "new local commit"]);
    assert!(f.push(&plan, &view).unwrap_err().contains("HEAD"));
    let view = f.view();
    let plan = f.action(preview.clone(), &view).unwrap();
    f.git(&["push", "origin", "feature:main"]);
    assert!(f.push(&plan, &view).unwrap_err().contains("изменились"));
    let plan = f.action(preview, &view).unwrap();
    assert_eq!(plan["behind"], 1);
    assert_eq!(plan["ahead"], 1);
    f.git(&["config", "push.default", "matching"]);
    f.git(&["config", "remote.origin.mirror", "true"]);
    assert!(f.push(&plan, &view).is_err());
    assert_eq!(
        f.remote_head("main").unwrap(),
        f.git(&["rev-parse", "feature"])
    );
}
#[test]
fn new_git_actions_respect_active_nested_workers_and_workspace_binding() {
    let f = Fixture::new(true);
    f.remote();
    let initial = f.view();
    let (_, peer) = f.service.project("peer").unwrap();
    peer.initialize().unwrap();
    let bridge_worker::WorkerLockOutcome::Acquired(worker) =
        bridge_worker::WorkerLock::try_acquire(&peer).unwrap()
    else {
        panic!("worker lock")
    };
    for operation in [
        json!({"action":"create","name":"topic","base":"HEAD","switch":false}),
        json!({"action":"fetch","remote":"origin"}),
        json!({"action":"preview","remote":"origin","destination":"main"}),
    ] {
        assert!(f.action(operation, &initial).unwrap_err().contains("peer"));
    }
    drop(worker);
    let mut wrong = initial;
    wrong["workspace"] = json!("/wrong/workspace");
    assert!(
        f.action(json!({"action":"fetch","remote":"origin"}), &wrong)
            .unwrap_err()
            .contains("Workspace")
    );
}
