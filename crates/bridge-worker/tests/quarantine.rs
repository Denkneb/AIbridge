use bridge_config::{ProjectEntry, load_config_with_state_root};
use bridge_domain::WorkflowId;
use bridge_storage::RustStateLayout;
use bridge_worker::quarantine;
use std::{fs, path::PathBuf, process::Command};
struct Fixture {
    root: PathBuf,
    project: ProjectEntry,
    layout: RustStateLayout,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bridge-quarantine-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("main")).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Proof",
                "-c",
                "user.email=proof@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(root.join("main"))
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let config = root.join("projects.toml");
        fs::write(&config,format!("[projects.proof]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4190\"\npassword_file={}\nmax_rounds=3\n",serde_json::json!(root.join("main")),serde_json::json!(root.join("password")))).unwrap();
        let project = load_config_with_state_root(&config, &root.join("state"))
            .unwrap()
            .project("proof")
            .unwrap()
            .clone();
        let layout = RustStateLayout::new(root.join("state"), project.id().clone()).unwrap();
        layout.initialize().unwrap();
        Self {
            root,
            project,
            layout,
        }
    }
    fn orphan(&self, id: &str, registered: bool) {
        let slot = self.layout.project_dir().join("worktrees").join(id);
        fs::create_dir_all(slot.join("runtime")).unwrap();
        fs::write(slot.join("runtime/kept"), b"valuable result").unwrap();
        if registered {
            assert!(
                Command::new("git")
                    .args(["worktree", "add", "--detach"])
                    .arg(slot.join("checkout"))
                    .arg("HEAD")
                    .current_dir(self.project.workspace())
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            fs::create_dir(slot.join("checkout")).unwrap();
        }
        self.layout
            .open()
            .unwrap()
            .register_worktree_quarantine(
                &WorkflowId::try_from(id.to_string()).unwrap(),
                slot.to_str().unwrap(),
                &Default::default(),
            )
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn snapshot_move_registered_and_plain_then_explicit_purge() {
    let f = Fixture::new();
    f.orphan("plain", false);
    f.orphan("registered", true);
    let before = quarantine::preview(&f.project, &f.layout, false).unwrap();
    assert_eq!(before["candidates"].as_array().unwrap().len(), 2);
    assert!(!f.layout.project_dir().join("quarantine").exists());
    assert!(quarantine::apply(&f.project, &f.layout, false, "all").is_err());
    assert!(quarantine::apply(&f.project, &f.layout, false, "all:stale").is_err());
    quarantine::apply(
        &f.project,
        &f.layout,
        false,
        &format!("all:{}", before["snapshot"].as_str().unwrap()),
    )
    .unwrap();
    for id in ["plain", "registered"] {
        assert_eq!(
            fs::read(
                f.layout
                    .project_dir()
                    .join("quarantine")
                    .join(id)
                    .join("runtime/kept")
            )
            .unwrap(),
            b"valuable result"
        );
    }
    let purge = quarantine::preview(&f.project, &f.layout, true).unwrap();
    quarantine::apply(
        &f.project,
        &f.layout,
        true,
        &format!("all:{}", purge["snapshot"].as_str().unwrap()),
    )
    .unwrap();
    assert!(
        quarantine::preview(&f.project, &f.layout, true).unwrap()["candidates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn symlink_and_replaced_inode_refuse_old_snapshot() {
    let f = Fixture::new();
    f.orphan("plain", false);
    let before = quarantine::preview(&f.project, &f.layout, false).unwrap();
    let slot = f.layout.project_dir().join("worktrees/plain");
    fs::rename(&slot, slot.with_extension("saved")).unwrap();
    fs::create_dir(&slot).unwrap();
    assert!(
        quarantine::apply(
            &f.project,
            &f.layout,
            false,
            &format!("all:{}", before["snapshot"].as_str().unwrap())
        )
        .is_err()
    );
    fs::remove_dir(&slot).unwrap();
    std::os::unix::fs::symlink(f.project.workspace(), &slot).unwrap();
    assert!(quarantine::apply(&f.project, &f.layout, false, "plain").is_err());
    assert!(f.project.workspace().exists());
}
#[test]
fn resumes_effect_before_registry_and_partial_registered_move() {
    let f = Fixture::new();
    f.orphan("plain", false);
    f.orphan("registered", true);
    let q = f.layout.project_dir().join("quarantine");
    fs::create_dir(&q).unwrap();
    fs::rename(
        f.layout.project_dir().join("worktrees/plain"),
        q.join("plain"),
    )
    .unwrap();
    fs::create_dir(q.join("registered")).unwrap();
    bridge_git::checkout::move_orphan(
        f.project.workspace(),
        &f.layout.project_dir().join("worktrees/registered/checkout"),
        &q.join("registered/checkout"),
    )
    .unwrap();
    quarantine::apply(&f.project, &f.layout, false, "plain,registered").unwrap();
    assert_eq!(
        fs::read(q.join("registered/runtime/kept")).unwrap(),
        b"valuable result"
    );
    fs::remove_dir_all(q.join("plain")).unwrap();
    quarantine::apply(&f.project, &f.layout, true, "plain").unwrap();
}
