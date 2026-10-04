use bridge_config::{ProjectEntry, load_config_with_state_root};
use bridge_worker::auto_approval::permission_decision;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bridge-auto-policy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for path in [
            "workspace",
            "state/proj",
            "state/repo",
            "custom-state/proj",
            "external/sub",
            "outside",
            "state-sibling",
        ] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        std::fs::write(root.join("state/proj/fixture.txt"), "fixture").unwrap();
        Self { root }
    }
    fn substitute(&self, text: &str) -> String {
        let mut result = text.to_owned();
        for (key, directory) in [
            ("${WORKSPACE}", "workspace"),
            ("${STATE}", "state"),
            ("${CUSTOM}", "custom-state"),
            ("${EXTERNAL}", "external"),
            ("${OUTSIDE}", "outside"),
            ("${TMP}", ""),
        ] {
            result = result.replace(key, self.root.join(directory).to_str().unwrap());
        }
        result
    }
    fn project(&self, extra: &str, state: &Path) -> ProjectEntry {
        let config = self.root.join("projects.toml");
        std::fs::write(&config,format!("[projects.proj]\nworkspace={}\nopencode_url=\"http://127.0.0.1:4101\"\npassword_file=\"unused\"\nmax_rounds=3\n{}\n",json!(self.root.join("workspace")),self.substitute(extra))).unwrap();
        load_config_with_state_root(&config, state)
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
#[cfg(unix)]
fn all_42_frozen_worker_permission_cases_match_without_widening_git_roots() {
    let corpus: Value = serde_json::from_str(include_str!(
        "../../../docs/fixtures/config-permission-v17.json"
    ))
    .unwrap();
    let mut count = 0;
    for case in corpus["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["operation"] == "worker")
    {
        count += 1;
        let f = Fixture::new();
        for setup in case["setup"].as_array().unwrap() {
            let (link, target) = match setup.as_str().unwrap() {
                "escape_link" => ("escape", f.root.join("outside")),
                "safe_link" => ("alias", f.root.join("state/proj")),
                "broken_link" => ("broken", f.root.join("state/absent")),
                "loop_link" => ("loop", f.root.join("state/loop")),
                _ => panic!("unknown setup"),
            };
            std::os::unix::fs::symlink(target, f.root.join("state").join(link)).unwrap();
        }
        let state = PathBuf::from(f.substitute(case["state_root"].as_str().unwrap()));
        let project = f.project(case["toml_extra"].as_str().unwrap(), &state);
        let roots = project.auto_approve_external_directories().to_vec();
        let permission: Value =
            serde_json::from_str(&f.substitute(&case["permission"].to_string())).unwrap();
        let decision = permission_decision(&project, &state, &permission);
        assert_eq!(
            decision.approved(),
            case["expect"]["approve"].as_bool().unwrap(),
            "{}",
            case["id"]
        );
        assert_eq!(
            decision.reason(),
            case["expect"]["reason"].as_str().unwrap(),
            "{}",
            case["id"]
        );
        assert_eq!(project.auto_approve_external_directories(), roots);
        assert!(!format!("{decision:?}").contains(f.root.to_str().unwrap()));
    }
    assert_eq!(count, 42);
}
#[test]
fn ordinary_permissions_share_the_existing_bash_policy_and_default_to_ask() {
    let f = Fixture::new();
    let state = f.root.join("state");
    let configured = f.project(
        "auto_approve_permissions=[\"read\",\"edit\",\"bash\",\"task\",\"future-tool\"]",
        &state,
    );
    for (name, patterns, approve, reason) in [
        ("read", json!([]), true, "configured"),
        ("edit", json!(["src"]), true, "configured"),
        ("task", json!([]), true, "configured"),
        ("future-tool", json!([]), false, "unknown_permission"),
        ("bash", json!([]), false, "missing_bash_patterns"),
        ("bash", json!(["cargo test"]), true, "configured"),
        (
            "bash",
            json!(["git commit -m secret"]),
            false,
            "git_write_blocked",
        ),
        (
            "bash",
            json!(["cargo test && git push"]),
            false,
            "unprovable_shell_syntax",
        ),
        (
            "bash",
            json!(["bash -c 'git add .'"]),
            false,
            "git_write_blocked",
        ),
        (
            "bash",
            json!(["git status", "git push"]),
            false,
            "git_write_blocked",
        ),
        ("skill", json!([]), false, "not_configured"),
    ] {
        let permission = json!({"id":"req","permission":name,"patterns":patterns});
        let decision = permission_decision(&configured, &state, &permission);
        assert_eq!(
            (decision.approved(), decision.reason()),
            (approve, reason),
            "{permission}"
        );
        assert!(!format!("{decision:?}").contains("secret"));
    }
    let defaults = f.project("", &state);
    assert!(
        !permission_decision(
            &defaults,
            &state,
            &json!({"id":"req","permission":"read","patterns":[]})
        )
        .approved()
    );
    for permission in [
        json!({"id":"","permission":"read","patterns":[]}),
        json!({"id":"req","patterns":[]}),
        json!({"id":"req","permission":"read","patterns":[1]}),
    ] {
        assert!(!permission_decision(&configured, &state, &permission).approved());
    }
}
#[test]
#[cfg(unix)]
fn glob_checks_all_children_including_dangling_symlinks_and_symlinked_state_root() {
    let f = Fixture::new();
    let state = f.root.join("state");
    let project = f.project("auto_approve_state_directory=true", &state);
    let permission =
        json!({"id":"req","permission":"external_directory","patterns":[state.join("proj/*")]});
    std::os::unix::fs::symlink(state.join("missing"), state.join("proj/link")).unwrap();
    assert!(permission_decision(&project, &state, &permission).approved());
    std::fs::remove_file(state.join("proj/link")).unwrap();
    std::os::unix::fs::symlink(f.root.join("outside/missing"), state.join("proj/link")).unwrap();
    assert!(!permission_decision(&project, &state, &permission).approved());
    let alias = f.root.join("alias-state");
    std::os::unix::fs::symlink(&state, &alias).unwrap();
    assert!(
        permission_decision(
            &project,
            &alias,
            &json!({"id":"req","permission":"external_directory","patterns":[alias.join("repo")]})
        )
        .approved()
    );
    let global = f.root.join("global-state");
    std::os::unix::fs::symlink("/", &global).unwrap();
    assert!(
        !permission_decision(
            &project,
            &global,
            &json!({"id":"req","permission":"external_directory","patterns":[global.join("tmp")]})
        )
        .approved()
    );
}
