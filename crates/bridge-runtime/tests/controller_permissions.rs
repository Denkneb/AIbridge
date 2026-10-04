use bridge_config::load_config_with_state_root;
use bridge_runtime::controller_permissions::{
    CONTROLLER_SUBAGENT_DEPTH, controller_agent_permission,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-controller-permissions-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        Self { root }
    }
    fn substitute(&self, text: &str) -> String {
        let mut result = text.to_owned();
        for (key, directory) in [
            ("${STATE}", "state"),
            ("${CUSTOM}", "custom-state"),
            ("${TMP}", ""),
        ] {
            result = result.replace(key, self.root.join(directory).to_str().unwrap());
        }
        result
    }
    fn project(&self, extra: &str) -> bridge_config::ProjectEntry {
        let config = self.root.join("projects.toml");
        std::fs::write(&config,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file=\"unused\"\nmax_rounds=3\n{extra}\n",json!(self.root.join("workspace")))).unwrap();
        load_config_with_state_root(&config, &self.root.join("state"))
            .unwrap()
            .project("proj")
            .unwrap()
            .clone()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[test]
fn six_frozen_controller_cases_keep_denies_and_scope_only_opted_in_state() {
    let corpus: Value = serde_json::from_str(include_str!(
        "../../../docs/fixtures/config-permission-v17.json"
    ))
    .unwrap();
    let mut count = 0;
    for case in corpus["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["operation"] == "controller")
    {
        count += 1;
        let f = Fixture::new();
        let extra = if case.get("override_approval").is_some() {
            "auto_approve_state_directory=true"
        } else {
            case["toml_extra"].as_str().unwrap()
        };
        let project = f.project(extra);
        let state = PathBuf::from(
            f.substitute(
                case.get("override_state_root")
                    .or_else(|| case.get("state_root"))
                    .and_then(Value::as_str)
                    .unwrap_or("${STATE}"),
            ),
        );
        let rules = controller_agent_permission(&project, &state);
        if case["expect"].get("error").is_some() {
            let error = rules.unwrap_err();
            let expected = if case["expect"]["error"] == "state_root" {
                "state directory approval cannot scope filesystem root"
            } else {
                "state directory approval root contains wildcard characters"
            };
            assert_eq!(error.message(), expected, "{}", case["id"]);
        } else {
            let expected: Value =
                serde_json::from_str(&f.substitute(&case["expect"]["permission"].to_string()))
                    .unwrap();
            assert_eq!(rules.unwrap(), expected, "{}", case["id"]);
            assert_eq!(
                CONTROLLER_SUBAGENT_DEPTH,
                case["expect"]["subagent_depth"].as_u64().unwrap() as u8
            );
        }
        assert!(project.auto_approve_external_directories().is_empty());
        assert!(!f.root.join("state").exists());
        assert!(!f.root.join("custom-state").exists());
    }
    assert_eq!(count, 6);
}
#[test]
fn default_ignores_unused_root_and_optin_normalizes_without_creating_it() {
    let f = Fixture::new();
    let default = f.project("");
    let permissions = controller_agent_permission(&default, std::path::Path::new("/")).unwrap();
    assert_eq!(
        permissions,
        json!({"edit":"deny","task":"deny","bash":"ask"})
    );
    let enabled = f.project("auto_approve_state_directory=true");
    let root = f.root.join("missing/./state");
    let permissions = controller_agent_permission(&enabled, &root).unwrap();
    assert_eq!(
        permissions["external_directory"][f.root.join("missing/state/*").to_str().unwrap()],
        "allow"
    );
    for root in [
        "relative",
        "/",
        "/tmp/state*",
        "/tmp/state?",
        "/tmp/state/../..",
    ] {
        assert!(controller_agent_permission(&enabled, std::path::Path::new(root)).is_err());
    }
    assert!(!f.root.join("missing").exists());
}
