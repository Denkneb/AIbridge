//! Offline controller child: records only booleans, never credential contents.
use serde_json::json;
use std::{env, fs, path::PathBuf};
fn main() {
    let path = PathBuf::from(env::var_os("OPENCODE_CONFIG").unwrap());
    let config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let evidence = json!({
        "cwd":env::current_dir().unwrap(),
        "args_empty":env::args_os().count()==1,
        "primary_token_correct":env::var("AGENT_BRIDGE_MCP_TOKEN").as_deref()==Ok("primary-fixture-token"),
        "linked_token_correct":env::var("AGENT_BRIDGE_MCP_TOKEN_PEER").as_deref()==Ok("peer-fixture-token"),
        "executor_password_absent":env::var_os("OPENCODE_SERVER_PASSWORD").is_none(),
        "executor_username_absent":env::var_os("OPENCODE_SERVER_USERNAME").is_none(),
        "controller_agent":config["default_agent"],
        "proxy_bypass":env::var("NO_PROXY").unwrap().split(',').any(|s|s=="127.0.0.1") && env::var("no_proxy").unwrap().split(',').any(|s|s=="localhost"),
    });
    fs::write(
        path.parent()
            .unwrap()
            .join("controller-fixture-observed.json"),
        evidence.to_string(),
    )
    .unwrap();
    std::process::exit(17);
}
