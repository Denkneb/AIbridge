//! Observe one dedicated round's messages and atomically persist its accounting.
use bridge_opencode::Message;
use bridge_storage::{
    FinishRoundInput, RoundUpdateError, RoundUpdateOutcome, StorageConnection,
    usage::{add_usage, normalize_usage},
};
use serde_json::{Value, json};
use std::collections::HashSet;

/// Recalculate from message history at every poll (never sum repeated polls).
/// Later TUI user continuations belong to the same dedicated round.
pub fn observed_accounting(messages: &[Message], outbound: &str) -> Value {
    let mut in_round = false;
    let mut users = HashSet::new();
    for message in messages.iter().filter(|m| m.is_user()) {
        if message.info().id() == Some(outbound) {
            in_round = true;
        }
        if in_round && let Some(id) = message.info().id() {
            users.insert(id);
        }
    }
    let mut usage = normalize_usage(&Value::Null);
    let mut model = None;
    for message in messages
        .iter()
        .filter(|m| m.is_assistant() && m.info().parent_id().is_some_and(|p| users.contains(p)))
    {
        let u = message.info().usage();
        usage = add_usage(
            &usage,
            &json!({"input":u.input(),"output":u.output(),"reasoning":u.reasoning(),"cache_read":u.cache_read(),"cache_write":u.cache_write(),"cost":u.cost()}),
        );
        if let Some((provider, id)) = message.info().model() {
            model = Some(json!({"provider_id":provider,"model_id":id}));
        }
    }
    let mut result = json!({"usage":usage});
    if let Some(model) = model {
        result["model"] = model;
    }
    result
}
/// Completion, error and deadline callers share this atomic result write.
/// Other result fields remain intact; accounting is the observed history total.
pub fn finish_round_with_accounting(
    storage: &mut StorageConnection,
    mut input: FinishRoundInput,
    messages: &[Message],
) -> Result<RoundUpdateOutcome, RoundUpdateError> {
    let outbound: Option<String> = storage.connection().query_row(
        "SELECT outbound_message_id FROM rounds WHERE task_id=?1 AND project_id=?2 AND round_number=?3",
        rusqlite::params![input.round.task_id.to_string(), input.round.project_id.as_str(), input.round.round_number],
        |row| row.get(0),
    ).map_err(RoundUpdateError::Database)?;
    let outbound = outbound
        .filter(|s| !s.is_empty())
        .ok_or(RoundUpdateError::InvalidPersistedState)?;
    let mut result = input.result_json.take().unwrap_or_else(|| json!({}));
    if result.is_null() {
        result = json!({});
    }
    let result = result
        .as_object_mut()
        .ok_or(RoundUpdateError::InvalidJson)?;
    let accounting = observed_accounting(messages, &outbound);
    result.insert("usage".to_owned(), accounting["usage"].clone());
    result.remove("model");
    if let Some(model) = accounting.get("model") {
        result.insert("model".to_owned(), model.clone());
    }
    input.result_json = Some(Value::Object(result.clone()));
    storage.finish_round(input)
}
