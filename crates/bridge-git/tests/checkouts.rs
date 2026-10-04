use bridge_domain::TaskId;
use bridge_git::checkout::{
    CheckoutError, CheckoutPaths, create_checkout, probe_checkout, registrations, remove_checkout,
};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    state: PathBuf,
    base: String,
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git fixture failed");
    String::from_utf8(out.stdout).unwrap().trim().into()
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-checkouts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = root.join("main");
        let state = root.join("state");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("file.txt"), "base\n").unwrap();
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-qm", "fixture"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        Self {
            root,
            repo,
            state,
            base,
        }
    }
    fn id(&self) -> TaskId {
        "11111111-1111-4111-8111-111111111111".parse().unwrap()
    }
    fn create(&self) -> bridge_git::checkout::CheckoutBinding {
        create_checkout(&self.repo, &self.state, self.id(), &self.base).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[test]
fn detached_checkout_is_exact_and_preserves_main_dirty_index_refs() {
    let f = Fixture::new();
    std::fs::write(f.repo.join("file.txt"), "user change").unwrap();
    git(&f.repo, &["add", "file.txt"]);
    std::fs::write(f.repo.join("untracked.txt"), "user").unwrap();
    let before = bridge_git::take_snapshot(&f.repo).unwrap();
    let refs = git(&f.repo, &["show-ref"]);
    let wt = f.create();
    assert_eq!(
        std::fs::read_to_string(wt.paths.checkout.join("file.txt")).unwrap(),
        "base\n"
    );
    assert!(!wt.paths.checkout.join("untracked.txt").exists());
    assert_eq!(bridge_git::take_snapshot(&f.repo).unwrap(), before);
    assert_eq!(git(&f.repo, &["show-ref"]), refs);
    assert!(
        !Command::new("git")
            .args(["symbolic-ref", "-q", "HEAD"])
            .current_dir(&wt.paths.checkout)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        probe_checkout(&f.repo, &f.state, f.id(), &wt.paths.checkout, Some(&f.base)).unwrap(),
        wt
    );
    assert!(!wt.paths.runtime_dir.starts_with(&wt.paths.checkout));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&wt.paths.runtime_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
#[test]
fn invalid_base_or_occupied_slot_never_creates_or_repairs() {
    let f = Fixture::new();
    for base in ["HEAD", "--all", "missing", &"f".repeat(40)] {
        assert!(create_checkout(&f.repo, &f.state, f.id(), base).is_err());
    }
    assert!(!f.state.join("worktrees").exists());
    let wt = f.create();
    std::fs::write(wt.paths.checkout.join("keep"), "keep").unwrap();
    assert!(create_checkout(&f.repo, &f.state, f.id(), &f.base).is_err());
    assert!(wt.paths.checkout.join("keep").exists());
}
#[test]
fn explicit_remove_is_idempotent_and_never_prunes_unrelated_registration() {
    let f = Fixture::new();
    let wt = f.create();
    let other = f.root.join("other");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--detach",
            other.to_str().unwrap(),
            &f.base,
        ],
    );
    std::fs::remove_dir_all(&other).unwrap();
    std::fs::write(wt.paths.checkout.join("dirty"), "data").unwrap();
    remove_checkout(&f.repo, &f.state, f.id(), &wt.paths.checkout).unwrap();
    remove_checkout(&f.repo, &f.state, f.id(), &wt.paths.checkout).unwrap();
    assert!(registrations(&f.repo).unwrap().contains(&other));
    assert!(wt.paths.runtime_dir.exists());
}
#[test]
fn missing_registered_checkout_refuses_cleanup_and_recreation() {
    let f = Fixture::new();
    let wt = f.create();
    std::fs::remove_dir_all(&wt.paths.checkout).unwrap();
    assert!(remove_checkout(&f.repo, &f.state, f.id(), &wt.paths.checkout).is_err());
    assert!(create_checkout(&f.repo, &f.state, f.id(), &f.base).is_err());
    assert!(registrations(&f.repo).unwrap().contains(&wt.paths.checkout));
}
#[test]
fn wrong_repository_base_backref_and_persisted_path_are_refused() {
    let f = Fixture::new();
    let wt = f.create();
    let other = Fixture::new();
    assert!(
        probe_checkout(
            &other.repo,
            &f.state,
            f.id(),
            &wt.paths.checkout,
            Some(&f.base)
        )
        .is_err()
    );
    assert!(probe_checkout(&f.repo, &f.state, f.id(), &f.repo, Some(&f.base)).is_err());
    std::fs::write(
        wt.git_dir.join("gitdir"),
        f.repo.join(".git").to_str().unwrap(),
    )
    .unwrap();
    assert!(probe_checkout(&f.repo, &f.state, f.id(), &wt.paths.checkout, Some(&f.base)).is_err());
    assert!(remove_checkout(&f.repo, &f.state, f.id(), &wt.paths.checkout).is_err());
    assert!(wt.paths.checkout.exists());
}
#[cfg(unix)]
#[test]
fn symlink_at_every_slot_component_is_refused_without_touching_target() {
    use std::os::unix::fs::symlink;
    for component in ["root", "task", "checkout", "runtime"] {
        let f = Fixture::new();
        let target = f.root.join("outside");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("sentinel"), "keep").unwrap();
        let root = f.state.join("worktrees");
        let task = root.join(f.id().to_string());
        let path = match component {
            "root" => root.clone(),
            "task" => {
                std::fs::create_dir(&root).unwrap();
                task.clone()
            }
            name => {
                std::fs::create_dir_all(&task).unwrap();
                task.join(name)
            }
        };
        symlink(&target, &path).unwrap();
        assert_eq!(
            CheckoutPaths::new(&f.state, f.id()).unwrap_err(),
            CheckoutError::Traversal
        );
        assert!(create_checkout(&f.repo, &f.state, f.id(), &f.base).is_err());
        assert_eq!(
            std::fs::read_to_string(target.join("sentinel")).unwrap(),
            "keep"
        );
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 1);
    }
}
#[test]
fn unsupported_features_and_state_inside_workspace_refuse_before_add() {
    for name in [".gitmodules", ".lfsconfig", ".gitattributes"] {
        let f = Fixture::new();
        std::fs::write(f.repo.join(name), "*.bin filter=lfs\n").unwrap();
        assert_eq!(
            create_checkout(&f.repo, &f.state, f.id(), &f.base).unwrap_err(),
            CheckoutError::Unsupported
        );
        assert!(!f.state.join("worktrees").exists());
    }
    let f = Fixture::new();
    git(&f.repo, &["config", "core.sparseCheckout", "true"]);
    assert!(f.create_error());
    let f = Fixture::new();
    let state = f.repo.join("state");
    std::fs::create_dir(&state).unwrap();
    assert!(create_checkout(&f.repo, &state, f.id(), &f.base).is_err());
    assert!(!state.join("worktrees").exists());
}
impl Fixture {
    fn create_error(&self) -> bool {
        create_checkout(&self.repo, &self.state, self.id(), &self.base).is_err()
    }
}

#[test]
fn worktree_scopes_reject_nested_external_and_traversing_paths_before_creation() {
    use bridge_git::checkout::check_worktree_scopes;
    let f = Fixture::new();
    assert!(check_worktree_scopes(&f.repo, &["file.txt".into(), "new/sub/file".into()]).is_ok());
    let nested = f.repo.join("nested");
    std::fs::create_dir(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    for scope in ["nested", "nested/new/file", "../outside", "/tmp/outside"] {
        assert!(check_worktree_scopes(&f.repo, &[scope.into()]).is_err());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&f.state, f.repo.join("external")).unwrap();
        std::os::unix::fs::symlink(&nested, f.repo.join("nested-link")).unwrap();
        std::fs::write(f.state.join("external-file"), "external").unwrap();
        std::os::unix::fs::symlink(f.state.join("external-file"), f.repo.join("file-link"))
            .unwrap();
        for scope in ["external", "nested-link", "file-link"] {
            assert!(check_worktree_scopes(&f.repo, &[scope.into()]).is_err());
        }
    }
    assert!(!f.state.join("worktrees").exists());
}
