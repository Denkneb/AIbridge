//! Write-only provider secrets. Only variable names are returned to the UI.
use crate::projects::ProjectService;
use bridge_config::{
    ProjectEnv,
    migration::{atomic_write, safe_path},
    validate_config_text,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};

fn encode(values: &BTreeMap<String, String>) -> String {
    values
        .iter()
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect()
}

impl ProjectService {
    pub fn opencode_keys(&self, project: &str) -> Result<Value, String> {
        let (p, _) = self.project(project)?;
        let env = p
            .read_opencode_env()
            .map_err(|_| "Env-файл OpenCode недоступен или некорректен (нужны права 0600)")?;
        Ok(
            json!({"file":p.opencode_env_file().map(|f| f.as_path()), "names":env.as_ref().map(|e| e.names().collect::<Vec<_>>()).unwrap_or_default()}),
        )
    }

    pub fn save_opencode_key(
        &self,
        project: &str,
        name: &str,
        value: Option<&str>,
        expected_file: Option<&str>,
    ) -> Result<Value, String> {
        if name.is_empty()
            || name.len() > 256
            || !name
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || i > 0 && b.is_ascii_digit())
            || value.is_some_and(|v| {
                v.len() > 8192
                    || v.chars()
                        .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
            })
        {
            return Err("Некорректное имя или значение ключа".into());
        }
        // Validate names (including reserved service variables) even on deletion.
        ProjectEnv::parse(&format!("{name}=\n"))
            .map_err(|_| "Недопустимое имя переменной окружения")?;
        let _lock = self.config_file_guard()?;
        let config = self.config_view()?;
        let p = config.project(project).ok_or("Проект не найден")?;
        let current = p
            .opencode_env_file()
            .map(|f| f.as_path().to_string_lossy().into_owned());
        if current.as_deref() != expected_file {
            return Err("Env-файл проекта изменён. Загрузите список ключей заново".into());
        }
        let source = p
            .read_opencode_env()
            .map_err(|_| "Env-файл OpenCode недоступен или некорректен (нужны права 0600)")?;
        let mut values: BTreeMap<String, String> = source
            .as_ref()
            .map(|e| {
                e.iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(value) = value {
            if value.is_empty() {
                return Err("Введите значение ключа".into());
            }
            values.insert(name.into(), value.into());
        } else {
            values.remove(name);
        }
        let text = encode(&values);
        if text.len() > 65536 {
            return Err("Слишком большой список ключей".into());
        }
        ProjectEnv::parse(&text).map_err(|_| "Некорректный список ключей")?;
        let target = self
            .config
            .parent()
            .ok_or("Каталог конфигурации недоступен")?
            .join("secrets")
            .join(format!("desktop-{project}.opencode.env"));
        safe_path(&target)?;
        for other in config.projects().values() {
            if target.starts_with(other.workspace())
                || [other.password_file(), other.mcp_token_file()]
                    .into_iter()
                    .flatten()
                    .any(|f| f.as_path() == target)
                || other.id().as_str() != project
                    && other
                        .opencode_env_file()
                        .is_some_and(|f| f.as_path() == target)
            {
                return Err("Файл ключей пересекается с файлами другого назначения".into());
            }
        }
        let owned = p.opencode_env_file().is_some_and(|f| f.as_path() == target);
        if !owned && target.exists() {
            return Err("Файл ключей уже существует и не привязан к этому проекту".into());
        }
        let original = self.config_bytes()?;
        let mut doc = std::str::from_utf8(&original)
            .map_err(|_| "Некорректная конфигурация")?
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| "Некорректная конфигурация")?;
        doc["projects"][project]["opencode_env_file"] =
            toml_edit::value(target.to_string_lossy().as_ref());
        let proposed_text = doc.to_string();
        let proposed = validate_config_text(&proposed_text, &self.config, Some(&self.state))
            .map_err(|_| "Некорректная конфигурация")?;
        let _runtime = bridge_runtime::project::project_config_edit_guard(
            &config,
            &proposed,
            project,
            &self.state,
        )
        .map_err(|e| e.to_string())?;
        let mode = fs::metadata(&self.config)
            .map_err(|_| "Конфигурация недоступна")?
            .permissions()
            .mode()
            & 0o777;
        if !owned {
            let backup = self
                .config
                .with_file_name(format!("projects.keys-{}.bak", uuid::Uuid::new_v4()));
            atomic_write(&backup, &original, mode)?;
        }
        atomic_write(&target, text.as_bytes(), 0o600)?;
        if !owned && let Err(error) = atomic_write(&self.config, proposed_text.as_bytes(), mode) {
            let _ = fs::remove_file(&target);
            return Err(error.into());
        }
        // An already owned file is the only changed resource; its binding stays intact.
        Ok(json!({"file":target,"names":values.keys().collect::<Vec<_>>()}))
    }
}
