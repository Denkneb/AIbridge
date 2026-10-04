use bridge_domain::{DomainError, Result};
use std::path::{Component, Path, PathBuf};

pub(crate) fn parse(values: &toml::Table) -> Result<bool> {
    values
        .get("auto_approve_state_directory")
        .map_or(Ok(false), |value| {
            value.as_bool().ok_or_else(|| {
                DomainError::invalid_input("project auto_approve_state_directory must be a boolean")
            })
        })
}
/// Builds a narrow OpenCode external-directory rule without touching state.
/// # Errors
/// Rejects relative/root paths, unrepresentable text and `*`/`?` wildcard roots.
pub fn state_directory_permission_pattern(state_root: &Path) -> Result<String> {
    if !state_root.is_absolute() {
        return Err(DomainError::invalid_input(
            "state directory approval requires absolute state root",
        ));
    }
    let mut normalized = PathBuf::new();
    for part in state_root.components() {
        match part {
            Component::ParentDir => {
                if normalized.parent().is_some() {
                    normalized.pop();
                }
            }
            Component::CurDir => {}
            _ => normalized.push(part.as_os_str()),
        }
    }
    if normalized.parent().is_none() {
        return Err(DomainError::invalid_input(
            "state directory approval cannot scope filesystem root",
        ));
    }
    let text = normalized
        .to_str()
        .ok_or_else(|| DomainError::invalid_input("state directory approval root is not UTF-8"))?;
    if text.contains(['*', '?']) {
        return Err(DomainError::invalid_input(
            "state directory approval root contains wildcard characters",
        ));
    }
    Ok(format!("{text}/*"))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        load_config, load_config_with_state_root,
        tests::{TempDir, project_toml_with},
    };
    #[test]
    fn state_approval_matches_delta_config_and_boundaries_without_creating_state() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../docs/fixtures/config-permission-v17.json"
        ))
        .unwrap();
        let dir = TempDir::new("state-approval-corpus");
        let workspace = dir.mkdir("ws");
        let root = workspace.parent().unwrap().join("uncreated-state");
        let mut count = 0;
        for case in corpus["cases"].as_array().unwrap() {
            if case["operation"] != "config" {
                continue;
            }
            count += 1;
            let path = dir.write(
                "projects.toml",
                &project_toml_with(
                    "proj",
                    workspace.to_str().unwrap(),
                    "http://127.0.0.1:4101",
                    case["toml_extra"].as_str().unwrap(),
                ),
            );
            let state = case["state_root"]
                .as_str()
                .unwrap()
                .replace("${STATE}", root.to_str().unwrap())
                .replace("${TMP}", workspace.parent().unwrap().to_str().unwrap());
            let result = load_config_with_state_root(&path, Path::new(&state));
            if let Some(expected) = case["expect"].get("error") {
                let error = result.expect_err("delta fixture must reject");
                let message = match expected.as_str().unwrap() {
                    "state_type" => "project auto_approve_state_directory must be a boolean",
                    "state_root" => "state directory approval cannot scope filesystem root",
                    "state_wildcard" => {
                        "state directory approval root contains wildcard characters"
                    }
                    "delivery_type" => "project delivery_mode must be a string",
                    "delivery_whitespace" => {
                        "project delivery_mode must not have surrounding whitespace"
                    }
                    "delivery_value" => "project delivery_mode must be manual or on_accept",
                    "delivery_requires_worktree" => {
                        "project delivery_mode=on_accept requires execution_mode=worktree"
                    }
                    _ => panic!("unknown fixture error {expected}"),
                };
                assert_eq!(error.message(), message, "{}", case["id"]);
            } else {
                let config = result.unwrap_or_else(|e| panic!("{}: {e}", case["id"]));
                let project = config.project("proj").unwrap();
                assert_eq!(
                    project.auto_approve_state_directory(),
                    case["expect"]["auto_approve_state_directory"]
                        .as_bool()
                        .unwrap()
                );
                assert_eq!(
                    project.delivery_mode().as_str(),
                    case["expect"]["delivery_mode"].as_str().unwrap()
                );
                assert!(project.auto_approve_external_directories().is_empty());
            }
            assert!(!root.exists());
        }
        assert_eq!(count, 35);
    }
    #[test]
    fn explicit_root_is_required_only_for_optin_and_cannot_expand_to_filesystem_root() {
        let dir = TempDir::new("state-approval-root");
        let workspace = dir.mkdir("ws");
        let path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().unwrap(),
                "http://127.0.0.1:4101",
                "auto_approve_state_directory=true\n",
            ),
        );
        assert!(load_config(&path).is_err());
        for root in ["/", "/tmp/..", "relative", "/tmp/secret*", "/tmp/secret?"] {
            assert!(load_config_with_state_root(&path, Path::new(root)).is_err());
        }
        assert_eq!(
            state_directory_permission_pattern(Path::new("/tmp/private/./state/")).unwrap(),
            "/tmp/private/state/*"
        );
        dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().unwrap(),
                "http://127.0.0.1:4101",
                "auto_approve_state_directory=false\n",
            ),
        );
        assert!(load_config(&path).is_ok());
        assert!(load_config_with_state_root(&path, Path::new("/")).is_ok());
    }
}
