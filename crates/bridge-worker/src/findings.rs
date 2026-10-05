//! Strict revision findings at the request and persisted worker boundaries (7.13).
//! No MCP server, spawning, secret gate or budget gate is implemented here.
use std::{error::Error, fmt, path::Path};

use bridge_domain::StructuredFindings;
use bridge_path_policy::validate_allowed_paths_with_trusted_roots;
use bridge_storage::{
    CreateRevisionRoundInput, RevisionRoundOutcome, RoundRef, RoundUpdateError, StorageConnection,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Redacted validation category; neither JSON nor filesystem data is exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingsError {
    /// A revision always requires a nonblank textual summary.
    InvalidText,
    /// The structured array violates its schema or the task's authorized scope.
    InvalidStructured,
}
impl FindingsError {
    /// Request-time error code; persisted violations are mapped by dispatch to
    /// the terminal `structured_findings_invariant` category.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidText => "invalid_request",
            Self::InvalidStructured => "invalid_structured_findings",
        }
    }
}
impl fmt::Display for FindingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl Error for FindingsError {}

/// Immutable validated revision, including its Python-compatible request hash.
/// The caller supplies the owning task's workspace, scope and trusted roots.
#[derive(Clone, PartialEq)]
pub struct RevisionFindings {
    text: String,
    structured: Option<StructuredFindings>,
    payload_hash: String,
}
impl fmt::Debug for RevisionFindings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RevisionFindings { .. }")
    }
}
impl RevisionFindings {
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn structured(&self) -> Option<&StructuredFindings> {
        self.structured.as_ref()
    }
    pub fn payload_hash(&self) -> &str {
        &self.payload_hash
    }

    /// Persists one revision atomically; identical retries do not write again.
    /// Caller owns the worker lock and applies lifecycle/secret/budget gates.
    pub fn create_round(
        &self,
        storage: &mut StorageConnection,
        round: RoundRef,
        request_id: String,
    ) -> Result<RevisionRoundOutcome, RoundUpdateError> {
        storage.create_revision_round_with_findings(
            CreateRevisionRoundInput {
                task_id: round.task_id,
                project_id: round.project_id,
                round_number: round.round_number,
                request_id,
                payload_hash: self.payload_hash.clone(),
                findings: Some(self.text.clone()),
            },
            self.structured.clone(),
        )
    }

    /// One-round override, recorded atomically with the newly created round.
    pub fn create_round_with_budget_override(
        &self,
        storage: &mut StorageConnection,
        round: RoundRef,
        request_id: String,
    ) -> Result<RevisionRoundOutcome, RoundUpdateError> {
        storage.create_revision_round_with_budget_override(
            CreateRevisionRoundInput {
                task_id: round.task_id,
                project_id: round.project_id,
                round_number: round.round_number,
                request_id,
                payload_hash: self.payload_hash.clone(),
                findings: Some(self.text.clone()),
            },
            self.structured.clone(),
        )
    }

    /// Text-only and empty-array revisions keep the historical prompt verbatim.
    /// Nonempty structured findings append the reference's deterministic block.
    pub fn prompt_findings(&self) -> String {
        let Some(items) = self
            .structured
            .as_ref()
            .filter(|s| !s.as_slice().is_empty())
        else {
            return self.text.clone();
        };
        let mut lines = vec!["Структурированные замечания ревью:".to_owned()];
        for (index, item) in items.as_slice().iter().enumerate() {
            let location = match item.line {
                Some(line) => format!("{}:{line}", item.path),
                None => item.path.clone(),
            };
            let message = item
                .message
                .split(python_whitespace)
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(format!(
                "{}. [{}] {location} ({}) {message}",
                index + 1,
                item.severity.as_str(),
                item.code
            ));
        }
        format!(
            "{}\n\n{}",
            self.text.trim_matches(python_whitespace),
            lines.join("\n")
        )
    }
}

/// Request-time JSON null/omission means text-only. Every array item must be
/// valid; filesystem normalization and exact file/directory scope matching
/// reject traversal, symlink escapes and untrusted qualified external paths.
pub fn validate_revision_findings(
    text: &str,
    structured: Option<&Value>,
    workspace: &Path,
    trusted_roots: &[&Path],
    allowed_paths: &[String],
) -> Result<RevisionFindings, FindingsError> {
    if text.trim_matches(python_whitespace).is_empty() {
        return Err(FindingsError::InvalidText);
    }
    let structured = structured
        .filter(|v| !v.is_null())
        .map(|value| {
            let decoded: StructuredFindings = serde_json::from_value(value.clone())
                .map_err(|_| FindingsError::InvalidStructured)?;
            normalize_scope(decoded, workspace, trusted_roots, allowed_paths)
        })
        .transpose()?;
    let mut payload = json!({"kind":"revise", "findings":text});
    if let Some(items) = structured.as_ref().filter(|s| !s.as_slice().is_empty()) {
        payload["structured_findings"] =
            serde_json::to_value(items).map_err(|_| FindingsError::InvalidStructured)?;
    }
    let payload_hash = format!("{:x}", Sha256::digest(python_json(&payload).as_bytes()));
    Ok(RevisionFindings {
        text: text.to_owned(),
        structured,
        payload_hash,
    })
}

fn normalize_scope(
    findings: StructuredFindings,
    workspace: &Path,
    trusted_roots: &[&Path],
    allowed: &[String],
) -> Result<StructuredFindings, FindingsError> {
    let mut items = findings.as_slice().to_vec();
    for item in &mut items {
        if item.message.trim_matches(python_whitespace).is_empty() {
            return Err(FindingsError::InvalidStructured);
        }
        let paths =
            validate_allowed_paths_with_trusted_roots(workspace, trusted_roots, &[&item.path])
                .map_err(|_| FindingsError::InvalidStructured)?;
        item.path = paths[0].to_scope_string();
        if !allowed.iter().any(|scope| {
            if scope.ends_with('/') {
                item.path == scope.trim_end_matches('/') || item.path.starts_with(scope)
            } else {
                item.path == *scope
            }
        }) {
            return Err(FindingsError::InvalidStructured);
        }
    }
    StructuredFindings::try_from(items).map_err(|_| FindingsError::InvalidStructured)
}

fn python_whitespace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

// Python _hash uses sorted keys, UTF-8 and its default ", " / ": " separators.
// Operate on Values so optional line=null is omitted by the domain serializer.
fn python_json(value: &Value) -> String {
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_json).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(items) => {
            let mut keys: Vec<_> = items.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.iter()
                    .map(|key| format!("{}: {}", json!(key), python_json(&items[*key])))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        scalar => scalar.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, process::Command};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("bridge-findings-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn finding(path: &str) -> Value {
        json!({"severity":"error", "path":path, "line":2, "code":"BUG-1", "message":"ошибка\nоператора"})
    }

    #[test]
    fn canonical_hash_matches_independent_python_goldens() {
        let tmp = Temp::new();
        let allowed = vec!["src/".to_owned()];
        let text =
            validate_revision_findings("  исправить\n", None, &tmp.0, &[], &allowed).unwrap();
        assert_eq!(
            text.payload_hash(),
            "548021a31b546cce1a9c48a7fa831f6cc35db5df810c79553b5c7adb559b9f7a"
        );
        assert_eq!(text.prompt_findings(), "  исправить\n");
        for empty in [Value::Null, json!([])] {
            let parsed =
                validate_revision_findings(text.text(), Some(&empty), &tmp.0, &[], &allowed)
                    .unwrap();
            assert_eq!(text.payload_hash(), parsed.payload_hash());
            assert_eq!(text.prompt_findings(), parsed.prompt_findings());
        }
        let value = json!([finding("src/./a.rs")]);
        let parsed =
            validate_revision_findings("fix", Some(&value), &tmp.0, &[], &allowed).unwrap();
        assert_eq!(
            parsed.payload_hash(),
            "44a45fa237ab303c4eca482b8574532cbaeb379488fa1b072e045be7545dcb1c"
        );
        assert_eq!(
            parsed.prompt_findings(),
            "fix\n\nСтруктурированные замечания ревью:\n1. [error] src/a.rs:2 (BUG-1) ошибка оператора"
        );
        let normalized = json!([finding("src/a.rs")]);
        assert_eq!(
            parsed,
            validate_revision_findings("fix", Some(&normalized), &tmp.0, &[], &allowed).unwrap()
        );
        assert!(!format!("{parsed:?}").contains("ошибка"));
    }

    #[test]
    fn consumes_all_ten_frozen_mcp_structured_cases() {
        let tmp = Temp::new();
        let corpus: Value =
            serde_json::from_str(include_str!("../../../docs/fixtures/mcp-cases.json")).unwrap();
        let mut count = 0;
        for case in corpus["cases"].as_array().unwrap() {
            let id = case["id"].as_str().unwrap();
            if !id.contains("request-changes-structured-findings-") {
                continue;
            }
            let input = &case["input"];
            let mut value = input["structured_findings"].clone();
            if value == "${STRUCTURED_FINDINGS_201}" {
                value = Value::Array(vec![finding("module.py"); 201]);
            }
            let result = validate_revision_findings(
                input["findings"].as_str().unwrap(),
                Some(&value),
                &tmp.0,
                &[],
                &["module.py".to_owned()],
            );
            assert_eq!(result.is_ok(), case["expectation"] == "success", "{id}");
            if let Err(error) = result {
                assert_eq!(
                    error.as_str(),
                    case["error_category"].as_str().unwrap(),
                    "{id}"
                );
            }
            count += 1;
        }
        assert_eq!(count, 10);
    }

    #[test]
    fn strict_schema_scope_and_mandatory_text() {
        let tmp = Temp::new();
        let allowed = vec!["src/".to_owned()];
        for text in ["", "  \n\t", "\u{1c}\u{1f}"] {
            assert_eq!(
                validate_revision_findings(
                    text,
                    Some(&json!([finding("src/a")])),
                    &tmp.0,
                    &[],
                    &allowed
                )
                .unwrap_err(),
                FindingsError::InvalidText
            );
        }
        for value in [
            json!({}),
            json!(false),
            json!([42]),
            json!([finding("src2/a")]),
            json!([finding("../src/a")]),
            json!([finding("src\\a")]),
            json!([finding("/")]),
            json!([finding(".")]),
            json!([finding("src/a\u{0}")]),
        ] {
            assert_eq!(
                validate_revision_findings("text", Some(&value), &tmp.0, &[], &allowed)
                    .unwrap_err(),
                FindingsError::InvalidStructured
            );
        }
        for line in [
            json!(true),
            json!(0),
            json!(-1),
            json!(2.0),
            json!(1_000_001),
        ] {
            let mut item = finding("src/a");
            item["line"] = line;
            assert!(
                validate_revision_findings("text", Some(&json!([item])), &tmp.0, &[], &allowed)
                    .is_err()
            );
        }
        for path in ["src", "src/", "src/nested/new.rs"] {
            assert!(
                validate_revision_findings(
                    "text",
                    Some(&json!([finding(path)])),
                    &tmp.0,
                    &[],
                    &allowed
                )
                .is_ok()
            );
        }
        assert!(
            validate_revision_findings(
                "text",
                Some(&json!([finding("src/a/child")])),
                &tmp.0,
                &[],
                &["src/a".to_owned()]
            )
            .is_err()
        );
        let mut no_line = finding("src/a");
        no_line["line"] = Value::Null;
        let parsed =
            validate_revision_findings("text", Some(&json!([no_line])), &tmp.0, &[], &allowed)
                .unwrap();
        assert!(
            !serde_json::to_value(parsed.structured()).unwrap()[0]
                .as_object()
                .unwrap()
                .contains_key("line")
        );
    }

    #[test]
    fn trusted_external_repository_never_widens_task_scope() {
        let tmp = Temp::new();
        let workspace = tmp.dir("workspace");
        let external = tmp.dir("trusted/repo");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&external)
                .status()
                .unwrap()
                .success()
        );
        let root = tmp.0.join("trusted");
        let file = external.join("file.rs").to_str().unwrap().to_owned();
        let value = json!([finding(&file)]);
        let allowed = vec![format!("{}/", external.display())];
        assert!(
            validate_revision_findings("text", Some(&value), &workspace, &[&root], &allowed)
                .is_ok()
        );
        assert!(
            validate_revision_findings("text", Some(&value), &workspace, &[], &allowed).is_err()
        );
        assert!(
            validate_revision_findings(
                "text",
                Some(&value),
                &workspace,
                &[&root],
                &["src/".to_owned()]
            )
            .is_err()
        );
        let nested = external.join("nested");
        std::fs::create_dir(&nested).unwrap();
        assert!(
            validate_revision_findings("text", Some(&value), &workspace, &[&nested], &allowed)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_and_dangling_symlink_escapes_fail_closed() {
        let tmp = Temp::new();
        let workspace = tmp.dir("workspace");
        let outside = tmp.dir("outside");
        std::os::unix::fs::symlink(&outside, workspace.join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.join("missing"), workspace.join("dangling")).unwrap();
        for path in ["escape/file", "dangling/file"] {
            assert!(
                validate_revision_findings(
                    "text",
                    Some(&json!([finding(path)])),
                    &workspace,
                    &[],
                    &["escape/".to_owned(), "dangling/".to_owned()]
                )
                .is_err()
            );
        }
    }
}
