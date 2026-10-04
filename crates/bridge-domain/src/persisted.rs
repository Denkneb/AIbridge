//! Pure v15-reference contracts for fields introduced in schema v7–v13.
//!
//! Decoding validates the entire value. Path authorization, secret scanning,
//! dependency graph checks, persistence and execution belong to consumers.
//! Public fields may be assembled by callers; call `validate` before trusting
//! an assembled or modified model. Sensitive models have redacted Debug output.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Number;
use sha2::{Digest, Sha256};

use crate::{DomainError, ProjectId, Result};

// Keep the wire shape next to the public model, while making validation part
// of every Deserialize path (including from_value and nested collections).
macro_rules! validated_model {
    ($(#[$meta:meta])* $name:ident { $( $(#[$attr:meta])* $field:ident: $ty:ty ),* $(,)? }) => {
        #[derive(Clone, PartialEq, Serialize)]
        $(#[$meta])*
        pub struct $name {
            $( $(#[$attr])* pub $field: $ty, )*
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                #[derive(Deserialize)]
                $(#[$meta])*
                struct Wire { $( $(#[$attr])* $field: $ty, )* }
                let wire = Wire::deserialize(deserializer)
                    .map_err(|_| D::Error::custom("invalid persisted contract"))?;
                let mut model = Self { $( $field: wire.$field, )* };
                model.normalize().map_err(D::Error::custom)?;
                Ok(model)
            }
        }
        impl $name {
            /// Validates an assembled or modified model without side effects.
            pub fn validate(&self) -> Result<()> {
                self.clone().normalize()
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        }
    };
}

macro_rules! vocabulary {
    ($(#[$meta:meta])* $name:ident { $( $(#[$variant_meta:meta])* $variant:ident => $text:literal ),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub enum $name { $( $(#[$variant_meta])* $variant, )* }
        impl $name {
            /// Exact contract spelling.
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text,)* }
            }
        }
        impl TryFrom<String> for $name {
            type Error = DomainError;
            fn try_from(value: String) -> Result<Self> {
                match value.as_str() {
                    $($text => Ok(Self::$variant),)*
                    _ => Err(invalid()),
                }
            }
        }
        impl From<$name> for String {
            fn from(value: $name) -> String { value.as_str().to_owned() }
        }
    };
}

fn invalid() -> DomainError {
    DomainError::invalid_input("invalid persisted contract")
}

fn check(condition: bool) -> Result<()> {
    if condition { Ok(()) } else { Err(invalid()) }
}

fn safe_token(value: &str, max: usize, first: fn(u8) -> bool, rest: fn(u8) -> bool) -> bool {
    !value.is_empty()
        && value.len() <= max
        && first(value.as_bytes()[0])
        && value.bytes().skip(1).all(rest)
}

fn project_token(value: &str) -> bool {
    safe_token(
        value,
        64,
        |b| b.is_ascii_lowercase() || b.is_ascii_digit(),
        |b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b),
    )
}

fn workflow_token(value: &str) -> bool {
    safe_token(
        value,
        128,
        |b| b.is_ascii_alphanumeric(),
        |b| b.is_ascii_alphanumeric() || b"._-".contains(&b),
    )
}

fn profile_token(value: &str) -> bool {
    safe_token(
        value,
        64,
        |b| b.is_ascii_lowercase(),
        |b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b),
    )
}

macro_rules! string_contract {
    ($(#[$meta:meta])* $name:ident, $normalize:expr) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl $name {
            /// Validated string representation.
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl TryFrom<String> for $name {
            type Error = DomainError;
            fn try_from(value: String) -> Result<Self> { ($normalize)(value).map(Self) }
        }
        impl From<$name> for String {
            fn from(value: $name) -> String { value.0 }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        }
    };
}

vocabulary! {
    /// Execution root selection; absent config values default to direct.
    #[derive(Default)]
    ExecutionMode { #[default] Direct => "direct", Worktree => "worktree" }
}

vocabulary! {
    /// Severity of a structured review finding.
    FindingSeverity { Info => "info", Warning => "warning", Error => "error", Critical => "critical" }
}

validated_model! {
    /// One persisted finding. Scope and symlink checks require a consumer's workspace.
    #[serde(deny_unknown_fields)]
    StructuredFinding {
        severity: FindingSeverity,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        code: String,
        message: String,
    }
}
impl StructuredFinding {
    fn normalize(&mut self) -> Result<()> {
        // Absolute qualified external paths are valid here. Only consumers can
        // establish authorization; this layer checks shape, not filesystem state.
        check(
            !self.path.is_empty() && self.path.chars().count() <= 4096 && !self.path.contains('\0'),
        )?;
        check(
            !self.path.contains('\\')
                && !self.path.split('/').any(|part| part == "..")
                && !matches!(self.path.as_str(), "." | "/"),
        )?;
        check(self.line.is_none_or(|line| (1..=1_000_000).contains(&line)))?;
        check(safe_token(
            &self.code,
            64,
            |b| b.is_ascii_alphanumeric(),
            |b| b.is_ascii_alphanumeric() || b"._+-".contains(&b),
        ))?;
        check(!self.message.trim().is_empty() && self.message.chars().count() <= 4000)
    }
}

/// Entire structured-findings column, bounded to 200 entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<StructuredFinding>", into = "Vec<StructuredFinding>")]
pub struct StructuredFindings(Vec<StructuredFinding>);
impl StructuredFindings {
    /// Findings in persisted order.
    pub fn as_slice(&self) -> &[StructuredFinding] {
        &self.0
    }
}
impl TryFrom<Vec<StructuredFinding>> for StructuredFindings {
    type Error = DomainError;
    fn try_from(value: Vec<StructuredFinding>) -> Result<Self> {
        check(value.len() <= 200)?;
        for finding in &value {
            finding.validate()?;
        }
        Ok(Self(value))
    }
}
impl From<StructuredFindings> for Vec<StructuredFinding> {
    fn from(value: StructuredFindings) -> Self {
        value.0
    }
}

vocabulary! {
    /// Supported token/cost budget counters.
    UsageField { Input => "input", Output => "output", Reasoning => "reasoning",
        CacheRead => "cache_read", CacheWrite => "cache_write", Cost => "cost" }
}

/// Positive finite JSON number. Retains integer versus floating-point encoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Number", into = "Number")]
pub struct PositiveNumber(Number);
impl PositiveNumber {
    /// Original JSON number, preserving integer precision in its encoding.
    pub fn as_number(&self) -> &Number {
        &self.0
    }
    /// Numeric value for comparisons; construction proves this is finite.
    pub fn as_f64(&self) -> f64 {
        self.0.as_f64().expect("validated number")
    }
}
impl TryFrom<Number> for PositiveNumber {
    type Error = DomainError;
    fn try_from(value: Number) -> Result<Self> {
        check(value.as_f64().is_some_and(|n| n.is_finite() && n > 0.0))?;
        Ok(Self(value))
    }
}
impl From<PositiveNumber> for Number {
    fn from(value: PositiveNumber) -> Number {
        value.0
    }
}
fn warning_threshold() -> PositiveNumber {
    PositiveNumber(Number::from_f64(0.8).expect("finite constant"))
}
validated_model! {
    /// Optional persisted budget. A missing column is represented by Option<Budget>.
    #[serde(deny_unknown_fields)]
    Budget {
        limits: BTreeMap<UsageField, PositiveNumber>,
        #[serde(default = "warning_threshold")]
        warning_threshold: PositiveNumber,
    }
}
impl Budget {
    fn normalize(&mut self) -> Result<()> {
        check(!self.limits.is_empty() && self.warning_threshold.as_f64() <= 1.0)
    }
}

string_contract! {
    /// Workflow identifier using the v15 safe-token contract.
    WorkflowId, |value: String| { check(workflow_token(&value))?; Ok(value) }
}
string_contract! {
    /// Dependency task reference. The Python contract permits safe non-UUID IDs.
    DependencyTaskId, |value: String| { check(workflow_token(&value))?; Ok(value) }
}
validated_model! {
    /// One strict dependency edge; no linked-project/graph lookup occurs here.
    #[serde(deny_unknown_fields)]
    DependencyReference { project_id: ProjectId, task_id: DependencyTaskId }
}
impl DependencyReference {
    fn normalize(&mut self) -> Result<()> {
        check(project_token(self.project_id.as_str()))
    }
}
validated_model! {
    /// Persisted workflow metadata. Defaults represent a task without a workflow.
    #[derive(Default)]
    #[serde(deny_unknown_fields)]
    WorkflowMetadata {
        #[serde(default)]
        workflow_id: Option<WorkflowId>,
        #[serde(default)]
        depends_on: Vec<DependencyReference>,
    }
}
impl WorkflowMetadata {
    fn normalize(&mut self) -> Result<()> {
        let mut seen = HashSet::new();
        for edge in &self.depends_on {
            edge.validate()?;
            check(seen.insert((edge.project_id.as_str(), edge.task_id.as_str())))?;
        }
        Ok(())
    }
}

vocabulary! {
    /// Origin of the profile definition.
    ProfileDefinitionSource { Builtin => "builtin", Config => "config" }
}
vocabulary! {
    /// Why this profile was selected; persisted tasks.profile_source uses this vocabulary.
    ProfileOrigin { Argument => "argument", ProjectDefault => "project_default", BuiltinDefault => "builtin_default" }
}
vocabulary! {
    /// Source of the effective model pinned in the snapshot.
    ProfileModelSource { Profile => "profile", Project => "project" }
}
fn required_nullable<'de, D: Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}
fn valid_model(model: &str) -> bool {
    model.split_once('/').is_some_and(|(provider, model)| {
        !provider.is_empty()
            && !model.is_empty()
            && provider.trim() == provider
            && model.trim() == model
    })
}
fn profile_text(value: &str, max: usize) -> bool {
    value.chars().count() <= max
        && !value
            .chars()
            .any(|c| matches!(c as u32, 0..=8 | 11..=12 | 14..=31 | 127))
}
fn hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Python `_hash`: sorted UTF-8 JSON with default comma/colon separators.
/// Snapshot hashing remains compact and uses its separate canonical contract.
pub fn request_payload_hash(value: &serde_json::Value) -> String {
    fn render(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Array(items) => format!(
                "[{}]",
                items.iter().map(render).collect::<Vec<_>>().join(", ")
            ),
            serde_json::Value::Object(items) => {
                let mut keys: Vec<_> = items.keys().collect();
                keys.sort();
                format!(
                    "{{{}}}",
                    keys.iter()
                        .map(|key| format!("{}: {}", serde_json::json!(key), render(&items[*key])))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            scalar => scalar.to_string(),
        }
    }
    sha256(&render(value))
}
validated_model! {
    /// Exact nine-key submit-time profile_json payload.
    /// definition_hash pins the definition; canonical_hash pins this effective snapshot.
    #[serde(deny_unknown_fields)]
    ProfileSnapshot {
        id: String,
        source: ProfileDefinitionSource,
        origin: ProfileOrigin,
        purpose: String,
        instructions: String,
        #[serde(deserialize_with = "required_nullable")]
        model: Option<String>,
        model_source: ProfileModelSource,
        definition_version: String,
        definition_hash: String,
    }
}
impl ProfileSnapshot {
    fn normalize(&mut self) -> Result<()> {
        check(profile_token(&self.id))?;
        check(profile_text(&self.purpose, 200) && profile_text(&self.instructions, 8000))?;
        check(self.model.as_deref().is_none_or(valid_model))?;
        check(!self.definition_version.is_empty() && hex64(&self.definition_hash))
    }
    /// Python-compatible sorted, compact, UTF-8 canonical snapshot JSON.
    pub fn canonical_json(&self) -> Result<String> {
        self.validate()?;
        let object = serde_json::to_value(self).map_err(|_| invalid())?;
        // Explicit sorting makes the contract independent of serde_json features.
        let sorted: BTreeMap<_, _> = object.as_object().ok_or_else(invalid)?.iter().collect();
        serde_json::to_string(&sorted).map_err(|_| invalid())
    }
    /// SHA-256 of canonical_snapshot, matching tasks.profile_hash.
    pub fn canonical_hash(&self) -> Result<String> {
        Ok(sha256(&self.canonical_json()?))
    }
    /// Proves snapshot identity/hash/selection origin match the persisted task row.
    /// The row's profile_source is the selection origin, not the definition source.
    pub fn validate_identity(
        &self,
        profile: &str,
        profile_hash: &str,
        profile_source: ProfileOrigin,
    ) -> Result<()> {
        check(self.id == profile && self.origin == profile_source && hex64(profile_hash))?;
        check(self.canonical_hash()? == profile_hash)
    }
}

/// Computes Python's definition_hash for the four canonical definition fields.
/// Unlike the snapshot hash, Python's definition JSON uses spaces after separators.
/// The model here is the definition's own model, not an inherited project model.
pub fn profile_definition_hash(
    id: &str,
    purpose: &str,
    instructions: &str,
    model: Option<&str>,
) -> Result<String> {
    check(profile_token(id) && profile_text(purpose, 200) && profile_text(instructions, 8000))?;
    check(model.is_none_or(valid_model))?;
    let id = serde_json::to_string(id).map_err(|_| invalid())?;
    let purpose = serde_json::to_string(purpose).map_err(|_| invalid())?;
    let instructions = serde_json::to_string(instructions).map_err(|_| invalid())?;
    let model = serde_json::to_string(&model).map_err(|_| invalid())?;
    Ok(sha256(&format!(
        "{{\"id\": {id}, \"instructions\": {instructions}, \"model\": {model}, \"purpose\": {purpose}}}"
    )))
}

string_contract! {
    /// Bounded, normalized hexadecimal checkpoint ref/digest (1..=128 characters).
    CheckpointRef, |value: String| {
        check(!value.is_empty() && value.len() <= 128 && value.bytes().all(|b| b.is_ascii_hexdigit()))?;
        Ok(value.to_ascii_lowercase())
    }
}
string_contract! {
    /// Repository-relative checkpoint path. It never contains an external root.
    CheckpointPath, |value: String| {
        check(!value.is_empty() && value.chars().count() <= 300 && !value.starts_with('/')
            && !value.contains(['\\', '\0']) && value.split('/').all(|p| !matches!(p, "" | "." | "..")))?;
        Ok(value)
    }
}
vocabulary! {
    /// Change classification in checkpoint diff statistics.
    CheckpointChange { Added => "added", Modified => "modified", Deleted => "deleted", Renamed => "renamed" }
}
vocabulary! {
    /// File classification. Unknown preserves the absence of baseline kind evidence.
    CheckpointFileKind { File => "file", Binary => "binary", Symlink => "symlink", Unknown => "unknown" }
}
validated_model! {
    /// Bounded diff-stat counters; fields are required, even when zero.
    CheckpointCounts { added: u32, modified: u32, deleted: u32, renamed: u32 }
}
impl CheckpointCounts {
    fn normalize(&mut self) -> Result<()> {
        check(
            [self.added, self.modified, self.deleted, self.renamed]
                .into_iter()
                .all(|n| n <= 1_000_000),
        )
    }
}
validated_model! {
    /// One repository-relative diff entry. Renames require a source path.
    CheckpointDiffEntry {
        path: CheckpointPath,
        change: CheckpointChange,
        kind: CheckpointFileKind,
        #[serde(default, rename = "from", skip_serializing_if = "Option::is_none")]
        from_path: Option<CheckpointPath>,
    }
}
impl CheckpointDiffEntry {
    fn normalize(&mut self) -> Result<()> {
        if self.change == CheckpointChange::Renamed {
            check(self.from_path.is_some())?;
        } else {
            self.from_path = None;
        }
        Ok(())
    }
}
validated_model! {
    /// Bounded diff summary; counters need not equal the truncated detail list.
    CheckpointDiffStat { counts: CheckpointCounts, entries: Vec<CheckpointDiffEntry>, truncated: u32 }
}
impl CheckpointDiffStat {
    fn normalize(&mut self) -> Result<()> {
        self.counts.validate()?;
        check(self.entries.len() <= 200 && self.truncated <= 1_000_000)?;
        for entry in &self.entries {
            entry.validate()?;
        }
        Ok(())
    }
}
validated_model! {
    /// Content-free state of one changed file, reused for the next round's delta.
    CheckpointFileState { digest: CheckpointRef, kind: CheckpointFileKind }
}
impl CheckpointFileState {
    fn normalize(&mut self) -> Result<()> {
        Ok(())
    }
}
validated_model! {
    /// One repository checkpoint. Available repositories require complete state.
    CheckpointRepository {
        label: String,
        available: bool,
        diff_stat: CheckpointDiffStat,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head: Option<CheckpointRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index_fingerprint: Option<CheckpointRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree_fingerprint: Option<CheckpointRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changed: Option<BTreeMap<CheckpointPath, CheckpointFileState>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        absent: Option<Vec<CheckpointPath>>,
    }
}
impl CheckpointRepository {
    fn normalize(&mut self) -> Result<()> {
        let valid_label = self.label == "workspace"
            || self.label.strip_prefix("external ").is_some_and(|n| {
                !n.is_empty()
                    && n.len() <= 3
                    && n.bytes().all(|b| b.is_ascii_digit())
                    && n.parse::<u32>().is_ok_and(|n| (1..=64).contains(&n))
            });
        check(valid_label)?;
        self.diff_stat.validate()?;
        if self.available {
            check(
                self.head.is_some()
                    && self.index_fingerprint.is_some()
                    && self.worktree_fingerprint.is_some(),
            )?;
            let changed = self.changed.as_ref().ok_or_else(invalid)?;
            let absent = self.absent.as_ref().ok_or_else(invalid)?;
            check(changed.len() + absent.len() <= 2000)?;
            for state in changed.values() {
                state.validate()?;
            }
        } else {
            self.head = None;
            self.index_fingerprint = None;
            self.worktree_fingerprint = None;
            self.changed = None;
            self.absent = None;
        }
        Ok(())
    }
}
validated_model! {
    /// Version-1 multi-repository checkpoint, with mirrored workspace fingerprints.
    /// Unknown fields are ignored as in the reference reader; required state is strict.
    RoundCheckpoint {
        version: u32,
        #[serde(default)]
        head: Option<CheckpointRef>,
        #[serde(default)]
        index_fingerprint: Option<CheckpointRef>,
        #[serde(default)]
        worktree_fingerprint: Option<CheckpointRef>,
        repositories: Vec<CheckpointRepository>,
    }
}
impl RoundCheckpoint {
    fn normalize(&mut self) -> Result<()> {
        check(self.version == 1 && (1..=64).contains(&self.repositories.len()))?;
        for (index, repo) in self.repositories.iter().enumerate() {
            let expected = if index == 0 {
                "workspace".to_owned()
            } else {
                format!("external {index}")
            };
            check(repo.label == expected)?;
            repo.validate()?;
        }
        let workspace = &self.repositories[0];
        if workspace.available {
            check(
                self.head == workspace.head
                    && self.index_fingerprint == workspace.index_fingerprint
                    && self.worktree_fingerprint == workspace.worktree_fingerprint,
            )
        } else {
            check(
                self.head.is_none()
                    && self.index_fingerprint.is_none()
                    && self.worktree_fingerprint.is_none(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    // Computed from read-only Python HEAD 86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a.
    const PROFILE_GOLDENS: &str = r###"[
  {
    "snapshot": {
      "id": "implementer",
      "source": "builtin",
      "origin": "builtin_default",
      "purpose": "Обычная реализация согласованного плана в allowed_paths.",
      "instructions": "",
      "model": null,
      "model_source": "project",
      "definition_version": "1",
      "definition_hash": "c3a63d5c7a5a08a107a1b75ed0dff5be3b482f96d742ca17090fc1eff221cfc6"
    },
    "canonical": "{\"definition_hash\":\"c3a63d5c7a5a08a107a1b75ed0dff5be3b482f96d742ca17090fc1eff221cfc6\",\"definition_version\":\"1\",\"id\":\"implementer\",\"instructions\":\"\",\"model\":null,\"model_source\":\"project\",\"origin\":\"builtin_default\",\"purpose\":\"Обычная реализация согласованного плана в allowed_paths.\",\"source\":\"builtin\"}",
    "hash": "0aacabb9dbf088ffbf44ad058bc511be0c93302abbda5f1e99ef01ba87ff0423",
    "definition_model": null
  },
  {
    "snapshot": {
      "id": "test-writer",
      "source": "builtin",
      "origin": "argument",
      "purpose": "Тесты и тестовые фикстуры без изменения production-кода.",
      "instructions": "Профиль test-writer (дополнительные акценты; не расширяет scope и разрешения bridge):\n- не меняй production-код вне явно разрешённых тестовых путей;\n- пиши детерминированные, изолированные тесты без зависимости от порядка запуска и внешней сети;\n- запускай ровно согласованные test_commands и сохраняй реальные результаты;\n- не подгоняй тесты под неверное поведение без явного указания в задаче.",
      "model": "local/pinned-model",
      "model_source": "project",
      "definition_version": "1",
      "definition_hash": "adb167704ad838bea3b7d7408097055761a3c3126269b115e105a62fc294b30c"
    },
    "canonical": "{\"definition_hash\":\"adb167704ad838bea3b7d7408097055761a3c3126269b115e105a62fc294b30c\",\"definition_version\":\"1\",\"id\":\"test-writer\",\"instructions\":\"Профиль test-writer (дополнительные акценты; не расширяет scope и разрешения bridge):\\n- не меняй production-код вне явно разрешённых тестовых путей;\\n- пиши детерминированные, изолированные тесты без зависимости от порядка запуска и внешней сети;\\n- запускай ровно согласованные test_commands и сохраняй реальные результаты;\\n- не подгоняй тесты под неверное поведение без явного указания в задаче.\",\"model\":\"local/pinned-model\",\"model_source\":\"project\",\"origin\":\"argument\",\"purpose\":\"Тесты и тестовые фикстуры без изменения production-кода.\",\"source\":\"builtin\"}",
    "hash": "47607af521a8debf7e93cfb7577d66b5a583cbc8cdb4455d69d64451a664eebc",
    "definition_model": null
  },
  {
    "snapshot": {
      "id": "custom",
      "source": "config",
      "origin": "project_default",
      "purpose": "Разбор 🦀",
      "instructions": "Строка\n\t\"quoted\" \\ end",
      "model": "vendor/model/v2",
      "model_source": "profile",
      "definition_version": "1",
      "definition_hash": "661d49ab483bb9b2227fc33889dac83883502addb1d2529c562cec6142fe79eb"
    },
    "canonical": "{\"definition_hash\":\"661d49ab483bb9b2227fc33889dac83883502addb1d2529c562cec6142fe79eb\",\"definition_version\":\"1\",\"id\":\"custom\",\"instructions\":\"Строка\\n\\t\\\"quoted\\\" \\\\ end\",\"model\":\"vendor/model/v2\",\"model_source\":\"profile\",\"origin\":\"project_default\",\"purpose\":\"Разбор 🦀\",\"source\":\"config\"}",
    "hash": "3aa9b9bf573ec7c2daa80a27b52c37dbe7818b36bd440b25fd095b786077fff5",
    "definition_model": "vendor/model/v2"
  }
]"###;

    fn round_trip<T>(payload: Value) -> T
    where
        T: for<'de> Deserialize<'de> + Serialize,
    {
        let parsed: T = serde_json::from_value(payload.clone()).expect("valid reference shape");
        assert_eq!(serde_json::to_value(&parsed).expect("encode"), payload);
        parsed
    }

    fn rejects<T: for<'de> Deserialize<'de>>(payload: Value) {
        assert!(serde_json::from_value::<T>(payload).is_err());
    }

    fn finding() -> Value {
        json!({"severity":"warning", "path":"src/module.rs", "line":12,
            "code":"review.scope-1+note", "message":"Проверить границу 🦀"})
    }

    fn profile() -> Value {
        serde_json::from_str::<Value>(PROFILE_GOLDENS).unwrap()[2]["snapshot"].clone()
    }

    fn checkpoint() -> Value {
        // Same shape as tests/test_storage.py:_valid_checkpoint on the pinned HEAD.
        json!({
            "version":1, "head":"abc123", "index_fingerprint":"ab12", "worktree_fingerprint":"cd34",
            "repositories":[{
                "label":"workspace", "available":true, "head":"abc123",
                "index_fingerprint":"ab12", "worktree_fingerprint":"cd34", "changed":{}, "absent":[],
                "diff_stat":{"counts":{"added":1,"modified":0,"deleted":0,"renamed":0},
                    "entries":[{"path":"module.py","change":"added","kind":"file"}],"truncated":0}
            }]
        })
    }

    #[test]
    fn finding_round_trips_each_severity_and_optional_line() {
        for severity in ["info", "warning", "error", "critical"] {
            let mut value = finding();
            value["severity"] = json!(severity);
            round_trip::<StructuredFinding>(value.clone());
            value.as_object_mut().unwrap().remove("line");
            round_trip::<StructuredFinding>(value.clone());
            value["line"] = Value::Null;
            let parsed: StructuredFinding = serde_json::from_value(value).unwrap();
            assert!(serde_json::to_value(parsed).unwrap().get("line").is_none());
        }
        let mut external = finding();
        external["path"] = json!("/trusted/repo/module.rs");
        round_trip::<StructuredFinding>(external);
    }

    #[test]
    fn finding_rejects_corruption_and_bounds_in_characters() {
        for (key, bad) in [
            ("severity", json!("fatal")),
            ("severity", json!(false)),
            ("line", json!(0)),
            ("line", json!(-1)),
            ("line", json!(1_000_001)),
            ("line", json!(true)),
            ("line", json!(1.5)),
            ("line", json!("12")),
            ("code", json!("")),
            ("code", json!("bad code")),
            ("code", json!("_bad")),
            ("code", json!("a".repeat(65))),
            ("message", json!(" \n\t")),
            ("message", json!("я".repeat(4001))),
            ("path", json!("")),
            ("path", json!("a\u{0}b")),
            ("path", json!("../secret")),
            ("path", json!("a\\b")),
            ("path", json!("x".repeat(4097))),
        ] {
            let mut value = finding();
            value[key] = bad;
            rejects::<StructuredFinding>(value);
        }
        let mut value = finding();
        value["message"] = json!("я".repeat(4000));
        value["line"] = json!(1_000_000);
        round_trip::<StructuredFinding>(value);
        for key in ["severity", "path", "code", "message"] {
            let mut value = finding();
            value.as_object_mut().unwrap().remove(key);
            rejects::<StructuredFinding>(value);
        }
        let mut value = finding();
        value["unexpected"] = json!(1);
        rejects::<StructuredFinding>(value);
    }

    #[test]
    fn findings_column_is_a_bounded_array_with_no_partial_salvage() {
        round_trip::<StructuredFindings>(json!([]));
        round_trip::<StructuredFindings>(Value::Array(vec![finding(); 200]));
        rejects::<StructuredFindings>(Value::Array(vec![finding(); 201]));
        rejects::<StructuredFindings>(json!([finding(), {"severity":"warning"}]));
        rejects::<StructuredFindings>(json!({}));
        round_trip::<Option<StructuredFindings>>(Value::Null);
    }

    #[test]
    fn budget_retains_fractional_limits_integer_precision_and_default_threshold() {
        let payload = json!({"limits":{"input":9007199254740993u64, "output":2,
            "reasoning":0.5, "cache_read":1.5, "cache_write":1,"cost":0.125},"warning_threshold":0.8});
        let budget = round_trip::<Budget>(payload.clone());
        assert_eq!(
            budget.limits[&UsageField::Input].as_number().as_u64(),
            Some(9007199254740993)
        );
        let mut omitted = payload;
        omitted.as_object_mut().unwrap().remove("warning_threshold");
        let budget: Budget = serde_json::from_value(omitted).unwrap();
        assert_eq!(budget.warning_threshold.as_f64(), 0.8);
        round_trip::<Budget>(json!({"limits":{"input":1},"warning_threshold":1}));
        round_trip::<Option<Budget>>(Value::Null);
    }

    #[test]
    fn budget_rejects_unknown_empty_nonpositive_and_non_numeric_limits() {
        for limits in [
            json!({}),
            Value::Null,
            json!([]),
            json!({"other":1}),
            json!({"input":0}),
            json!({"input":-1}),
            json!({"input":true}),
            json!({"cost":"1.5"}),
            json!({"input":null}),
            json!({"input":1,"cost":-0.5}),
        ] {
            rejects::<Budget>(json!({"limits":limits}));
        }
        for threshold in [
            json!(0),
            json!(-0.1),
            json!(1.0001),
            json!(true),
            json!("0.8"),
            Value::Null,
        ] {
            rejects::<Budget>(json!({"limits":{"input":1},"warning_threshold":threshold}));
        }
        rejects::<Budget>(json!({"warning_threshold":0.8}));
        rejects::<Budget>(json!({"limits":{"input":1},"unknown":true}));
        for raw in [
            r#"{"limits":{"cost":1e999}}"#,
            r#"{"limits":{"cost":NaN}}"#,
            r#"{"limits":{"cost":Infinity}}"#,
        ] {
            assert!(serde_json::from_str::<Budget>(raw).is_err());
        }
    }

    #[test]
    fn execution_mode_is_strict_and_defaults_to_direct() {
        assert_eq!(ExecutionMode::default(), ExecutionMode::Direct);
        for mode in ["direct", "worktree"] {
            round_trip::<ExecutionMode>(json!(mode));
        }
        for value in [
            json!(" direct"),
            json!("worktree "),
            json!("Direct"),
            json!(""),
            json!(true),
            Value::Null,
        ] {
            rejects::<ExecutionMode>(value);
        }
    }

    #[test]
    fn workflow_round_trips_safe_non_uuid_dependency_ids_and_legacy_defaults() {
        round_trip::<WorkflowMetadata>(json!({"workflow_id":"Flow.1-x", "depends_on":[
            {"project_id":"other-project", "task_id":"task-legacy.1"},
            {"project_id":"proj_2", "task_id":"1234"}]}));
        let parsed: WorkflowMetadata = serde_json::from_value(json!({})).unwrap();
        assert_eq!(parsed, WorkflowMetadata::default());
        round_trip::<WorkflowMetadata>(json!({"workflow_id":null,"depends_on":[]}));
        round_trip::<WorkflowId>(json!("x".repeat(128)));
        assert!("task-legacy.1".parse::<crate::TaskId>().is_err());
    }

    #[test]
    fn workflow_rejects_corrupt_or_duplicate_edges_without_dropping_them() {
        for bad in ["", "_bad", "a/b", "a b", "a\n", "🦀"] {
            rejects::<WorkflowId>(json!(bad));
            rejects::<DependencyTaskId>(json!(bad));
        }
        rejects::<WorkflowId>(json!("x".repeat(129)));
        for project in ["", "UPPER", "_bad", "a/b", "x\n"] {
            rejects::<DependencyReference>(json!({"project_id":project,"task_id":"task-1"}));
        }
        for edge in [
            json!({"project_id":"proj"}),
            json!({"project_id":"proj","task_id":true}),
            json!({"project_id":"proj","task_id":"task-1","extra":1}),
            Value::Null,
        ] {
            rejects::<WorkflowMetadata>(json!({"workflow_id":"wf","depends_on":[edge]}));
        }
        let edge = json!({"project_id":"proj","task_id":"task-1"});
        rejects::<WorkflowMetadata>(json!({"workflow_id":"wf","depends_on":[edge.clone(),edge]}));
        rejects::<WorkflowMetadata>(json!({"workflow_id":"wf","depends_on":null}));
    }

    #[test]
    fn profile_canonical_json_and_both_hashes_match_python_goldens() {
        let cases: Vec<Value> = serde_json::from_str(PROFILE_GOLDENS).unwrap();
        for case in cases {
            let snapshot = round_trip::<ProfileSnapshot>(case["snapshot"].clone());
            assert_eq!(
                snapshot.canonical_json().unwrap(),
                case["canonical"].as_str().unwrap()
            );
            assert_eq!(
                snapshot.canonical_hash().unwrap(),
                case["hash"].as_str().unwrap()
            );
            assert_eq!(
                profile_definition_hash(
                    &snapshot.id,
                    &snapshot.purpose,
                    &snapshot.instructions,
                    case["definition_model"].as_str()
                )
                .unwrap(),
                snapshot.definition_hash
            );
            snapshot
                .validate_identity(
                    &snapshot.id,
                    case["hash"].as_str().unwrap(),
                    snapshot.origin,
                )
                .unwrap();
            // Reverse input key order to independently prove canonicalization.
            let reversed = case["snapshot"]
                .as_object()
                .unwrap()
                .iter()
                .rev()
                .map(|(key, value)| format!("{}:{}", serde_json::to_string(key).unwrap(), value))
                .collect::<Vec<_>>()
                .join(",");
            let reversed: ProfileSnapshot =
                serde_json::from_str(&format!("{{{reversed}}}")).unwrap();
            assert_eq!(
                reversed.canonical_hash().unwrap(),
                snapshot.canonical_hash().unwrap()
            );
        }
    }

    #[test]
    fn profile_identity_gate_detects_shape_valid_tampering() {
        let snapshot: ProfileSnapshot = serde_json::from_value(profile()).unwrap();
        let hash = snapshot.canonical_hash().unwrap();
        for (field, value) in [
            ("id", json!("other")),
            ("instructions", json!("changed")),
            ("model", json!("vendor/other")),
            ("definition_hash", json!("a".repeat(64))),
            ("origin", json!("argument")),
            ("source", json!("builtin")),
        ] {
            let mut tampered = profile();
            tampered[field] = value;
            let tampered: ProfileSnapshot = serde_json::from_value(tampered).unwrap();
            assert!(
                tampered
                    .validate_identity(&snapshot.id, &hash, snapshot.origin)
                    .is_err()
            );
        }
        assert!(
            snapshot
                .validate_identity("other", &hash, snapshot.origin)
                .is_err()
        );
        assert!(
            snapshot
                .validate_identity(&snapshot.id, &hash, ProfileOrigin::Argument)
                .is_err()
        );
        for corrupt_hash in ["", "not-hex", &"0".repeat(64), &hash.to_uppercase()] {
            assert!(
                snapshot
                    .validate_identity(&snapshot.id, corrupt_hash, snapshot.origin)
                    .is_err()
            );
        }
    }

    #[test]
    fn profile_requires_exact_keys_and_valid_enumerations_text_model_and_hash() {
        let good = profile();
        for key in good.as_object().unwrap().keys() {
            let mut missing = good.clone();
            missing.as_object_mut().unwrap().remove(key);
            rejects::<ProfileSnapshot>(missing);
        }
        let mut extra = good.clone();
        extra["extra"] = json!("sensitive-input");
        rejects::<ProfileSnapshot>(extra);
        for (key, bad) in [
            ("id", json!("Bad")),
            ("id", json!("a".repeat(65))),
            ("source", json!("argument")),
            ("origin", json!("config")),
            ("model_source", json!("unknown")),
            ("purpose", json!("я".repeat(201))),
            ("instructions", json!("я".repeat(8001))),
            ("instructions", json!("bad\u{7}")),
            ("purpose", json!("bad\u{7f}")),
            ("model", json!("model")),
            ("model", json!("provider/")),
            ("model", json!("/model")),
            ("model", json!(" provider/model")),
            ("model", json!("provider/model ")),
            ("definition_version", json!("")),
            ("definition_hash", json!("A".repeat(64))),
            ("definition_hash", json!("g".repeat(64))),
            ("definition_hash", json!("0".repeat(63))),
        ] {
            let mut value = good.clone();
            value[key] = bad;
            rejects::<ProfileSnapshot>(value);
        }
        let mut good = good;
        good["purpose"] = json!("я".repeat(200));
        good["instructions"] = json!("я".repeat(8000));
        good["model"] = Value::Null;
        round_trip::<ProfileSnapshot>(good);
    }

    #[test]
    fn checkpoint_round_trips_complete_multi_repository_state() {
        let mut value = checkpoint();
        let workspace = &mut value["repositories"][0];
        workspace["changed"] = json!({"src/bin.dat":{"digest":"aabbcc","kind":"binary"},
            "link":{"digest":"112233","kind":"symlink"}});
        workspace["absent"] = json!(["old.rs"]);
        workspace["diff_stat"]["entries"] = json!([
            {"path":"new.rs","from":"old.rs","change":"renamed","kind":"unknown"},
            {"path":"src/bin.dat","change":"modified","kind":"binary"}]);
        let mut external = workspace.clone();
        external["label"] = json!("external 1");
        value["repositories"].as_array_mut().unwrap().push(external);
        round_trip::<RoundCheckpoint>(value);
    }

    #[test]
    fn checkpoint_normalizes_hex_and_supports_unavailable_repositories() {
        let mut value = checkpoint();
        value["head"] = json!("ABC123");
        value["repositories"][0]["head"] = json!("ABC123");
        value["repositories"][0]["changed"] = json!({"file":{"digest":"AbCd","kind":"file"}});
        let parsed: RoundCheckpoint = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.head.as_ref().unwrap().as_str(), "abc123");
        assert_eq!(
            parsed.repositories[0]
                .changed
                .as_ref()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .digest
                .as_str(),
            "abcd"
        );
        let unavailable = json!({"label":"workspace","available":false,
            "diff_stat":{"counts":{"added":0,"modified":0,"deleted":0,"renamed":0},"entries":[],"truncated":0}});
        let mut value = json!({"version":1,"head":null,"index_fingerprint":null,"worktree_fingerprint":null,
            "repositories":[unavailable]});
        round_trip::<RoundCheckpoint>(value.clone());
        value["head"] = json!("abc");
        rejects::<RoundCheckpoint>(value);
    }

    #[test]
    fn checkpoint_rejects_missing_available_state_and_mirror_mismatches() {
        for key in [
            "head",
            "index_fingerprint",
            "worktree_fingerprint",
            "changed",
            "absent",
            "diff_stat",
        ] {
            let mut missing = checkpoint();
            missing["repositories"][0]
                .as_object_mut()
                .unwrap()
                .remove(key);
            rejects::<RoundCheckpoint>(missing);
        }
        for key in ["head", "index_fingerprint", "worktree_fingerprint"] {
            let mut missing = checkpoint();
            missing.as_object_mut().unwrap().remove(key);
            rejects::<RoundCheckpoint>(missing);
            let mut mismatch = checkpoint();
            mismatch[key] = json!("fedcba");
            rejects::<RoundCheckpoint>(mismatch);
        }
        for bad in [
            json!(false),
            json!(0),
            json!(-1),
            json!(1.0),
            json!(2),
            json!("1"),
        ] {
            let mut value = checkpoint();
            value["version"] = bad;
            rejects::<RoundCheckpoint>(value);
        }
        for bad in [json!(0), json!(1), json!("true"), Value::Null] {
            let mut value = checkpoint();
            value["repositories"][0]["available"] = bad;
            rejects::<RoundCheckpoint>(value);
        }
    }

    #[test]
    fn checkpoint_topology_is_unique_contiguous_and_bounded() {
        let mut value = checkpoint();
        for index in 1..64 {
            let mut repo = value["repositories"][0].clone();
            repo["label"] = json!(format!("external {index}"));
            value["repositories"].as_array_mut().unwrap().push(repo);
        }
        round_trip::<RoundCheckpoint>(value.clone());
        let mut extra = value.clone();
        let mut repo = extra["repositories"][0].clone();
        repo["label"] = json!("external 64");
        extra["repositories"].as_array_mut().unwrap().push(repo);
        rejects::<RoundCheckpoint>(extra);
        for label in [
            "workspace",
            "external 0",
            "external 2",
            "external 01",
            "/private/repo",
            "external 1\n",
        ] {
            let mut corrupt = value.clone();
            corrupt["repositories"][1]["label"] = json!(label);
            rejects::<RoundCheckpoint>(corrupt);
        }
        let mut reordered = value;
        reordered["repositories"].as_array_mut().unwrap().swap(0, 1);
        rejects::<RoundCheckpoint>(reordered);
        let mut empty = checkpoint();
        empty["repositories"] = json!([]);
        rejects::<RoundCheckpoint>(empty);
    }

    #[test]
    fn checkpoint_bounds_counts_entries_paths_refs_and_exact_state() {
        let mut boundary = checkpoint();
        boundary["repositories"][0]["diff_stat"]["counts"]["added"] = json!(1_000_000);
        boundary["repositories"][0]["diff_stat"]["truncated"] = json!(1_000_000);
        let entry = boundary["repositories"][0]["diff_stat"]["entries"][0].clone();
        boundary["repositories"][0]["diff_stat"]["entries"] = Value::Array(vec![entry; 200]);
        round_trip::<RoundCheckpoint>(boundary);
        for invalid in [
            json!(-1),
            json!(true),
            json!(0.5),
            json!(1_000_001),
            Value::Null,
        ] {
            let mut value = checkpoint();
            value["repositories"][0]["diff_stat"]["counts"]["added"] = invalid.clone();
            rejects::<RoundCheckpoint>(value);
            let mut value = checkpoint();
            value["repositories"][0]["diff_stat"]["truncated"] = invalid;
            rejects::<RoundCheckpoint>(value);
        }
        for path in [
            "/private/path",
            "a/../b",
            "a/./b",
            "a//b",
            "a\\b",
            "a\u{0}b",
            "",
        ] {
            rejects::<CheckpointPath>(json!(path));
            let mut value = checkpoint();
            value["repositories"][0]["absent"] = json!([path]);
            rejects::<RoundCheckpoint>(value);
        }
        round_trip::<CheckpointPath>(json!("я".repeat(300)));
        rejects::<CheckpointPath>(json!("я".repeat(301)));
        for digest in ["", "not-hex", "/private/path", &"a".repeat(129)] {
            rejects::<CheckpointRef>(json!(digest));
        }
        round_trip::<CheckpointRef>(json!("a".repeat(128)));
        let mut value = checkpoint();
        let entry = value["repositories"][0]["diff_stat"]["entries"][0].clone();
        value["repositories"][0]["diff_stat"]["entries"] = Value::Array(vec![entry; 201]);
        rejects::<RoundCheckpoint>(value);
        let mut value = checkpoint();
        value["repositories"][0]["absent"] = json!(vec!["file"; 2000]);
        round_trip::<RoundCheckpoint>(value.clone());
        value["repositories"][0]["changed"] = json!({"other":{"digest":"abcd","kind":"file"}});
        rejects::<RoundCheckpoint>(value);
        let mut value = checkpoint();
        let changed: serde_json::Map<String, Value> = (0..2001)
            .map(|index| {
                (
                    format!("file-{index}"),
                    json!({"digest":"abcd","kind":"file"}),
                )
            })
            .collect();
        value["repositories"][0]["changed"] = Value::Object(changed);
        rejects::<RoundCheckpoint>(value);
    }

    #[test]
    fn checkpoint_rejects_malformed_nested_state_and_rename_without_source() {
        for state in [
            json!(true),
            json!({}),
            json!({"digest":"abcd","kind":"directory"}),
            json!({"digest":12,"kind":"file"}),
        ] {
            let mut value = checkpoint();
            value["repositories"][0]["changed"] = json!({"file":state});
            rejects::<RoundCheckpoint>(value);
        }
        let mut value = checkpoint();
        value["repositories"][0]["diff_stat"]["entries"][0]["change"] = json!("renamed");
        rejects::<RoundCheckpoint>(value);
        for key in ["added", "modified", "deleted", "renamed"] {
            let mut value = checkpoint();
            value["repositories"][0]["diff_stat"]["counts"]
                .as_object_mut()
                .unwrap()
                .remove(key);
            rejects::<RoundCheckpoint>(value);
        }
        let mut value = checkpoint();
        value["repositories"][0]["absent"] = json!({});
        rejects::<RoundCheckpoint>(value);
    }

    #[test]
    fn malformed_models_and_errors_do_not_expose_sensitive_payloads() {
        let secret = "private/path-token=sensitive";
        let mut value = profile();
        value["unknown"] = json!(secret);
        let error = serde_json::from_value::<ProfileSnapshot>(value)
            .err()
            .unwrap();
        assert!(!error.to_string().contains(secret));
        let mut snapshot: ProfileSnapshot = serde_json::from_value(profile()).unwrap();
        snapshot.instructions = secret.to_owned();
        snapshot.id = secret.to_owned();
        assert!(!format!("{snapshot:?}").contains(secret));
        let error = snapshot.canonical_hash().unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::InvalidInput);
        assert!(!format!("{error} {error:?}").contains(secret));
        let mut finding: StructuredFinding = serde_json::from_value(finding()).unwrap();
        finding.path = secret.to_owned();
        finding.message = secret.to_owned();
        assert!(!format!("{finding:?}").contains(secret));
        finding.line = Some(0);
        assert!(finding.validate().is_err());
        let mut checkpoint: RoundCheckpoint = serde_json::from_value(checkpoint()).unwrap();
        checkpoint.repositories[0].changed = None;
        assert!(checkpoint.validate().is_err());
    }
}
