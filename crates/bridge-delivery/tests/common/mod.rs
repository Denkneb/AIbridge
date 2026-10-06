use bridge_config::{ProjectEntry, load_config_with_state_root};
use bridge_domain::TaskId;
use bridge_storage::{CreateTaskInput, RustStateLayout, WorktreeRegistration, WorktreeStatus};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    process::Command,
    sync::{Mutex, MutexGuard},
};
// These byte/durable-boundary fixtures do not test concurrent delivery. A
// subprocess fork from another fixture can inherit CLOEXEC flock descriptors
// until exec and briefly make a just-released private project look busy.
// Keep this harness sequential; production admission stays nonblocking.
static FIXTURES: Mutex<()> = Mutex::new(());
pub struct Fixture {
    _serial: MutexGuard<'static, ()>,
    pub root: PathBuf,
    pub project: ProjectEntry,
    pub layout: RustStateLayout,
    pub id: TaskId,
    pub checkout: PathBuf,
}
impl Fixture {
    pub fn new() -> Self {
        let serial = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("bridge-delivery-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let main = root.join("main");
        fs::create_dir(&main).unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&main)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "fixture"]);
        git(&["config", "user.email", "fixture@example.test"]);
        fs::create_dir(main.join("src")).unwrap();
        fs::write(main.join("src/a"), b"base\0binary").unwrap();
        fs::write(main.join("src/remove"), "remove").unwrap();
        fs::write(main.join("src/executable"), "exec").unwrap();
        symlink("a", main.join("src/link")).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        fs::write(root.join("password"), "fixture-secret").unwrap();
        fs::set_permissions(root.join("password"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(root.join("projects.toml"),format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:9000\"\npassword_file={}\nmax_rounds=3\nexecution_mode=\"worktree\"\n",json!(main),json!(root.join("password")))).unwrap();
        let project = load_config_with_state_root(&root.join("projects.toml"), &root.join("state"))
            .unwrap()
            .project("proj")
            .unwrap()
            .clone();
        let layout = RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
        layout.initialize().unwrap();
        let id = uuid::Uuid::new_v4().to_string().parse().unwrap();
        let baseline = bridge_git::take_snapshot(&main).unwrap();
        let base = baseline.head().unwrap().as_str().to_owned();
        let mut storage = layout.open().unwrap();
        storage
            .create_task(CreateTaskInput {
                task_id: id,
                project_id: project.id().clone(),
                workspace: main.to_str().unwrap().into(),
                task: "change".into(),
                request_id: "request".into(),
                payload_hash: "hash".into(),
                base_head: Some(base.clone()),
                allowed_paths: vec!["src/".into()],
                test_commands: vec![],
                snapshot: Some(baseline.to_json().unwrap()),
            })
            .unwrap();
        storage.connection().execute_batch("UPDATE tasks SET status='accepted',execution_mode='worktree';DELETE FROM active_writers;").unwrap();
        let binding =
            bridge_git::checkout::create_checkout(&main, &layout.project_dir(), id, &base).unwrap();
        storage
            .register_worktree(
                id,
                project.id(),
                binding.paths.checkout.to_str().unwrap(),
                &WorktreeRegistration {
                    runtime_dir: Some(binding.paths.runtime_dir.to_str().unwrap().into()),
                    base_head: Some(base),
                    baseline_json: Some(baseline.to_json().unwrap().to_string()),
                    status: Some(WorktreeStatus::Created),
                    ..Default::default()
                },
            )
            .unwrap();
        Self {
            _serial: serial,
            root,
            project,
            layout,
            id,
            checkout: binding.paths.checkout,
        }
    }
    pub fn changes(&self) {
        fs::write(self.checkout.join("src/a"), b"result\xff\0").unwrap();
        fs::remove_file(self.checkout.join("src/remove")).unwrap();
        fs::write(self.checkout.join("src/new"), [0, 255, 10, 20]).unwrap();
        fs::set_permissions(
            self.checkout.join("src/executable"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::remove_file(self.checkout.join("src/link")).unwrap();
        symlink("new", self.checkout.join("src/link")).unwrap();
    }
    pub fn dest(&self) -> PathBuf {
        self.checkout.parent().unwrap().join("runtime/artifact")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
