//! Saved usage accounting and read-only soft budget decisions (7.14).
use crate::{
    BUDGET_USAGE_FIELDS, BudgetValidationError, ProjectId, RoundUpdateError, StorageConnection,
    TaskBudget, TaskId,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

/// Malformed legacy accounting contributes zero, never guessed numeric values.
pub fn normalize_usage(value: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for field in BUDGET_USAGE_FIELDS {
        let value = &value[*field];
        result.insert(
            (*field).to_owned(),
            if value.is_number() && value.as_f64().is_some_and(|v| v.is_finite() && v >= 0.0) {
                value.clone()
            } else {
                json!(0)
            },
        );
    }
    Value::Object(result)
}
/// Saturation on floating overflow is fail-closed: it cannot reset spending to zero.
pub fn add_usage(left: &Value, right: &Value) -> Value {
    let mut left = normalize_usage(left);
    let right = normalize_usage(right);
    for field in BUDGET_USAGE_FIELDS {
        left[*field] = match (left[*field].as_u64(), right[*field].as_u64()) {
            (Some(a), Some(b)) if a.checked_add(b).is_some() => json!(a + b),
            _ => json!(
                (left[*field].as_f64().unwrap() + right[*field].as_f64().unwrap()).min(f64::MAX)
            ),
        };
    }
    left
}
/// Only saved result blobs count; invalid/non-object JSON and missing usage are zero.
pub fn total_saved_usage(results: impl IntoIterator<Item = Value>) -> Value {
    results
        .into_iter()
        .fold(normalize_usage(&Value::Null), |total, result| {
            let decoded = result
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok());
            add_usage(&total, &decoded.as_ref().unwrap_or(&result)["usage"])
        })
}
/// Stable diagnostic object with ratios only for configured fields.
pub fn budget_state(budget: &TaskBudget, used: &Value) -> Value {
    let limits = &budget.as_json()["limits"];
    let threshold = budget.warning_threshold();
    let normalized = normalize_usage(used);
    let mut used = serde_json::Map::new();
    let mut ratios = serde_json::Map::new();
    let mut exhausted = Vec::new();
    let mut warning = false;
    for field in BUDGET_USAGE_FIELDS {
        if limits.get(*field).is_none() {
            continue;
        }
        let amount = normalized[*field].as_f64().unwrap();
        let limit = limits[*field].as_f64().unwrap();
        let ratio = (amount / limit).min(f64::MAX);
        used.insert((*field).to_owned(), normalized[*field].clone());
        ratios.insert((*field).to_owned(), json!(ratio));
        if amount >= limit {
            exhausted.push(*field);
        }
        warning |= ratio >= threshold;
    }
    let (gate, reason) = if !exhausted.is_empty() {
        (
            "exhausted",
            Some(format!(
                "budget limit(s) already reached: {}",
                exhausted.join(", ")
            )),
        )
    } else if warning {
        (
            "warning",
            Some(format!("budget reached warning threshold {threshold}")),
        )
    } else {
        ("none", None)
    };
    json!({"limits":limits,"used":used,"ratios":ratios,"warning_threshold":budget.as_json()["warning_threshold"],"warning":warning,"exhausted":!exhausted.is_empty(),"exhausted_fields":exhausted,"gate":gate,"gate_reason":reason})
}

/// Override affects only the budget decision, never other revision gates.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetDecision {
    pub state: Option<Value>,
    pub error: Option<&'static str>,
    pub overridden: bool,
}
pub fn revision_budget_decision(
    budget: Result<Option<TaskBudget>, BudgetValidationError>,
    used: &Value,
    allow_override: bool,
) -> BudgetDecision {
    let (state, error) = match budget {
        Ok(None) => (None, None),
        Ok(Some(budget)) => {
            let state = budget_state(&budget, used);
            let error = (state["exhausted"] == true).then_some("budget_exhausted");
            (Some(state), error)
        }
        Err(_) => (
            Some(
                json!({"configured":true,"corrupt":true,"warning":false,"exhausted":false,"exhausted_fields":[],"gate":"corrupt","gate_reason":"saved budget state could not be validated"}),
            ),
            Some("budget_corrupt"),
        ),
    };
    BudgetDecision {
        state,
        error: if allow_override { None } else { error },
        overridden: allow_override && error.is_some(),
    }
}
impl StorageConnection {
    /// Reads budget and saved results in one snapshot. This service reserves no
    /// request id and changes no rows. A revision handler must evaluate it under
    /// its existing task/lifecycle fence immediately before creating a round.
    pub fn revision_budget_decision(
        &mut self,
        task: TaskId,
        project: &ProjectId,
        allow_override: bool,
    ) -> Result<BudgetDecision, RoundUpdateError> {
        let tx = self
            .connection_mut()
            .transaction()
            .map_err(RoundUpdateError::Database)?;
        let budget = tx
            .query_row(
                "SELECT budget_json FROM tasks WHERE task_id=?1 AND project_id=?2",
                params![task.to_string(), project.as_str()],
                |row| {
                    Ok(match row.get_ref(0)? {
                        rusqlite::types::ValueRef::Null => Ok(None),
                        rusqlite::types::ValueRef::Text(bytes) => std::str::from_utf8(bytes)
                            .map_err(|_| BudgetValidationError)
                            .and_then(|s| crate::normalize_persisted_budget(Some(s))),
                        _ => Err(BudgetValidationError),
                    })
                },
            )
            .optional()
            .map_err(RoundUpdateError::Database)?
            .ok_or(RoundUpdateError::MissingTask)?;
        let results = {
            let mut query = tx.prepare("SELECT result_json FROM rounds WHERE task_id=?1 AND project_id=?2 ORDER BY round_number").map_err(RoundUpdateError::Database)?;
            query
                .query_map(params![task.to_string(), project.as_str()], |row| {
                    Ok(row
                        .get::<_, Option<String>>(0)
                        .ok()
                        .flatten()
                        .map(Value::String)
                        .unwrap_or(Value::Null))
                })
                .map_err(RoundUpdateError::Database)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(RoundUpdateError::Database)?
        };
        let decision =
            revision_budget_decision(budget, &total_saved_usage(results), allow_override);
        tx.commit().map_err(RoundUpdateError::Database)?;
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_source_goldens() {
        let corpus: Value =
            serde_json::from_str(include_str!("../../../docs/fixtures/usage-goldens.json"))
                .unwrap();
        for case in corpus["cases"].as_array().unwrap() {
            let result = match case["kind"].as_str().unwrap() {
                "normalize" => normalize_usage(&case["input"]),
                "total" => total_saved_usage(case["input"].as_array().unwrap().clone()),
                "state" => budget_state(
                    &TaskBudget::from_json(&case["budget"]).unwrap(),
                    &case["used"],
                ),
                _ => unreachable!(),
            };
            assert_eq!(result, case["expect"]);
        }
    }
    #[test]
    fn saved_usage_and_numeric_shapes() {
        let results = vec![
            json!({"usage":{"input":10,"cost":0.25,"output":true}}),
            json!("{\"usage\":{\"input\":2,\"output\":3,\"cost\":0.5}}"),
            json!("broken"),
            json!([1]),
            json!({"usage":{"input":-1,"output":"100"}}),
        ];
        assert_eq!(
            total_saved_usage(results),
            json!({"input":12,"output":3,"reasoning":0,"cache_read":0,"cache_write":0,"cost":0.75})
        );
        assert_eq!(
            add_usage(&json!({"input":u64::MAX}), &json!({"input":0}))["input"],
            json!(u64::MAX)
        );
        assert_eq!(
            add_usage(&json!({"cost":f64::MAX}), &json!({"cost":f64::MAX}))["cost"],
            json!(f64::MAX)
        );
    }
    #[test]
    fn warning_exhaustion_order_and_override() {
        let budget =
            TaskBudget::from_json(&json!({"limits":{"cost":2,"input":100,"output":5}})).unwrap();
        assert_eq!(budget_state(&budget, &json!({"input":79}))["gate"], "none");
        assert_eq!(
            budget_state(&budget, &json!({"input":80}))["gate"],
            "warning"
        );
        let state = budget_state(&budget, &json!({"input":100,"cost":3}));
        assert_eq!(state["exhausted_fields"], json!(["input", "cost"]));
        assert_eq!(state["used"], json!({"input":100,"output":0,"cost":3}));
        assert_eq!(
            revision_budget_decision(Ok(Some(budget.clone())), &json!({"input":100}), false).error,
            Some("budget_exhausted")
        );
        assert!(revision_budget_decision(Ok(Some(budget)), &json!({"input":100}), true).overridden);
        assert_eq!(
            revision_budget_decision(Err(BudgetValidationError), &Value::Null, false).error,
            Some("budget_corrupt")
        );
        assert!(
            revision_budget_decision(Err(BudgetValidationError), &Value::Null, true).overridden
        );
        assert!(!revision_budget_decision(Ok(None), &Value::Null, true).overridden);
    }
}
