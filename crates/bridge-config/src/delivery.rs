use bridge_domain::{DeliveryMode, DomainError, ExecutionMode, Result};

pub(crate) fn parse(values: &toml::Table, mode: ExecutionMode) -> Result<DeliveryMode> {
    let Some(value) = values.get("delivery_mode") else {
        return Ok(DeliveryMode::Manual);
    };
    let raw = value
        .as_str()
        .ok_or_else(|| DomainError::invalid_input("project delivery_mode must be a string"))?;
    if raw.trim() != raw {
        return Err(DomainError::invalid_input(
            "project delivery_mode must not have surrounding whitespace",
        ));
    }
    let delivery = DeliveryMode::try_from(raw.to_owned()).map_err(|_| {
        DomainError::invalid_input("project delivery_mode must be manual or on_accept")
    })?;
    if delivery == DeliveryMode::OnAccept && mode != ExecutionMode::Worktree {
        return Err(DomainError::invalid_input(
            "project delivery_mode=on_accept requires execution_mode=worktree",
        ));
    }
    Ok(delivery)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        load_config,
        tests::{TempDir, project_toml_with},
    };
    #[test]
    fn delivery_config_defaults_gate_and_types_are_strict() {
        let dir = TempDir::new("delivery-mode");
        let workspace = dir.mkdir("ws");
        for (extra, expected) in [
            ("", DeliveryMode::Manual),
            ("delivery_mode='manual'\n", DeliveryMode::Manual),
            (
                "execution_mode='worktree'\ndelivery_mode='on_accept'\n",
                DeliveryMode::OnAccept,
            ),
        ] {
            let path = dir.write(
                "projects.toml",
                &project_toml_with(
                    "proj",
                    workspace.to_str().unwrap(),
                    "http://127.0.0.1:4101",
                    extra,
                ),
            );
            let config = load_config(&path).unwrap();
            assert_eq!(config.project("proj").unwrap().delivery_mode(), expected);
        }
        for raw in [
            "true",
            "1",
            "[]",
            "{}",
            "''",
            "'on_accept'",
            "' on_accept'",
            "'on_accept '",
            "'ON_ACCEPT'",
        ] {
            let path = dir.write(
                "projects.toml",
                &project_toml_with(
                    "proj",
                    workspace.to_str().unwrap(),
                    "http://127.0.0.1:4101",
                    &format!("delivery_mode={raw}\n"),
                ),
            );
            assert!(load_config(&path).is_err(), "{raw}");
        }
    }
    #[test]
    fn delivery_config_matches_v17_delta_fixture_cases() {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../../docs/fixtures/config-permission-v17.json"
        ))
        .unwrap();
        let dir = TempDir::new("delivery-corpus");
        let workspace = dir.mkdir("ws");
        let mut count = 0;
        for case in corpus["cases"].as_array().unwrap() {
            if case["operation"] != "config"
                || ![
                    "config-delivery-",
                    "config-defaults-",
                    "config-manual-",
                    "config-on-accept-",
                ]
                .iter()
                .any(|prefix| case["id"].as_str().unwrap().starts_with(prefix))
            {
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
            let result = load_config(&path);
            if case["expect"].get("error").is_some() {
                let error = result.expect_err("fixture must reject");
                let expected = match case["expect"]["error"].as_str().unwrap() {
                    "delivery_type" => "project delivery_mode must be a string",
                    "delivery_whitespace" => {
                        "project delivery_mode must not have surrounding whitespace"
                    }
                    "delivery_value" => "project delivery_mode must be manual or on_accept",
                    "delivery_requires_worktree" => {
                        "project delivery_mode=on_accept requires execution_mode=worktree"
                    }
                    _ => panic!("unknown delivery fixture error"),
                };
                assert_eq!(error.message(), expected, "{}", case["id"]);
            } else {
                let config = result.unwrap_or_else(|e| panic!("{}: {e}", case["id"]));
                assert_eq!(
                    config.project("proj").unwrap().delivery_mode().as_str(),
                    case["expect"]["delivery_mode"].as_str().unwrap()
                );
            }
        }
        assert_eq!(count, 19, "all delivery delta cases must be selected");
    }
}
