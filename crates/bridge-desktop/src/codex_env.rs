//! Per-project private Codex launch environment; values never enter project TOML.
use std::collections::BTreeMap;

pub fn parse(text: &str) -> Result<BTreeMap<String, String>, String> {
    if text.len() > 65536 {
        return Err("Слишком большой список переменных окружения".into());
    }
    let mut values = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let invalid = || format!("Некорректная переменная окружения в строке {}", index + 1);
        let (name, value) = line.split_once('=').ok_or_else(invalid)?;
        if name.is_empty()
            || !name
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
            || value.contains('\0')
        {
            return Err(invalid());
        }
        let value = value.trim();
        let value = if value.starts_with(['\'', '"']) {
            if value.len() < 2 || !value.ends_with(value.chars().next().unwrap()) {
                return Err(invalid());
            }
            &value[1..value.len() - 1]
        } else {
            value
        };
        values.insert(name.to_owned(), value.to_owned());
    }
    Ok(values)
}

impl crate::projects::ProjectService {
    fn codex_env_path(&self, project: &str) -> Result<std::path::PathBuf, String> {
        let (_, layout) = self.project(project).map_err(str::to_owned)?;
        Ok(layout.project_dir().join("desktop-codex.env"))
    }
    pub fn read_codex_env(&self, project: &str) -> Result<String, String> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let path = self.codex_env_path(project)?;
        bridge_config::migration::safe_path(&path).map_err(str::to_owned)?;
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
            Err(_) => return Err("Переменные Codex недоступны".into()),
        };
        let metadata = file.metadata().map_err(|_| "Переменные Codex недоступны")?;
        if !metadata.is_file()
            || metadata.len() > 65536
            || metadata.uid() != nix::unistd::Uid::current().as_raw()
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err("Файл переменных Codex должен принадлежать текущему пользователю и иметь права 0600".into());
        }
        let mut text = String::new();
        file.by_ref()
            .take(65537)
            .read_to_string(&mut text)
            .map_err(|_| "Не удалось прочитать переменные Codex")?;
        parse(&text)?;
        Ok(text)
    }
    pub fn save_codex_env(&self, project: &str, text: &str) -> Result<(), String> {
        parse(text)?;
        let path = self.codex_env_path(project)?;
        // Also refuse unsafe pre-existing files, rather than replacing foreign state.
        self.read_codex_env(project)?;
        bridge_config::migration::atomic_write(&path, text.as_bytes(), 0o600).map_err(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    #[test]
    fn accepts_exports_quotes_empty_values_and_literal_shell_text() {
        let values = parse("# comment\nexport MODEL=first\nMODEL=second\nKEY='with spaces'\nEMPTY=\nLITERAL=\"$(echo secret) $HOME\"\n").unwrap();
        assert_eq!(values["MODEL"], "second");
        assert_eq!(values["KEY"], "with spaces");
        assert_eq!(values["EMPTY"], "");
        assert_eq!(values["LITERAL"], "$(echo secret) $HOME");
    }
    #[test]
    fn rejects_bad_names_quotes_and_nul_without_echoing_secrets() {
        for text in ["1KEY=secret", "export KEY", "KEY='secret", "KEY=secret\0"] {
            let error = parse(text).unwrap_err();
            assert!(error.contains("строке 1"));
            assert!(!error.contains("secret"));
        }
    }
}
