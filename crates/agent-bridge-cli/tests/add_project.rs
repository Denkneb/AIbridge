use std::{fs, os::unix::fs::PermissionsExt, process::Command};
#[test]
fn preview_apply_preserves_bytes_modes_and_repeat_is_noop() {
    let root = std::env::temp_dir().join(format!("bridge-add-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("main")).unwrap();
    let config = root.join("projects.toml");
    let original = "# Keep this comment and formatting\n[projects]\n";
    fs::write(&config, original).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
    let run = |mode: &str| {
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .arg("add-project")
            .arg(root.join("main"))
            .args(["--id", "new-project", "--config"])
            .arg(&config)
            .arg("--state-root")
            .arg(root.join("state"))
            .arg(mode)
            .output()
            .unwrap()
    };
    let out = run("--dry-run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    assert!(!root.join("state").exists());
    assert!(!root.join("secrets").exists());
    let out = run("--apply");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = fs::read(&config).unwrap();
    assert!(bytes.starts_with(original.as_bytes()));
    assert_eq!(
        fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        fs::metadata(root.join("secrets/new-project.password"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(root.join("state/new-project"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let backups: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .filter(|p| p.path().extension().is_some_and(|e| e == "bak"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read(backups[0].path()).unwrap(), original.as_bytes());
    assert!(run("--apply").status.success());
    assert_eq!(fs::read(&config).unwrap(), bytes);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn invalid_and_symlink_bindings_do_not_write() {
    let root = std::env::temp_dir().join(format!("bridge-add-invalid-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("main")).unwrap();
    let config = root.join("config");
    fs::write(&config, "[projects]\n").unwrap();
    std::os::unix::fs::symlink(&config, root.join("link")).unwrap();
    for (id, path) in [("bad.id", "config"), ("valid", "link")] {
        let out = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .arg("add-project")
            .arg(root.join("main"))
            .args(["--id", id, "--config"])
            .arg(root.join(path))
            .arg("--state-root")
            .arg(root.join("state"))
            .arg("--apply")
            .output()
            .unwrap();
        assert!(!out.status.success());
    }
    assert_eq!(fs::read_to_string(config).unwrap(), "[projects]\n");
    assert!(!root.join("state").exists());
    fs::remove_dir_all(root).unwrap();
}
