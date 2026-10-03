//! Config-owned immutable definitions and pure selection/snapshot helpers.

use crate::{OpenCodeModel, ProjectEntry, parse_opencode_model, profile_secrets};
use bridge_domain::{
    DomainError, ProfileDefinitionSource, ProfileModelSource, ProfileOrigin, ProfileSnapshot,
    Result, profile_definition_hash,
};
use std::{collections::BTreeMap, fmt};

/// Version pinned for the exact built-in definition set.
pub const BUILTIN_PROFILE_VERSION: &str = "1";
/// Version pinned for config-defined profiles.
pub const CUSTOM_PROFILE_VERSION: &str = "1";
/// Historical built-in default executor.
pub const DEFAULT_PROFILE_ID: &str = "implementer";

/// Immutable, validated executor instructions. Profiles never widen task scope.
#[derive(Clone, PartialEq, Eq)]
pub struct ProfileDefinition {
    id: String,
    purpose: String,
    instructions: String,
    model: Option<OpenCodeModel>,
    source: ProfileDefinitionSource,
    definition_hash: String,
}
impl fmt::Debug for ProfileDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfileDefinition").finish_non_exhaustive()
    }
}
impl ProfileDefinition {
    fn new(
        id: String,
        purpose: String,
        instructions: String,
        model: Option<OpenCodeModel>,
        source: ProfileDefinitionSource,
    ) -> Result<Self> {
        let model_text = model.as_ref().map(model_text);
        let definition_hash =
            profile_definition_hash(&id, &purpose, &instructions, model_text.as_deref())?;
        Ok(Self {
            id,
            purpose,
            instructions,
            model,
            source,
            definition_hash,
        })
    }
    /// Safe profile identifier.
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Human-readable purpose, explicitly exposed.
    pub fn purpose(&self) -> &str {
        &self.purpose
    }
    /// Validated instruction text, explicitly exposed.
    pub fn instructions(&self) -> &str {
        &self.instructions
    }
    /// The definition's own model, before project-model inheritance.
    pub fn model(&self) -> Option<&OpenCodeModel> {
        self.model.as_ref()
    }
    /// Definition origin: builtin or config.
    pub fn source(&self) -> ProfileDefinitionSource {
        self.source
    }
    /// Python-compatible canonical definition SHA-256.
    pub fn definition_hash(&self) -> &str {
        &self.definition_hash
    }
    /// Whether selection preserves the historical built-in implementer behavior.
    pub fn is_historical_implementer(&self) -> bool {
        self.source == ProfileDefinitionSource::Builtin && self.id == DEFAULT_PROFILE_ID
    }
}

fn model_text(model: &OpenCodeModel) -> String {
    format!("{}/{}", model.provider(), model.model())
}

/// Borrowed selection from a project's immutable definitions.
#[derive(Debug, Clone)]
pub struct ResolvedProfile<'a> {
    definition: &'a ProfileDefinition,
    origin: ProfileOrigin,
}
impl<'a> ResolvedProfile<'a> {
    /// Selected definition, including any config override.
    pub fn definition(&self) -> &'a ProfileDefinition {
        self.definition
    }
    /// Selection origin, distinct from the definition source.
    pub fn origin(&self) -> ProfileOrigin {
        self.origin
    }
    /// Pins the effective model: profile model wins; otherwise use project model.
    pub fn snapshot(&self, project_model: Option<&OpenCodeModel>) -> ProfileSnapshot {
        let definition = self.definition;
        let (model, model_source) = match definition.model() {
            Some(model) => (Some(model), ProfileModelSource::Profile),
            None => (project_model, ProfileModelSource::Project),
        };
        ProfileSnapshot {
            id: definition.id.clone(),
            source: definition.source,
            origin: self.origin,
            purpose: definition.purpose.clone(),
            instructions: definition.instructions.clone(),
            model: model.map(model_text),
            model_source,
            definition_version: match definition.source {
                ProfileDefinitionSource::Builtin => BUILTIN_PROFILE_VERSION,
                ProfileDefinitionSource::Config => CUSTOM_PROFILE_VERSION,
            }
            .to_owned(),
            definition_hash: definition.definition_hash.clone(),
        }
    }
}

pub(super) fn resolve_profile<'a>(
    project: &'a ProjectEntry,
    requested: Option<&str>,
) -> Option<ResolvedProfile<'a>> {
    let (id, origin) = if let Some(id) = requested.filter(|id| !id.is_empty()) {
        (id, ProfileOrigin::Argument)
    } else if let Some(id) = project.default_profile() {
        (id, ProfileOrigin::ProjectDefault)
    } else {
        (DEFAULT_PROFILE_ID, ProfileOrigin::BuiltinDefault)
    };
    project
        .profile_definitions()
        .get(id)
        .map(|definition| ResolvedProfile { definition, origin })
}

/// Fresh merged-map foundation, with exact version-1 built-in instruction text.
pub fn builtin_profiles() -> BTreeMap<String, ProfileDefinition> {
    BUILTINS
        .iter()
        .map(|(id, purpose, instructions)| {
            let definition = ProfileDefinition::new(
                (*id).to_owned(),
                (*purpose).to_owned(),
                (*instructions).to_owned(),
                None,
                ProfileDefinitionSource::Builtin,
            )
            .expect("validated fixed built-in definition");
            ((*id).to_owned(), definition)
        })
        .collect()
}

fn error(message: &'static str) -> DomainError {
    DomainError::invalid_input(message)
}
fn text_has_controls(value: &str) -> bool {
    value
        .chars()
        .any(|c| matches!(c as u32, 0..=8 | 11..=12 | 14..=31 | 127))
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

pub(super) fn parse_definitions(
    values: &toml::Table,
) -> Result<BTreeMap<String, ProfileDefinition>> {
    let mut definitions = builtin_profiles();
    let Some(raw) = values.get("profiles") else {
        return Ok(definitions);
    };
    let table = raw
        .as_table()
        .ok_or_else(|| error("project profiles must be a table"))?;
    for (id, raw) in table {
        let profile = raw
            .as_table()
            .ok_or_else(|| error("project profile must be a table"))?;
        if profile
            .keys()
            .any(|key| !matches!(key.as_str(), "purpose" | "instructions" | "model"))
        {
            return Err(error("project profile has an unknown key"));
        }
        if !valid_id(id) {
            return Err(error("project profile id is invalid"));
        }
        let purpose = profile
            .get("purpose")
            .ok_or_else(|| error("project profile purpose is missing"))?
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| error("project profile purpose must be a non-empty string"))?;
        if purpose.chars().count() > 200 {
            return Err(error("project profile purpose is too long"));
        }
        if text_has_controls(purpose) {
            return Err(error("project profile purpose contains control characters"));
        }
        let instructions = match profile.get("instructions") {
            Some(raw) => raw
                .as_str()
                .ok_or_else(|| error("project profile instructions must be a string"))?,
            None => "",
        };
        if instructions.chars().count() > 8000 {
            return Err(error("project profile instructions are too long"));
        }
        if text_has_controls(instructions) {
            return Err(error(
                "project profile instructions contain control characters",
            ));
        }
        if !profile_secrets::categories(instructions).is_empty() {
            return Err(error(
                "project profile instructions carry a suspected secret",
            ));
        }
        let model = profile.get("model").map(|raw| {
            let raw = raw.as_str().ok_or_else(|| error("project profile model must be a string"))?;
            parse_opencode_model(raw).map_err(|_| error("project profile model must be provider/model without surrounding whitespace"))
        }).transpose()?;
        let definition = ProfileDefinition::new(
            id.clone(),
            purpose.to_owned(),
            instructions.to_owned(),
            model,
            ProfileDefinitionSource::Config,
        )?;
        definitions.insert(id.clone(), definition);
    }
    Ok(definitions)
}

pub(super) fn parse_default(
    values: &toml::Table,
    definitions: &BTreeMap<String, ProfileDefinition>,
) -> Result<Option<String>> {
    let Some(raw) = values.get("default_profile") else {
        return Ok(None);
    };
    let id = raw
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| error("project default_profile must be a non-empty string"))?;
    if id.trim() != id {
        return Err(error(
            "project default_profile must not have surrounding whitespace",
        ));
    }
    if !definitions.contains_key(id) {
        return Err(error("project default_profile is not a known profile"));
    }
    Ok(Some(id.to_owned()))
}

// Exact texts from read-only Python HEAD 86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a.
const BUILTINS: [(&str, &str, &str); 4] = [
    (
        "implementer",
        "Обычная реализация согласованного плана в allowed_paths.",
        "",
    ),
    (
        "test-writer",
        "Тесты и тестовые фикстуры без изменения production-кода.",
        "Профиль test-writer (дополнительные акценты; не расширяет scope и разрешения bridge):\n- не меняй production-код вне явно разрешённых тестовых путей;\n- пиши детерминированные, изолированные тесты без зависимости от порядка запуска и внешней сети;\n- запускай ровно согласованные test_commands и сохраняй реальные результаты;\n- не подгоняй тесты под неверное поведение без явного указания в задаче.",
    ),
    (
        "migration-specialist",
        "Миграции схемы/данных с обратной совместимостью.",
        "Профиль migration-specialist (дополнительные акценты; не расширяет scope и разрешения bridge):\n- делай миграции аддитивными и идемпотентными, безопасными при повторном запуске;\n- сохраняй обратную совместимость чтения старых данных;\n- не выполняй деструктивных операций без отдельного согласования в задаче;\n- проверяй повторный запуск миграции и запускай согласованные test_commands.",
    ),
    (
        "review-investigator",
        "Read-only сбор проверяемых фактов для независимого ревью.",
        "Профиль review-investigator (дополнительные акценты; не расширяет scope и разрешения bridge):\n- не меняй production-код; при необходимости пиши только отчёт в явно разрешённый путь;\n- возвращай проверяемые факты (ссылки на файлы и строки, шаги воспроизведения), а не выводы-заменители ревью;\n- этот профиль не заменяет независимое ревью Codex и не даёт права приёмки.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Config, load_config,
        tests::{TempDir, project_toml_with},
    };
    use serde_json::{Value, json};
    // Independently computed by pinned Python profiles.py, no runtime state.
    const GOLDENS: &str = r###"[
  {
    "snapshot": {
      "id": "implementer",
      "source": "builtin",
      "origin": "argument",
      "purpose": "Обычная реализация согласованного плана в allowed_paths.",
      "instructions": "",
      "model": null,
      "model_source": "project",
      "definition_version": "1",
      "definition_hash": "c3a63d5c7a5a08a107a1b75ed0dff5be3b482f96d742ca17090fc1eff221cfc6"
    },
    "canonical": "{\"definition_hash\":\"c3a63d5c7a5a08a107a1b75ed0dff5be3b482f96d742ca17090fc1eff221cfc6\",\"definition_version\":\"1\",\"id\":\"implementer\",\"instructions\":\"\",\"model\":null,\"model_source\":\"project\",\"origin\":\"argument\",\"purpose\":\"Обычная реализация согласованного плана в allowed_paths.\",\"source\":\"builtin\"}",
    "hash": "26db54569cb453c5d2808d7367b3b89d8bc70da3db56368cfe62456cd4384632"
  },
  {
    "snapshot": {
      "id": "test-writer",
      "source": "builtin",
      "origin": "argument",
      "purpose": "Тесты и тестовые фикстуры без изменения production-кода.",
      "instructions": "Профиль test-writer (дополнительные акценты; не расширяет scope и разрешения bridge):\n- не меняй production-код вне явно разрешённых тестовых путей;\n- пиши детерминированные, изолированные тесты без зависимости от порядка запуска и внешней сети;\n- запускай ровно согласованные test_commands и сохраняй реальные результаты;\n- не подгоняй тесты под неверное поведение без явного указания в задаче.",
      "model": null,
      "model_source": "project",
      "definition_version": "1",
      "definition_hash": "adb167704ad838bea3b7d7408097055761a3c3126269b115e105a62fc294b30c"
    },
    "canonical": "{\"definition_hash\":\"adb167704ad838bea3b7d7408097055761a3c3126269b115e105a62fc294b30c\",\"definition_version\":\"1\",\"id\":\"test-writer\",\"instructions\":\"Профиль test-writer (дополнительные акценты; не расширяет scope и разрешения bridge):\\n- не меняй production-код вне явно разрешённых тестовых путей;\\n- пиши детерминированные, изолированные тесты без зависимости от порядка запуска и внешней сети;\\n- запускай ровно согласованные test_commands и сохраняй реальные результаты;\\n- не подгоняй тесты под неверное поведение без явного указания в задаче.\",\"model\":null,\"model_source\":\"project\",\"origin\":\"argument\",\"purpose\":\"Тесты и тестовые фикстуры без изменения production-кода.\",\"source\":\"builtin\"}",
    "hash": "c28451d20d6b39f20abf5cb62f296ec6274f4c5a0284e56157f7f903beb26ca5"
  },
  {
    "snapshot": {
      "id": "migration-specialist",
      "source": "builtin",
      "origin": "argument",
      "purpose": "Миграции схемы/данных с обратной совместимостью.",
      "instructions": "Профиль migration-specialist (дополнительные акценты; не расширяет scope и разрешения bridge):\n- делай миграции аддитивными и идемпотентными, безопасными при повторном запуске;\n- сохраняй обратную совместимость чтения старых данных;\n- не выполняй деструктивных операций без отдельного согласования в задаче;\n- проверяй повторный запуск миграции и запускай согласованные test_commands.",
      "model": null,
      "model_source": "project",
      "definition_version": "1",
      "definition_hash": "d00b3a2d30ddff5fc4d681e0ffcc7d519be1caa7e176d1eb404d292a97ee0870"
    },
    "canonical": "{\"definition_hash\":\"d00b3a2d30ddff5fc4d681e0ffcc7d519be1caa7e176d1eb404d292a97ee0870\",\"definition_version\":\"1\",\"id\":\"migration-specialist\",\"instructions\":\"Профиль migration-specialist (дополнительные акценты; не расширяет scope и разрешения bridge):\\n- делай миграции аддитивными и идемпотентными, безопасными при повторном запуске;\\n- сохраняй обратную совместимость чтения старых данных;\\n- не выполняй деструктивных операций без отдельного согласования в задаче;\\n- проверяй повторный запуск миграции и запускай согласованные test_commands.\",\"model\":null,\"model_source\":\"project\",\"origin\":\"argument\",\"purpose\":\"Миграции схемы/данных с обратной совместимостью.\",\"source\":\"builtin\"}",
    "hash": "38ad4bfea1bae16cf3c9345e556a9dfc244620f657a908f48d533833f382c4a9"
  },
  {
    "snapshot": {
      "id": "review-investigator",
      "source": "builtin",
      "origin": "argument",
      "purpose": "Read-only сбор проверяемых фактов для независимого ревью.",
      "instructions": "Профиль review-investigator (дополнительные акценты; не расширяет scope и разрешения bridge):\n- не меняй production-код; при необходимости пиши только отчёт в явно разрешённый путь;\n- возвращай проверяемые факты (ссылки на файлы и строки, шаги воспроизведения), а не выводы-заменители ревью;\n- этот профиль не заменяет независимое ревью Codex и не даёт права приёмки.",
      "model": null,
      "model_source": "project",
      "definition_version": "1",
      "definition_hash": "0caadf69a2427a157c3b35edb20c3a468acb7f39aebb3c9a9c912798ebb70606"
    },
    "canonical": "{\"definition_hash\":\"0caadf69a2427a157c3b35edb20c3a468acb7f39aebb3c9a9c912798ebb70606\",\"definition_version\":\"1\",\"id\":\"review-investigator\",\"instructions\":\"Профиль review-investigator (дополнительные акценты; не расширяет scope и разрешения bridge):\\n- не меняй production-код; при необходимости пиши только отчёт в явно разрешённый путь;\\n- возвращай проверяемые факты (ссылки на файлы и строки, шаги воспроизведения), а не выводы-заменители ревью;\\n- этот профиль не заменяет независимое ревью Codex и не даёт права приёмки.\",\"model\":null,\"model_source\":\"project\",\"origin\":\"argument\",\"purpose\":\"Read-only сбор проверяемых фактов для независимого ревью.\",\"source\":\"builtin\"}",
    "hash": "8ff9e3e01c3161e6c8f86fd5a1119e97b4011fd6dcd39e04455d00bae95a4cba"
  }
]"###;

    fn load(extra: &str) -> (TempDir, Config) {
        let dir = TempDir::new("profile-config");
        let workspace = dir.mkdir("ws");
        let path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().unwrap(),
                "http://127.0.0.1:4101",
                extra,
            ),
        );
        let config = load_config(&path).expect("valid profile config");
        (dir, config)
    }
    fn invalid(extra: &str) -> DomainError {
        let dir = TempDir::new("profile-invalid");
        let workspace = dir.mkdir("ws");
        let path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().unwrap(),
                "http://127.0.0.1:4101",
                extra,
            ),
        );
        load_config(&path).expect_err("invalid profile config must fail")
    }
    fn custom_text(purpose: &str, instructions: &str) -> String {
        format!(
            "[projects.proj.profiles.custom]\npurpose = {}\ninstructions = {}\n",
            toml::Value::String(purpose.to_owned()),
            toml::Value::String(instructions.to_owned())
        )
    }
    fn definition_json(definition: &ProfileDefinition) -> Value {
        json!({"id":definition.id(), "purpose":definition.purpose(), "instructions":definition.instructions(),
            "model":definition.model().map(model_text), "source":definition.source().as_str()})
    }
    fn check_subset(actual: &Value, expected: &Value) {
        for (key, value) in expected.as_object().unwrap() {
            assert_eq!(&actual[key], value, "field {key}");
        }
    }

    #[test]
    fn builtins_and_canonical_snapshots_match_independent_python_goldens() {
        let (_dir, config) = load("");
        let entry = config.project("proj").unwrap();
        assert_eq!(entry.profile_definitions().len(), 4);
        assert_eq!(entry.default_profile(), None);
        assert!(!entry.contains_key("profiles"));
        assert!(!entry.contains_key("default_profile"));
        for golden in serde_json::from_str::<Vec<Value>>(GOLDENS).unwrap() {
            let id = golden["snapshot"]["id"].as_str().unwrap();
            let snapshot = entry.profile_snapshot(Some(id)).unwrap();
            assert_eq!(serde_json::to_value(&snapshot).unwrap(), golden["snapshot"]);
            assert_eq!(
                snapshot.canonical_json().unwrap(),
                golden["canonical"].as_str().unwrap()
            );
            assert_eq!(
                snapshot.canonical_hash().unwrap(),
                golden["hash"].as_str().unwrap()
            );
        }
        let historical = entry.resolve_profile(None).unwrap();
        assert!(historical.definition().is_historical_implementer());
        assert_eq!(historical.definition().instructions(), "");
        assert_eq!(historical.origin(), ProfileOrigin::BuiltinDefault);
    }

    #[test]
    fn profile_selection_precedence_empty_request_and_unknown_refusal() {
        let (_dir, config) = load("default_profile = 'test-writer'\n");
        let entry = config.project("proj").unwrap();
        for requested in [None, Some("")] {
            let resolved = entry.resolve_profile(requested).unwrap();
            assert_eq!(resolved.definition().id(), "test-writer");
            assert_eq!(resolved.origin(), ProfileOrigin::ProjectDefault);
        }
        let explicit = entry.resolve_profile(Some("implementer")).unwrap();
        assert_eq!(explicit.origin(), ProfileOrigin::Argument);
        assert!(explicit.definition().is_historical_implementer());
        for id in ["unknown-private-profile", " implementer", "implementer "] {
            assert!(entry.resolve_profile(Some(id)).is_none());
            let error = entry.profile_snapshot(Some(id)).unwrap_err();
            assert_eq!(error.kind(), bridge_domain::ErrorKind::NotFound);
            assert!(!format!("{error} {error:?}").contains(id));
        }
    }

    #[test]
    fn custom_override_is_nonhistorical_even_with_the_same_definition_hash() {
        let builtin = builtin_profiles().remove("implementer").unwrap();
        let extra = format!(
            "default_profile = 'implementer'\n[projects.proj.profiles.implementer]\npurpose = {}\n",
            toml::Value::String(builtin.purpose().to_owned())
        );
        let (_dir, config) = load(&extra);
        let entry = config.project("proj").unwrap();
        let resolved = entry.resolve_profile(None).unwrap();
        assert_eq!(resolved.origin(), ProfileOrigin::ProjectDefault);
        assert_eq!(
            resolved.definition().source(),
            ProfileDefinitionSource::Config
        );
        assert_eq!(
            resolved.definition().definition_hash(),
            builtin.definition_hash()
        );
        assert!(!resolved.definition().is_historical_implementer());
        assert_eq!(entry.profile_definitions().len(), 4);
    }

    #[test]
    fn snapshot_pins_effective_model_and_keeps_definition_hash_independent() {
        let custom = "[projects.proj.profiles.custom]\npurpose = 'custom'\n";
        let (_a, no_model) = load(custom);
        let (_b, inherited) = load(&format!("opencode_model = 'project/model'\n{custom}"));
        let (_c, own_model) = load(&format!(
            "opencode_model = 'project/model'\n{custom}model = 'profile/model/sub'\n"
        ));
        let a = no_model
            .project("proj")
            .unwrap()
            .profile_snapshot(Some("custom"))
            .unwrap();
        let b = inherited
            .project("proj")
            .unwrap()
            .profile_snapshot(Some("custom"))
            .unwrap();
        let c = own_model
            .project("proj")
            .unwrap()
            .profile_snapshot(Some("custom"))
            .unwrap();
        assert_eq!(a.model, None);
        assert_eq!(a.model_source, ProfileModelSource::Project);
        assert_eq!(b.model.as_deref(), Some("project/model"));
        assert_eq!(b.model_source, ProfileModelSource::Project);
        assert_eq!(c.model.as_deref(), Some("profile/model/sub"));
        assert_eq!(c.model_source, ProfileModelSource::Profile);
        assert_eq!(a.definition_hash, b.definition_hash);
        assert_ne!(a.canonical_hash().unwrap(), b.canonical_hash().unwrap());
        let pinned = b.clone();
        drop(inherited);
        assert_eq!(pinned.model.as_deref(), Some("project/model"));
        let (_d, historical) = load("opencode_model = 'project/model'\n");
        let snapshot = historical
            .project("proj")
            .unwrap()
            .profile_snapshot(None)
            .unwrap();
        assert_eq!(snapshot.instructions, "");
        assert_eq!(snapshot.model.as_deref(), Some("project/model"));
    }

    #[test]
    fn custom_text_bounds_controls_and_unicode_character_counts() {
        let (_dir, config) = load(&custom_text(&"я".repeat(200), &"я".repeat(8000)));
        config
            .project("proj")
            .unwrap()
            .profile_snapshot(Some("custom"))
            .unwrap()
            .validate()
            .unwrap();
        for (purpose, instructions) in [
            ("я".repeat(201), "".to_owned()),
            ("p".to_owned(), "я".repeat(8001)),
            (" \n\t".to_owned(), "".to_owned()),
            ("p\u{1}".to_owned(), "".to_owned()),
            ("p".to_owned(), "x\u{7f}".to_owned()),
        ] {
            assert_eq!(
                invalid(&custom_text(&purpose, &instructions)).kind(),
                bridge_domain::ErrorKind::InvalidInput
            );
        }
        load(&custom_text(" p ", "line\n\ttab\rreturn"));
        let id = "a".repeat(64);
        load(&format!(
            "default_profile = '{id}'\n[projects.proj.profiles.{id}]\npurpose = 'p'\n"
        ));
        for id in [
            "Bad".to_owned(),
            "_bad".to_owned(),
            "1bad".to_owned(),
            "a".repeat(65),
            "я".to_owned(),
        ] {
            invalid(&format!(
                "[projects.proj.profiles.{}]\npurpose = 'p'\n",
                toml::Value::String(id)
            ));
        }
    }

    #[test]
    fn profile_tables_reject_security_keys_and_malformed_models() {
        for key in [
            "allowed_paths",
            "test_commands",
            "allow_dirty",
            "allow_commit",
            "allow_suspected_secrets",
            "execution_mode",
            "auto_approve_permissions",
        ] {
            let error = invalid(&format!(
                "[projects.proj.profiles.custom]\npurpose = 'p'\n{key} = true\n"
            ));
            assert_eq!(error.message(), "project profile has an unknown key");
        }
        for raw in [
            "true",
            "3",
            "[]",
            "{}",
            "'model'",
            "'provider/'",
            "'/model'",
            "' provider/model'",
            "'provider/ model'",
        ] {
            invalid(&format!(
                "[projects.proj.profiles.custom]\npurpose = 'p'\nmodel = {raw}\n"
            ));
        }
    }

    #[test]
    fn secret_instructions_fail_closed_without_disclosing_credentials() {
        let corpus: Value =
            serde_json::from_str(include_str!("../../../docs/fixtures/security-cases.json"))
                .unwrap();
        let mut checked = 0;
        for case in corpus["cases"].as_array().unwrap() {
            if case["operation"] != "secret_gate"
                || case["input"]["allow"] == true
                || case["expect"]["categories"]
                    .as_array()
                    .is_none_or(|items| items.is_empty())
            {
                continue;
            }
            checked += 1;
            let text = case["input"]["text"].as_str().unwrap();
            let error = invalid(&custom_text("p", text));
            assert_eq!(
                error.message(),
                "project profile instructions carry a suspected secret"
            );
            assert!(!format!("{error} {error:?}").contains(text));
        }
        assert!(checked >= 9);
        load(&custom_text(
            "password and secret documentation",
            "password=example secret=placeholder",
        ));
    }

    #[test]
    fn definitions_and_errors_redact_input_and_projects_are_isolated() {
        let private = "private-instruction-value";
        let (_dir, config) = load(&custom_text("private-purpose-value", private));
        let entry = config.project("proj").unwrap();
        let resolved = entry.resolve_profile(Some("custom")).unwrap();
        assert!(!format!("{entry:?} {resolved:?}").contains(private));
        for error in [
            invalid("default_profile = 'private-unknown-profile'\n"),
            invalid("[projects.proj.profiles.custom]\npurpose = 'p'\nprivate_unknown_key = true\n"),
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains("private"));
        }
        let dir = TempDir::new("profile-isolation");
        let a = dir.mkdir("a");
        let b = dir.mkdir("b");
        let text = format!(
            "{}\n{}",
            project_toml_with(
                "proj",
                a.to_str().unwrap(),
                "http://127.0.0.1:4101",
                &custom_text("p", "")
            ),
            project_toml_with("other", b.to_str().unwrap(), "http://127.0.0.1:4102", "")
        );
        let config = load_config(&dir.write("projects.toml", &text)).unwrap();
        assert!(
            config
                .project("proj")
                .unwrap()
                .profile_definitions()
                .contains_key("custom")
        );
        assert!(
            !config
                .project("other")
                .unwrap()
                .profile_definitions()
                .contains_key("custom")
        );
        let bad = text.replace(
            "[projects.\"other\"]\n",
            "[projects.\"other\"]\ndefault_profile = 'custom'\n",
        );
        assert_ne!(
            bad, text,
            "the invalid project default must actually be inserted"
        );
        assert!(load_config(&dir.write("projects.toml", &bad)).is_err());
    }

    #[test]
    fn all_profile_config_resolution_and_snapshot_corpus_cases_are_checked() {
        let corpus: Value =
            serde_json::from_str(include_str!("../../../docs/fixtures/config-cases.json")).unwrap();
        let mut checked = 0;
        for case in corpus["cases"].as_array().unwrap() {
            if !case["rule"]
                .as_str()
                .is_some_and(|rule| rule.contains("profile"))
            {
                continue;
            }
            checked += 1;
            let dir = TempDir::new("profile-corpus");
            let workspace = dir.mkdir("ws");
            let text = case["toml"]
                .as_str()
                .unwrap()
                .replace("${WORKSPACE}", workspace.to_str().unwrap());
            let result = load_config(&dir.write("projects.toml", &text));
            let valid = case["expectation"] == "valid";
            match case["operation"].as_str().unwrap() {
                "load_all_projects" => {
                    if !valid {
                        assert_eq!(
                            result.unwrap_err().kind(),
                            bridge_domain::ErrorKind::InvalidInput
                        );
                        continue;
                    }
                    let config = result.unwrap_or_else(|e| panic!("case {}: {e}", case["id"]));
                    let entry = config.project("proj").unwrap();
                    if let Some(expected) = case["expect"].get("default_profile") {
                        assert_eq!(json!(entry.default_profile()), *expected);
                    }
                    if let Some(definitions) = case["expect"].get("profile_definitions") {
                        for (id, expected) in definitions.as_object().unwrap() {
                            check_subset(
                                &definition_json(&entry.profile_definitions()[id]),
                                expected,
                            );
                        }
                    }
                }
                "resolve_profile" => {
                    let config = result.unwrap();
                    let entry = config.project("proj").unwrap();
                    let resolved = entry.resolve_profile(case["profile"].as_str());
                    if !valid {
                        assert!(resolved.is_none());
                        continue;
                    }
                    let resolved = resolved.unwrap();
                    let mut actual = definition_json(resolved.definition());
                    actual["origin"] = json!(resolved.origin().as_str());
                    check_subset(&actual, &case["expect"]);
                }
                "profile_snapshot" => {
                    let config = result.unwrap();
                    let snapshot = config
                        .project("proj")
                        .unwrap()
                        .profile_snapshot(case["profile"].as_str())
                        .unwrap();
                    if valid {
                        check_subset(&serde_json::to_value(&snapshot).unwrap(), &case["expect"]);
                    } else {
                        let mut corrupt = serde_json::to_value(&snapshot).unwrap();
                        corrupt[case["tamper"].as_str().unwrap()] = case["tamper_value"].clone();
                        let corrupt: ProfileSnapshot = serde_json::from_value(corrupt).unwrap();
                        assert!(
                            corrupt
                                .validate_identity(
                                    &snapshot.id,
                                    &snapshot.canonical_hash().unwrap(),
                                    snapshot.origin
                                )
                                .is_err()
                        );
                    }
                }
                other => panic!("unexpected profile operation {other}"),
            }
        }
        assert_eq!(checked, 33);
    }
}
