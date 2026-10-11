use bridge_desktop::projects::ProjectService;
use bridge_domain::TaskId;
use bridge_storage::{CreateTaskInput, WorktreeRegistration, WorktreeStatus};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};
struct Fixture {
    root: PathBuf,
    service: ProjectService,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bridge-diff-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("main")).unwrap();
        fs::create_dir(root.join("peer")).unwrap();
        let config = root.join("projects.toml");
        fs::write(&config,format!("[projects.primary]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file={}\nmax_rounds=3\n[projects.peer]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4102\"\npassword_file={}\nmax_rounds=3\n",json!(root.join("main")),json!(root.join("password")),json!(root.join("peer")),json!(root.join("peer-password")))).unwrap();
        let service = ProjectService::new(config, root.join("state")).unwrap();
        let f = Self { root, service };
        f.git(&["init", "-b", "main"]);
        f.git(&["config", "user.name", "Fixture"]);
        f.git(&["config", "user.email", "fixture@example.invalid"]);
        fs::write(f.root.join("main/file.txt"), "first\nстарое\nlast\n").unwrap();
        fs::write(f.root.join("main/deleted.txt"), "delete me\n").unwrap();
        f.git(&["add", "."]);
        f.git(&["commit", "-m", "base"]);
        f
    }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.root.join("main"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{:?}", args);
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn task(&self) -> TaskId {
        let (project, layout) = self.service.project("primary").unwrap();
        layout.initialize().unwrap();
        let mut storage = layout.open().unwrap();
        let baseline = bridge_git::take_snapshot(project.workspace()).unwrap();
        let id = uuid::Uuid::new_v4().to_string().parse().unwrap();
        storage
            .create_task(CreateTaskInput {
                task_id: id,
                project_id: project.id().clone(),
                workspace: project.workspace().to_string_lossy().into(),
                task: "diff fixture".into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                payload_hash: "hash".into(),
                base_head: baseline.head().map(|h| h.as_str().into()),
                allowed_paths: vec!["*".into()],
                test_commands: vec![],
                snapshot: Some(baseline.to_json().unwrap()),
            })
            .unwrap();
        id
    }
    fn list(&self, id: TaskId) -> Value {
        self.service
            .task_diff("primary", &id.to_string(), 0)
            .unwrap()
    }
    fn file(&self, id: TaskId, path: &str) -> Value {
        let list = self.list(id);
        self.service
            .task_diff_file(
                "primary",
                &id.to_string(),
                0,
                path,
                list["revision"].as_str().unwrap(),
            )
            .unwrap()
    }
    fn worktree(&self, id: TaskId) -> PathBuf {
        let (project, layout) = self.service.project("primary").unwrap();
        let mut storage = layout.open().unwrap();
        let task = storage.get_task(id).unwrap().unwrap();
        let binding = bridge_git::checkout::create_checkout(
            project.workspace(),
            &layout.project_dir(),
            id,
            task.base_head.as_deref().unwrap(),
        )
        .unwrap();
        let baseline = bridge_git::take_snapshot(&binding.paths.checkout)
            .unwrap()
            .to_json()
            .unwrap();
        storage
            .connection()
            .execute(
                "UPDATE tasks SET execution_mode='worktree' WHERE task_id=?1",
                [id.to_string()],
            )
            .unwrap();
        storage
            .register_worktree(
                id,
                project.id(),
                binding.paths.checkout.to_str().unwrap(),
                &WorktreeRegistration {
                    runtime_dir: Some(binding.paths.runtime_dir.to_string_lossy().into()),
                    base_head: task.base_head,
                    baseline_json: Some(baseline.to_string()),
                    status: Some(WorktreeStatus::Created),
                    ..Default::default()
                },
            )
            .unwrap();
        binding.paths.checkout
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn external_repository_uses_frozen_root_and_refuses_a_substituted_symlink() {
    let f = Fixture::new();
    let peer = f.root.join("peer");
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.name", "Fixture"],
        vec!["config", "user.email", "fixture@example.invalid"],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&peer)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    fs::write(peer.join("file.txt"), "external old\n").unwrap();
    for args in [vec!["add", "."], vec!["commit", "-m", "base"]] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&peer)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let external = bridge_git::take_snapshot(&peer).unwrap().to_json().unwrap();
    let id = f.task();
    let (_, layout) = f.service.project("primary").unwrap();
    let storage = layout.open().unwrap();
    let mut snapshot = storage.get_task(id).unwrap().unwrap().snapshot.unwrap();
    let mut external = external;
    external["root"] = json!(peer);
    snapshot["external_repositories"] = json!([external]);
    storage
        .connection()
        .execute(
            "UPDATE tasks SET snapshot=?1 WHERE task_id=?2",
            rusqlite::params![snapshot.to_string(), id.to_string()],
        )
        .unwrap();
    fs::write(peer.join("file.txt"), "external new\n").unwrap();
    let list = f.service.task_diff("primary", &id.to_string(), 1).unwrap();
    assert_eq!(list["total"], 1);
    assert_eq!(list["repositories"].as_array().unwrap().len(), 2);
    let diff = f
        .service
        .task_diff_file(
            "primary",
            &id.to_string(),
            1,
            "file.txt",
            list["revision"].as_str().unwrap(),
        )
        .unwrap();
    assert!(
        diff["patch"]
            .as_str()
            .unwrap()
            .contains("-external old\n+external new")
    );
    assert!(!list.to_string().contains(peer.to_str().unwrap()));
    fs::rename(&peer, f.root.join("peer-held")).unwrap();
    std::os::unix::fs::symlink(f.root.join("peer-held"), &peer).unwrap();
    assert!(f.service.task_diff("primary", &id.to_string(), 1).is_err());
}

#[test]
fn diff_rejects_a_symlinked_parent_directory() {
    let f = Fixture::new();
    fs::create_dir(f.root.join("main/nested")).unwrap();
    fs::write(f.root.join("main/nested/file.txt"), "old\n").unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-m", "nested"]);
    let id = f.task();
    fs::rename(f.root.join("main/nested"), f.root.join("outside")).unwrap();
    fs::write(f.root.join("outside/file.txt"), "outside\n").unwrap();
    std::os::unix::fs::symlink(f.root.join("outside"), f.root.join("main/nested")).unwrap();
    let list = f.list(id);
    assert!(
        f.service
            .task_diff_file(
                "primary",
                &id.to_string(),
                0,
                "nested/file.txt",
                list["revision"].as_str().unwrap()
            )
            .is_err()
    );
}

#[test]
fn diff_includes_new_deleted_and_unicode_files_without_mutating_git_or_state() {
    let f = Fixture::new();
    let id = f.task();
    fs::write(f.root.join("main/file.txt"), "first\nновое\nlast\n").unwrap();
    fs::write(
        f.root.join("main/new file.txt"),
        "<script>safe text</script>\n",
    )
    .unwrap();
    fs::remove_file(f.root.join("main/deleted.txt")).unwrap();
    let head = f.git(&["rev-parse", "HEAD"]);
    let index = fs::read(f.root.join("main/.git/index")).unwrap();
    let (_, layout) = f.service.project("primary").unwrap();
    let _observer = layout.open_readonly().unwrap();
    let stamp = f.service.dashboard_revision("primary", false).unwrap();
    let list = f.list(id);
    assert_eq!(list["total"], 3);
    assert_eq!(list["files"][0]["change"], "deleted");
    assert!(
        f.file(id, "file.txt")["patch"]
            .as_str()
            .unwrap()
            .contains("-старое\n+новое")
    );
    assert!(
        f.file(id, "new file.txt")["patch"]
            .as_str()
            .unwrap()
            .contains("+<script>safe text</script>")
    );
    assert!(
        f.file(id, "deleted.txt")["patch"]
            .as_str()
            .unwrap()
            .contains("-delete me")
    );
    assert_eq!(head, f.git(&["rev-parse", "HEAD"]));
    assert_eq!(index, fs::read(f.root.join("main/.git/index")).unwrap());
    assert_eq!(
        stamp,
        f.service.dashboard_revision("primary", false).unwrap()
    );
    assert!(
        !f.file(id, "file.txt")
            .to_string()
            .contains(f.root.to_str().unwrap())
    );
}
#[test]
fn diff_rejects_cross_project_arbitrary_paths_and_stale_revision() {
    let f = Fixture::new();
    let id = f.task();
    fs::write(f.root.join("main/file.txt"), "changed\n").unwrap();
    let list = f.list(id);
    assert!(f.service.task_diff("peer", &id.to_string(), 0).is_err());
    assert!(f.service.task_diff("primary", &id.to_string(), 1).is_err());
    for path in [
        "../password",
        "/etc/passwd",
        ".git/config",
        "missing",
        "deleted.txt",
    ] {
        assert!(
            f.service
                .task_diff_file(
                    "primary",
                    &id.to_string(),
                    0,
                    path,
                    list["revision"].as_str().unwrap()
                )
                .is_err()
        );
    }
    fs::write(f.root.join("main/file.txt"), "changed again\n").unwrap();
    assert!(
        f.service
            .task_diff_file(
                "primary",
                &id.to_string(),
                0,
                "file.txt",
                list["revision"].as_str().unwrap()
            )
            .unwrap_err()
            .contains("обновите")
    );
}
#[test]
fn dirty_baseline_is_never_misrepresented_as_commit_content() {
    let f = Fixture::new();
    fs::write(f.root.join("main/file.txt"), "before task, dirty\n").unwrap();
    let id = f.task();
    fs::write(f.root.join("main/file.txt"), "after task\n").unwrap();
    assert_eq!(f.file(id, "file.txt")["kind"], "baseline_unavailable");
}
#[test]
fn binary_oversized_and_secret_contents_are_not_returned() {
    let f = Fixture::new();
    let id = f.task();
    fs::write(f.root.join("main/binary.bin"), [0, 255, 1]).unwrap();
    fs::write(
        f.root.join("main/large.txt"),
        vec![b'a'; bridge_git::diff::FILE_LIMIT + 1],
    )
    .unwrap();
    let secret = format!("sk-proj-{}", "a".repeat(25));
    fs::write(f.root.join("main/secret.txt"), &secret).unwrap();
    assert_eq!(f.file(id, "binary.bin")["kind"], "binary");
    assert_eq!(f.file(id, "large.txt")["kind"], "large");
    let result = f.file(id, "secret.txt");
    assert_eq!(result["kind"], "redacted");
    assert!(!result.to_string().contains(&secret));
}
#[test]
fn literal_glob_names_and_symlink_targets_are_reviewed_without_following_links() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    fs::write(f.root.join("main/*.txt"), "literal old\n").unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-m", "literal"]);
    let id = f.task();
    fs::write(f.root.join("main/*.txt"), "literal new\n").unwrap();
    symlink("/etc/passwd", f.root.join("main/link")).unwrap();
    let result = f.file(id, "link");
    assert!(result["patch"].as_str().unwrap().contains("+/etc/passwd"));
    assert_eq!(result["after_mode"], "120000");
    assert!(!result.to_string().contains("root:x:"));
    assert!(
        f.file(id, "*.txt")["patch"]
            .as_str()
            .unwrap()
            .contains("-literal old\n+literal new")
    );
}
#[test]
fn worktree_diff_is_isolated_from_main_and_detects_missing_checkout() {
    let f = Fixture::new();
    let id = f.task();
    let root = f.worktree(id);
    fs::write(root.join("file.txt"), "task worktree\n").unwrap();
    fs::write(f.root.join("main/file.txt"), "unrelated main\n").unwrap();
    let patch = f.file(id, "file.txt")["patch"].as_str().unwrap().to_owned();
    assert!(patch.contains("+task worktree"));
    assert!(!patch.contains("unrelated main"));
    fs::rename(&root, root.with_extension("missing")).unwrap();
    assert!(f.service.task_diff("primary", &id.to_string(), 0).is_err());
}
#[test]
fn inherited_dirty_baseline_uses_exact_parent_bytes() {
    let f = Fixture::new();
    let id = f.task();
    let root = f.worktree(id);
    fs::write(root.join("file.txt"), "inherited\n").unwrap();
    let baseline = bridge_git::take_snapshot(&root).unwrap().to_json().unwrap();
    let (p, l) = f.service.project("primary").unwrap();
    let storage = l.open().unwrap();
    storage
        .connection()
        .execute(
            "UPDATE tasks SET status='accepted' WHERE task_id=?1",
            [id.to_string()],
        )
        .unwrap();
    storage
        .connection()
        .execute(
            "DELETE FROM active_writers WHERE task_id=?1",
            [id.to_string()],
        )
        .unwrap();
    let child = f.task();
    let child_root = f.worktree(child);
    fs::write(child_root.join("file.txt"), "next step\n").unwrap();
    let snapshot = json!({"automation_parent":id.to_string()});
    storage
        .connection()
        .execute(
            "UPDATE tasks SET snapshot=?1 WHERE task_id=?2",
            rusqlite::params![snapshot.to_string(), child.to_string()],
        )
        .unwrap();
    storage
        .connection()
        .execute(
            "UPDATE worktrees SET baseline_json=?1 WHERE task_id=?2",
            rusqlite::params![baseline.to_string(), child.to_string()],
        )
        .unwrap();
    let patch = f.file(child, "file.txt")["patch"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(patch.contains("-inherited\n+next step"));
    fs::write(root.join("file.txt"), "parent changed\n").unwrap();
    assert_eq!(f.file(child, "file.txt")["kind"], "baseline_unavailable");
    assert_eq!(p.id().as_str(), "primary");
}

#[test]
fn project_diff_without_tasks_distinguishes_head_index_and_worktree() {
    let f = Fixture::new();
    let original_head = f.git(&["rev-parse", "HEAD"]);
    fs::write(f.root.join("main/file.txt"), "first\nиндекс\nlast\n").unwrap();
    f.git(&["add", "file.txt"]);
    let index = f.git(&["ls-files", "--stage"]);
    fs::write(f.root.join("main/file.txt"), "first\nрабочий\nlast\n").unwrap();
    fs::write(f.root.join("main/new.txt"), "new\n").unwrap();
    fs::remove_file(f.root.join("main/deleted.txt")).unwrap();
    for (scope, old, new, total) in [
        ("all", "старое", "рабочий", 3),
        ("staged", "старое", "индекс", 1),
        ("unstaged", "индекс", "рабочий", 3),
    ] {
        let list = f.service.project_diff("primary", scope).unwrap();
        assert_eq!(list["total"], total, "{list}");
        let file = f
            .service
            .project_diff_file(
                "primary",
                scope,
                "file.txt",
                list["revision"].as_str().unwrap(),
            )
            .unwrap();
        let patch = file["patch"].as_str().unwrap();
        assert!(patch.contains(&format!("-{old}")), "{patch}");
        assert!(patch.contains(&format!("+{new}")), "{patch}");
    }
    assert!(!f.root.join("state").exists());
    assert_eq!(f.git(&["rev-parse", "HEAD"]), original_head);
    assert_eq!(f.git(&["ls-files", "--stage"]), index);
    let list = f.service.project_diff("primary", "all").unwrap();
    fs::write(f.root.join("main/new.txt"), "changed\n").unwrap();
    assert!(
        f.service
            .project_diff_file(
                "primary",
                "all",
                "file.txt",
                list["revision"].as_str().unwrap()
            )
            .is_err()
    );
    assert!(f.service.project_diff("primary", "invalid").is_err());
    assert!(
        f.service
            .project_diff_file("primary", "all", "../password", "")
            .is_err()
    );
}
#[test]
fn project_diff_shows_staged_change_even_when_worktree_matches_head() {
    let f = Fixture::new();
    let old = fs::read(f.root.join("main/file.txt")).unwrap();
    fs::write(f.root.join("main/file.txt"), "staged only\n").unwrap();
    f.git(&["add", "file.txt"]);
    fs::write(f.root.join("main/file.txt"), old).unwrap();
    assert_eq!(
        f.service.project_diff("primary", "all").unwrap()["total"],
        0
    );
    assert_eq!(
        f.service.project_diff("primary", "staged").unwrap()["total"],
        1
    );
    assert_eq!(
        f.service.project_diff("primary", "unstaged").unwrap()["total"],
        1
    );
}
#[test]
fn project_diff_unborn_head_and_staged_deletions_are_exact() {
    let f = Fixture::new();
    f.git(&["checkout", "--orphan", "unborn"]);
    f.git(&["rm", "-rf", "."]);
    fs::write(f.root.join("main/new.txt"), "staged new\n").unwrap();
    f.git(&["add", "new.txt"]);
    for scope in ["all", "staged"] {
        let list = f.service.project_diff("primary", scope).unwrap();
        assert_eq!(list["total"], 1, "{list}");
        assert_eq!(list["files"][0]["change"], "added");
        let file = f
            .service
            .project_diff_file(
                "primary",
                scope,
                "new.txt",
                list["revision"].as_str().unwrap(),
            )
            .unwrap();
        assert!(file["patch"].as_str().unwrap().contains("+staged new"));
    }
    fs::remove_file(f.root.join("main/new.txt")).unwrap();
    assert_eq!(
        f.service.project_diff("primary", "all").unwrap()["total"],
        0
    );
    let list = f.service.project_diff("primary", "unstaged").unwrap();
    let file = f
        .service
        .project_diff_file(
            "primary",
            "unstaged",
            "new.txt",
            list["revision"].as_str().unwrap(),
        )
        .unwrap();
    assert!(file["patch"].as_str().unwrap().contains("-staged new"));
    f.git(&["checkout", "-f", "main"]);
    f.git(&["rm", "file.txt"]);
    let list = f.service.project_diff("primary", "staged").unwrap();
    let file = f
        .service
        .project_diff_file(
            "primary",
            "staged",
            "file.txt",
            list["revision"].as_str().unwrap(),
        )
        .unwrap();
    assert!(file["patch"].as_str().unwrap().contains("-старое"));
}
#[test]
fn project_diff_handles_binary_secrets_and_literal_paths() {
    let f = Fixture::new();
    fs::write(f.root.join("main/[*].txt"), "literal\n").unwrap();
    fs::write(f.root.join("main/binary.bin"), b"\0\xff").unwrap();
    fs::write(
        f.root.join("main/secret.txt"),
        "-----BEGIN PRIVATE KEY-----\nsecret\n",
    )
    .unwrap();
    let list = f.service.project_diff("primary", "all").unwrap();
    for (path, kind) in [
        ("[*].txt", "text"),
        ("binary.bin", "binary"),
        ("secret.txt", "redacted"),
    ] {
        let file = f
            .service
            .project_diff_file("primary", "all", path, list["revision"].as_str().unwrap())
            .unwrap();
        assert_eq!(file["kind"], kind, "{file}");
    }
}
