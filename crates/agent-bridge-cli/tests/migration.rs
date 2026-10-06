use std::{fs, os::unix::fs::PermissionsExt, process::Command};
#[test]
fn migration_preserves_jsonc_env_config_backup_and_is_idempotent() {
    let root = std::env::temp_dir().join(format!("bridge-migration-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(root.join("main")).unwrap();
    let source = b"{/* comment */ \"provider\":{},}\n";
    fs::write(root.join("main/opencode.jsonc"), source).unwrap();
    let config = root.join("projects.toml");
    let text = format!(
        "# preserved\n[projects.proof]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4199\"\npassword_file={}\nmax_rounds=3\n",
        serde_json::json!(root.join("main")),
        serde_json::json!(root.join("password"))
    );
    fs::write(&config, &text).unwrap();
    let run = |apply: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        c.args(["migrate-opencode-config", "--project", "proof", "--config"])
            .arg(&config)
            .arg("--state-root")
            .arg(root.join("state"));
        if apply {
            c.arg("--apply");
        }
        c.output().unwrap()
    };
    let o = run(false);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!root.join("secrets").exists());
    assert_eq!(fs::read_to_string(&config).unwrap(), text);
    let o = run(true);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(fs::read(root.join("opencode/proof.json")).unwrap(), source);
    assert_eq!(fs::read(root.join("main/opencode.jsonc")).unwrap(), source);
    assert_eq!(
        fs::read_to_string(root.join("projects.toml.bak")).unwrap(),
        text
    );
    assert_eq!(
        fs::metadata(root.join("secrets/proof.env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let bytes = fs::read(&config).unwrap();
    let o = run(true);
    assert!(o.status.success());
    let first = o.stdout.split(|b| *b == b'\n').next().unwrap();
    let report: serde_json::Value = serde_json::from_slice(first).unwrap();
    assert_eq!(report["changed"], false);
    assert_eq!(fs::read(&config).unwrap(), bytes);
    fs::write(
        root.join("secrets/proof.env"),
        "OPENCODE_SERVER_PASSWORD=forbidden\n",
    )
    .unwrap();
    assert!(!run(true).status.success());
    assert_eq!(fs::read(&config).unwrap(), bytes);
    fs::remove_dir_all(root).unwrap();
}
