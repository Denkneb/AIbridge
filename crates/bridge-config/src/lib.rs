//! Minimal loader for the `projects.toml` configuration file.
//!
//! This crate implements only the structural loading stage described by task
//! 2.1: it reads a UTF-8 TOML file from an explicit path, requires a top-level
//! `projects` table and returns the raw per-project tables for the later,
//! dedicated validation tasks (2.2–2.9). It deliberately performs no semantic
//! validation of project ids, workspaces, endpoints, credentials, models or
//! permissions.
//!
//! Errors use the shared [`bridge_domain::DomainError`] and its
//! [`bridge_domain::ErrorKind`] category. Their [`Display`](std::fmt::Display)
//! and [`Debug`](std::fmt::Debug) output contains only static, developer
//! authored text: file contents, credential values and the absolute input path
//! are never rendered, even though the underlying diagnostic is retained as
//! the error [`source`](std::error::Error::source).

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use bridge_domain::{DomainError, Result};

/// The top-level TOML table that holds the configured projects.
pub const PROJECTS_TABLE: &str = "projects";

/// A single raw project entry from the `projects` table.
///
/// The entry keeps the parsed TOML table exactly as it appeared in the file so
/// that later validation tasks can inspect every key and value. No field is
/// interpreted here, and the entry is not a promise that the project is
/// semantically valid.
#[derive(Clone)]
pub struct ProjectEntry {
    values: toml::Table,
}

impl ProjectEntry {
    /// Returns the raw project table.
    #[must_use]
    pub fn values(&self) -> &toml::Table {
        &self.values
    }

    /// Returns the raw value stored under `key`, if present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&toml::Value> {
        self.values.get(key)
    }

    /// Returns `true` when the entry defines `key`.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
}

impl fmt::Debug for ProjectEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectEntry")
            .field("keys", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// A successfully loaded `projects.toml` configuration.
///
/// The container preserves the raw project tables keyed by their project id.
/// Iteration order is deterministic (lexicographic by id) because both the
/// backing [`BTreeMap`] and the default TOML map sort their keys.
#[derive(Clone)]
pub struct Config {
    projects: BTreeMap<String, ProjectEntry>,
}

impl Config {
    /// Returns all projects keyed by their raw project id.
    #[must_use]
    pub fn projects(&self) -> &BTreeMap<String, ProjectEntry> {
        &self.projects
    }

    /// Returns the raw entry for `id`, if present.
    #[must_use]
    pub fn project(&self, id: &str) -> Option<&ProjectEntry> {
        self.projects.get(id)
    }

    /// Returns the number of configured projects.
    #[must_use]
    pub fn len(&self) -> usize {
        self.projects.len()
    }

    /// Returns `true` when no project is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("projects", &self.projects.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Loads `projects.toml` from the explicit `path`.
///
/// # Errors
///
/// * [`bridge_domain::ErrorKind::NotFound`] when the file does not exist;
/// * [`bridge_domain::ErrorKind::PermissionDenied`] when the file cannot be
///   read because of permissions;
/// * [`bridge_domain::ErrorKind::Internal`] for any other I/O failure;
/// * [`bridge_domain::ErrorKind::InvalidInput`] when the bytes are not UTF-8,
///   when the TOML is syntactically invalid, when the `projects` table is
///   missing, or when the `projects` value or a project entry has the wrong
///   shape.
///
/// None of these errors renders the file contents, credential values or the
/// absolute input path.
pub fn load_config(path: &Path) -> Result<Config> {
    let bytes = std::fs::read(path).map_err(read_error)?;
    let text = String::from_utf8(bytes).map_err(|source| {
        DomainError::invalid_input("configuration file is not valid UTF-8").with_source(source)
    })?;
    parse_config(&text)
}

/// Parses already-read TOML text into a [`Config`].
///
/// The TOML grammar guarantees that a document root is a table, so the only
/// structural requirements checked here are the `projects` table and the shape
/// of its entries.
fn parse_config(text: &str) -> Result<Config> {
    let root: toml::Table = toml::from_str(text).map_err(|source| {
        DomainError::invalid_input("configuration file is not valid TOML").with_source(source)
    })?;

    let projects = root
        .get(PROJECTS_TABLE)
        .ok_or_else(|| DomainError::invalid_input("configuration is missing the 'projects' table"))?
        .as_table()
        .ok_or_else(|| DomainError::invalid_input("'projects' must be a TOML table"))?;

    let mut entries = BTreeMap::new();
    for (id, value) in projects {
        let values = value.as_table().ok_or_else(|| {
            DomainError::invalid_input("each 'projects' entry must be a TOML table")
        })?;
        entries.insert(
            id.clone(),
            ProjectEntry {
                values: values.clone(),
            },
        );
    }

    Ok(Config { projects: entries })
}

/// Maps an I/O failure to a safe, typed [`DomainError`].
///
/// The original [`std::io::Error`] is kept only as the error source, so its
/// path-bearing [`Display`](std::fmt::Display) text never reaches the safe
/// output.
fn read_error(source: std::io::Error) -> DomainError {
    match source.kind() {
        std::io::ErrorKind::NotFound => {
            DomainError::not_found("configuration file was not found").with_source(source)
        }
        std::io::ErrorKind::PermissionDenied => {
            DomainError::permission_denied("configuration file could not be read")
                .with_source(source)
        }
        _ => DomainError::internal("configuration file could not be read").with_source(source),
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, PROJECTS_TABLE, load_config, parse_config};
    use bridge_domain::ErrorKind;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Corpus case `valid-minimal-project`: required keys only.
    const MINIMAL: &str = "[projects.proj]\nworkspace = \"ws\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = 3\n";

    fn unique_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bridge-config-{tag}-{}-{nanos}.toml",
            std::process::id()
        ))
    }

    #[test]
    fn crate_constant_names_the_projects_table() {
        assert_eq!(PROJECTS_TABLE, "projects");
    }

    #[test]
    fn parses_minimal_project() {
        let config = parse_config(MINIMAL).expect("minimal config must load");

        assert_eq!(config.len(), 1);
        assert!(!config.is_empty());

        let entry = config.project("proj").expect("project 'proj' must exist");
        assert_eq!(
            entry.get("workspace").and_then(toml::Value::as_str),
            Some("ws")
        );
        assert_eq!(
            entry.get("opencode_url").and_then(toml::Value::as_str),
            Some("http://127.0.0.1:4101")
        );
        assert_eq!(
            entry.get("max_rounds").and_then(toml::Value::as_integer),
            Some(3)
        );
        assert!(entry.contains_key("password_file"));
        assert_eq!(entry.get("missing_key"), None);
        assert_eq!(entry.values().len(), 4);
    }

    #[test]
    fn parses_multiple_projects_in_deterministic_order() {
        let text = "[projects.beta]\nworkspace = \"b\"\n\n[projects.alpha]\nworkspace = \"a\"\n";
        let config = parse_config(text).expect("multi-project config must load");

        let ids: Vec<&str> = config.projects().keys().map(String::as_str).collect();
        assert_eq!(ids, ["alpha", "beta"]);
        assert_eq!(config.len(), 2);
        assert_eq!(
            config
                .project("alpha")
                .and_then(|entry| entry.get("workspace"))
                .and_then(toml::Value::as_str),
            Some("a")
        );
    }

    // Corpus case `invalid-toml-syntax`.
    #[test]
    fn rejects_syntactically_invalid_toml() {
        let error = parse_config("this is not = = = toml\n").expect_err("invalid TOML must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "configuration file is not valid TOML");
    }

    // Corpus case `invalid-projects-table-missing`.
    #[test]
    fn rejects_missing_projects_table() {
        let error =
            parse_config("[other]\nkey = \"value\"\n").expect_err("missing projects must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "configuration is missing the 'projects' table"
        );
    }

    #[test]
    fn rejects_non_table_projects_value() {
        for text in ["projects = \"nope\"\n", "projects = 5\n", "projects = [1, 2]\n"] {
            let error = parse_config(text).expect_err("non-table projects must fail");
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert_eq!(error.to_string(), "'projects' must be a TOML table");
        }
    }

    #[test]
    fn rejects_non_table_project_entry() {
        let error = parse_config("[projects]\nproj = 5\n").expect_err("non-table entry must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "each 'projects' entry must be a TOML table"
        );
    }

    #[test]
    fn empty_projects_table_is_valid() {
        let config = parse_config("[projects]\n").expect("empty projects table must load");

        assert!(config.is_empty());
        assert!(config.projects().is_empty());
    }

    #[test]
    fn unknown_top_level_keys_do_not_break_loading() {
        let text = "[other]\nkey = \"value\"\n\n[projects.proj]\nworkspace = \"ws\"\n";
        let config = parse_config(text).expect("unknown top-level keys must be ignored");

        assert_eq!(config.len(), 1);
        assert!(config.project("proj").is_some());
    }

    #[test]
    fn loads_config_from_explicit_path() {
        let path = unique_path("valid");
        std::fs::write(&path, MINIMAL).expect("temporary config must be writable");

        let config = load_config(&path).expect("config from file must load");

        assert_eq!(config.len(), 1);
        assert!(config.project("proj").is_some());

        std::fs::remove_file(&path).expect("temporary config must be removable");
    }

    #[test]
    fn missing_file_is_a_safe_not_found_error() {
        let path = unique_path("missing");
        let error = load_config(&path).expect_err("missing file must fail");

        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(error.to_string(), "configuration file was not found");

        let path_text = path.to_str().expect("temporary path must be UTF-8");
        let rendered = format!("{error} {error:?}");
        assert!(
            !rendered.contains(path_text),
            "safe error output leaked the input path: {rendered}"
        );
    }

    #[test]
    fn invalid_utf8_is_a_safe_shape_failure() {
        let path = unique_path("utf8");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).expect("temporary config must be writable");

        let error = load_config(&path).expect_err("invalid UTF-8 must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "configuration file is not valid UTF-8");

        let path_text = path.to_str().expect("temporary path must be UTF-8");
        let rendered = format!("{error} {error:?}");
        assert!(
            !rendered.contains(path_text),
            "safe error output leaked the input path: {rendered}"
        );

        std::fs::remove_file(&path).expect("temporary config must be removable");
    }

    #[test]
    fn parse_error_does_not_leak_toml_content_or_paths() {
        const SECRET: &str = "super-secret-token-value";
        const SECRET_PATH: &str = "/abs/secret/workspace/path";

        let text = format!("password_file = \"{SECRET}\"\n[projects.proj\nworkspace = \"{SECRET_PATH}\"\n");
        let error = parse_config(&text).expect_err("invalid TOML must fail");

        let display = error.to_string();
        let debug = format!("{error:?}");

        assert!(!display.contains(SECRET), "Display leaked a secret: {display}");
        assert!(!debug.contains(SECRET), "Debug leaked a secret: {debug}");
        assert!(
            !display.contains(SECRET_PATH),
            "Display leaked a path: {display}"
        );
        assert!(!debug.contains(SECRET_PATH), "Debug leaked a path: {debug}");
    }

    #[test]
    fn shape_error_does_not_leak_toml_content() {
        const SECRET: &str = "shape-secret-token-value";

        let text = format!("unknown_top_level = \"{SECRET}\"\n");
        let error = parse_config(&text).expect_err("missing projects must fail");

        let rendered = format!("{error} {error:?}");
        assert!(
            !rendered.contains(SECRET),
            "safe error output leaked a secret: {rendered}"
        );
    }

    #[test]
    fn config_debug_does_not_render_values() {
        const SECRET: &str = "debug-secret-value";

        let text = format!("[projects.proj]\npassword_file = \"{SECRET}\"\n");
        let config = parse_config(&text).expect("config must load");

        let debug = format!("{config:?}");
        assert!(!debug.contains(SECRET), "Config Debug leaked a value: {debug}");
        assert!(debug.contains("proj"));

        let entry_debug = format!("{:?}", config.project("proj").expect("project exists"));
        assert!(
            !entry_debug.contains(SECRET),
            "ProjectEntry Debug leaked a value: {entry_debug}"
        );
    }

    #[test]
    fn config_is_cloneable() {
        fn assert_clone<T: Clone>() {}

        assert_clone::<Config>();
        assert_clone::<super::ProjectEntry>();
    }
}
