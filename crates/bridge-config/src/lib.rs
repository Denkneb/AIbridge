//! Loader and validation for the `projects.toml` configuration file.
//!
//! The crate implements the structural loading stage (task 2.1), the
//! project-id/workspace validation group (task 2.2), the endpoint/port
//! validation group (task 2.3), the cross-project uniqueness group (task 2.4),
//! the max-rounds/model/optional-path group (task 2.5) and the
//! auto-approve-permissions group (task 2.6). It reads a UTF-8 TOML file from an
//! explicit path, requires a top-level `projects` table and, for every project
//! entry, validates the project id against `^[a-z0-9][a-z0-9_-]{0,63}$`,
//! resolves the `workspace` to an existing directory, parses the required
//! `opencode_url` and the optional `mcp_url` into typed loopback endpoints,
//! requires a positive integer `max_rounds`, parses the optional
//! `opencode_model` and `opencode_env_file` and collects the optional
//! `auto_approve_permissions`. Relative workspaces and relative
//! `opencode_env_file` paths resolve against the directory that contains the
//! specific `projects.toml`, never against the process working directory. After
//! the individual projects pass, the loader rejects a canonical workspace, a
//! server endpoint or an MCP token file that is reused, and an MCP token file
//! that coincides with a password file.
//!
//! `auto_approve_permissions` is an optional TOML array of ordinary permission
//! names. Every entry must be a non-empty string without surrounding whitespace,
//! the reserved name `external_directory` is rejected in favour of
//! `auto_approve_external_directories`, and duplicates collapse while preserving
//! the first-seen order. An absent key yields an empty collection.
//!
//! Both URLs must be `http` URLs on the exact host `127.0.0.1` with an explicit
//! decimal port in `1..=65535` and no path, query, fragment, username or
//! password; `mcp_url` must additionally end with `/mcp`. This mirrors the
//! reference Python implementation (`src/agent_bridge/config.py`,
//! `_parse_endpoint`), including that an empty path and `/` are both accepted
//! for the OpenCode endpoint. The `url` crate validates the general syntax, but
//! the contract-sensitive scheme, host, explicit port and path are checked
//! against the original spelling so that WHATWG normalization (default-port
//! removal, non-standard IPv4 canonicalization, dot-segment folding) cannot
//! silently widen the contract.
//!
//! The remaining validation groups (trusted external directories, credential
//! readers and the project env reader) are deliberately out of scope. The raw
//! per-project table is preserved on [`ProjectEntry::values`] so those later
//! tasks can inspect every key and value without re-parsing.
//!
//! Errors use the shared [`bridge_domain::DomainError`] and its
//! [`bridge_domain::ErrorKind`] category. Their [`Display`](std::fmt::Display)
//! and [`Debug`](std::fmt::Debug) output contains only static, developer
//! authored text: project ids, workspace paths, config paths, file contents,
//! credential values and URL inputs are never rendered. Underlying parser
//! diagnostics are retained only as the error
//! [`source`](std::error::Error::source).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use bridge_domain::{DomainError, ProjectId, Result};
use url::Url;

/// The top-level TOML table that holds the configured projects.
pub const PROJECTS_TABLE: &str = "projects";

/// The per-project key that names the project workspace directory.
pub const WORKSPACE_KEY: &str = "workspace";

/// The required per-project key that names the OpenCode server endpoint.
pub const OPENCODE_URL_KEY: &str = "opencode_url";

/// The optional per-project key that names the MCP endpoint.
pub const MCP_URL_KEY: &str = "mcp_url";

/// The required per-project key that bounds the number of worker rounds.
pub const MAX_ROUNDS_KEY: &str = "max_rounds";

/// The optional per-project key that selects the OpenCode model.
pub const OPENCODE_MODEL_KEY: &str = "opencode_model";

/// The optional per-project key that names the OpenCode env file.
pub const OPENCODE_ENV_FILE_KEY: &str = "opencode_env_file";

/// The optional per-project key that lists ordinary permissions to auto-approve.
pub const AUTO_APPROVE_PERMISSIONS_KEY: &str = "auto_approve_permissions";

/// The reserved permission name that must go through the trusted-directory key.
const EXTERNAL_DIRECTORY_PERMISSION: &str = "external_directory";

/// The per-project key that names the OpenCode password file.
const PASSWORD_FILE_KEY: &str = "password_file";

/// The optional per-project key that names the MCP bearer-token file.
const MCP_TOKEN_FILE_KEY: &str = "mcp_token_file";

/// The only host accepted in configured endpoints.
pub const LOOPBACK_HOST: &str = "127.0.0.1";

/// The fixed path suffix of a configured MCP endpoint.
pub const MCP_PATH: &str = "/mcp";

/// A validated OpenCode server endpoint.
///
/// The host is always [`LOOPBACK_HOST`] and the port is always explicit and in
/// `1..=65535`, so a consumer never has to re-parse `opencode_url`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    port: u16,
}

impl Endpoint {
    /// Returns the explicit port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Returns the endpoint host, which is always [`LOOPBACK_HOST`].
    #[must_use]
    pub fn host(&self) -> &'static str {
        LOOPBACK_HOST
    }

    /// Returns the normalized `http://127.0.0.1:<port>` URL.
    #[must_use]
    pub fn url(&self) -> String {
        let port = self.port;
        format!("http://{LOOPBACK_HOST}:{port}")
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url())
    }
}

/// A validated MCP endpoint.
///
/// It wraps the base [`Endpoint`] and always renders with the fixed
/// [`MCP_PATH`] suffix. Keeping the base typed (rather than the raw string)
/// lets later consumers compare endpoints and read the port without parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpEndpoint {
    base: Endpoint,
}

impl McpEndpoint {
    /// Returns the explicit port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.base.port()
    }

    /// Returns the base endpoint without the `/mcp` suffix.
    #[must_use]
    pub fn base(&self) -> &Endpoint {
        &self.base
    }

    /// Returns the normalized `http://127.0.0.1:<port>/mcp` URL.
    #[must_use]
    pub fn url(&self) -> String {
        let base = self.base.url();
        format!("{base}{MCP_PATH}")
    }
}

impl fmt::Display for McpEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url())
    }
}

/// A validated optional OpenCode model selector.
///
/// The raw `opencode_model` value must be `'<providerID>/<modelID>'` and is
/// split on the **first** `/` only, so a model id may itself contain further
/// slashes. The provider and model components are stored without surrounding
/// whitespace and are never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeModel {
    provider: String,
    model: String,
}

impl OpenCodeModel {
    /// Returns the provider id before the first `/`.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the model id after the first `/`, which may contain `/` itself.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// A validated project entry.
///
/// The entry carries the typed, validated [`ProjectId`], the canonical,
/// absolute workspace [`Path`], the typed [`Endpoint`]/[`McpEndpoint`] values,
/// the positive `max_rounds`, the optional [`OpenCodeModel`], the optional
/// resolved `opencode_env_file` [`Path`] and the deduplicated
/// `auto_approve_permissions` list, so consumers never have to repeat the
/// task 2.2–2.6 validation or re-parse raw values. The raw TOML table is
/// preserved verbatim for the later validation groups (2.7–2.9), which inspect
/// every key and value.
#[derive(Clone)]
pub struct ProjectEntry {
    id: ProjectId,
    workspace: PathBuf,
    opencode_endpoint: Endpoint,
    mcp_endpoint: Option<McpEndpoint>,
    max_rounds: u64,
    opencode_model: Option<OpenCodeModel>,
    opencode_env_file: Option<PathBuf>,
    auto_approve_permissions: Vec<String>,
    values: toml::Table,
}

impl ProjectEntry {
    /// Returns the validated project id.
    #[must_use]
    pub fn id(&self) -> &ProjectId {
        &self.id
    }

    /// Returns the canonical, absolute workspace directory.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Returns the validated OpenCode server endpoint.
    #[must_use]
    pub fn opencode_endpoint(&self) -> &Endpoint {
        &self.opencode_endpoint
    }

    /// Returns the validated optional MCP endpoint.
    #[must_use]
    pub fn mcp_endpoint(&self) -> Option<&McpEndpoint> {
        self.mcp_endpoint.as_ref()
    }

    /// Returns the required positive `max_rounds`.
    #[must_use]
    pub fn max_rounds(&self) -> u64 {
        self.max_rounds
    }

    /// Returns the validated optional OpenCode model.
    #[must_use]
    pub fn opencode_model(&self) -> Option<&OpenCodeModel> {
        self.opencode_model.as_ref()
    }

    /// Returns the optional resolved `opencode_env_file` path.
    ///
    /// A relative configured path is resolved against the directory that
    /// contains `projects.toml`; an absolute path is preserved verbatim.
    #[must_use]
    pub fn opencode_env_file(&self) -> Option<&Path> {
        self.opencode_env_file.as_deref()
    }

    /// Returns the auto-approved ordinary permission names.
    ///
    /// The slice is empty when the key is absent. Duplicate names are collapsed
    /// while preserving the order of their first occurrence, and the reserved
    /// `external_directory` name never appears.
    #[must_use]
    pub fn auto_approve_permissions(&self) -> &[String] {
        &self.auto_approve_permissions
    }

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
            .field("id", &self.id)
            .field("keys", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// A successfully loaded and validated `projects.toml` configuration.
///
/// The container preserves the raw project tables keyed by their validated
/// project id. Iteration order is deterministic (lexicographic by id) because
/// both the backing [`BTreeMap`] and the default TOML map sort their keys.
#[derive(Clone)]
pub struct Config {
    projects: BTreeMap<String, ProjectEntry>,
}

impl Config {
    /// Returns all validated projects keyed by their project id.
    #[must_use]
    pub fn projects(&self) -> &BTreeMap<String, ProjectEntry> {
        &self.projects
    }

    /// Returns the validated entry for `id`, if present.
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

/// Loads and validates `projects.toml` from the explicit `path`.
///
/// Relative workspaces are resolved against the directory that contains
/// `path`. The returned [`Config`] never contains an unvalidated project id or
/// workspace.
///
/// # Errors
///
/// * [`bridge_domain::ErrorKind::NotFound`] when the file does not exist or a
///   configured workspace path does not exist;
/// * [`bridge_domain::ErrorKind::PermissionDenied`] when the file or a
///   workspace cannot be accessed because of permissions;
/// * [`bridge_domain::ErrorKind::Internal`] for any other I/O failure,
///   including an unresolvable symlink (a loop or an unreadable link) in a
///   credential path;
/// * [`bridge_domain::ErrorKind::InvalidInput`] when the bytes are not UTF-8,
///   when the TOML is syntactically invalid, when the `projects` table is
///   missing, when the `projects` value or a project entry has the wrong
///   shape, when a project id does not match
///   `^[a-z0-9][a-z0-9_-]{0,63}$`, when a workspace is missing, is not a
///   string, is empty or is not a directory, when `opencode_url`/`mcp_url`
///   are missing, have the wrong type or are not a loopback `http` endpoint
///   with an explicit port in `1..=65535` and the required path, when
///   `max_rounds` is missing, is not a TOML integer or is not positive, when
///   `opencode_model` is not a string, is not `'<providerID>/<modelID>'` or has
///   surrounding whitespace around the value or a component, when
///   `opencode_env_file` is not a non-empty string, when
///   `auto_approve_permissions` is not a TOML array of non-empty strings without
///   surrounding whitespace or contains the reserved `external_directory` name,
///   when an MCP token file is not a non-empty string, or when a canonical
///   workspace, a server endpoint or an MCP token file is reused across projects
///   or an MCP token file equals a password file.
///
/// None of these errors renders the file contents, credential values, project
/// ids, workspace paths, URL inputs or the absolute input path.
pub fn load_config(path: &Path) -> Result<Config> {
    let bytes = std::fs::read(path).map_err(read_error)?;
    let text = String::from_utf8(bytes).map_err(|source| {
        DomainError::invalid_input("configuration file is not valid UTF-8").with_source(source)
    })?;
    let projects = parse_projects(&text)?;
    let config_dir = path.parent().unwrap_or_else(|| Path::new(""));
    validate_projects(projects, config_dir)
}

/// Structurally parses already-read TOML text into the raw per-project tables.
///
/// This helper is intentionally separate from the path-aware validation: it
/// only checks the `projects` table and the shape of its entries, because the
/// workspace resolution needs the config file directory. The public
/// [`load_config`] always runs [`validate_projects`] afterwards.
fn parse_projects(text: &str) -> Result<BTreeMap<String, toml::Table>> {
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
        entries.insert(id.clone(), values.clone());
    }

    Ok(entries)
}

/// Validates the raw project tables and builds the public [`Config`].
///
/// Each project is validated independently first (tasks 2.2/2.3/2.5/2.6). Only
/// then are the cross-project uniqueness rules of task 2.4 applied: a canonical
/// workspace may not be bound to two projects, a server endpoint may not be
/// reused (neither between two projects nor by the OpenCode and MCP endpoints
/// of the same project, because both use the loopback host and are compared by
/// port), and an MCP token file may not repeat another token file or coincide
/// with a password file.
///
/// The credential paths are resolved like the reference implementation: an
/// absolute path is used as-is and a relative path is joined onto `config_dir`.
/// The file itself is not required to exist and its contents are never read;
/// only the normalized path is compared. The optional `opencode_env_file` is
/// resolved the same lexical way (absolute preserved, relative joined onto
/// `config_dir`) but is not read. Errors are static and never render a project
/// id, workspace, URL, credential path, model value, permission value or config
/// path.
fn validate_projects(raw: BTreeMap<String, toml::Table>, config_dir: &Path) -> Result<Config> {
    let mut projects = BTreeMap::new();
    let mut workspaces: BTreeSet<PathBuf> = BTreeSet::new();
    let mut ports: BTreeSet<u16> = BTreeSet::new();
    let mut credentials: Vec<(Option<PathBuf>, Option<PathBuf>)> = Vec::new();

    for (raw_id, values) in raw {
        let id = validate_project_id(&raw_id)?;
        let workspace = validate_workspace(&values, config_dir)?;
        let opencode_endpoint = validate_opencode_url(&values)?;
        let mcp_endpoint = validate_mcp_url(&values)?;

        if !workspaces.insert(workspace.clone()) {
            return Err(DomainError::invalid_input(
                "project workspace is already used by another project",
            ));
        }
        if !ports.insert(opencode_endpoint.port()) {
            return Err(duplicate_endpoint_error());
        }
        if let Some(mcp) = &mcp_endpoint
            && !ports.insert(mcp.port())
        {
            return Err(duplicate_endpoint_error());
        }

        let max_rounds = validate_max_rounds(&values)?;
        let opencode_model = validate_opencode_model(&values)?;
        let opencode_env_file = validate_opencode_env_file(&values, config_dir)?;
        let auto_approve_permissions = validate_auto_approve_permissions(&values)?;

        let password = resolve_password_file(&values, config_dir)?;
        let token = resolve_mcp_token_file(&values, config_dir)?;
        credentials.push((password, token));

        projects.insert(
            raw_id,
            ProjectEntry {
                id,
                workspace,
                opencode_endpoint,
                mcp_endpoint,
                max_rounds,
                opencode_model,
                opencode_env_file,
                auto_approve_permissions,
                values,
            },
        );
    }

    validate_credentials(&credentials)?;

    Ok(Config { projects })
}

/// The static error for a reused server endpoint.
fn duplicate_endpoint_error() -> DomainError {
    DomainError::invalid_input("project endpoint is already used by another project")
}

/// Resolves the optional `password_file` into a normalized path.
///
/// The key is optional here because the required-key rule belongs to a later
/// task; when it is absent or not a string this helper simply reports no
/// password path. The value is never rendered. Resolution is fallible because a
/// cyclic or unreadable symlink in the path is rejected safely.
fn resolve_password_file(values: &toml::Table, config_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(value) = values.get(PASSWORD_FILE_KEY) else {
        return Ok(None);
    };
    let Some(raw) = value.as_str() else {
        return Ok(None);
    };
    Ok(Some(resolve_credential_path(raw, config_dir)?))
}

/// Resolves the optional `mcp_token_file` into a normalized path.
///
/// The token must be a non-empty string when present. Its contents are never
/// read; only the normalized path participates in the uniqueness rules.
/// Resolution is fallible because a cyclic or unreadable symlink in the path is
/// rejected safely.
fn resolve_mcp_token_file(values: &toml::Table, config_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(value) = values.get(MCP_TOKEN_FILE_KEY) else {
        return Ok(None);
    };
    let raw = value
        .as_str()
        .ok_or_else(|| DomainError::invalid_input("project mcp_token_file must be a string"))?;
    if raw.is_empty() {
        return Err(DomainError::invalid_input(
            "project mcp_token_file must not be empty",
        ));
    }
    Ok(Some(resolve_credential_path(raw, config_dir)?))
}

/// Applies the credential-file uniqueness rules across all projects.
///
/// An MCP token file must be separate from every project password file and may
/// not be shared by two projects. Password files are intentionally not required
/// to be unique: the reference implementation only collects them to guard the
/// token rule.
fn validate_credentials(credentials: &[(Option<PathBuf>, Option<PathBuf>)]) -> Result<()> {
    let passwords: BTreeSet<&PathBuf> = credentials
        .iter()
        .filter_map(|(password, _)| password.as_ref())
        .collect();
    let mut tokens: BTreeSet<&PathBuf> = BTreeSet::new();

    for (password, token) in credentials {
        let Some(token) = token else {
            continue;
        };
        if password.as_ref() == Some(token) || passwords.contains(token) {
            return Err(DomainError::invalid_input(
                "project mcp_token_file must differ from the project password_file",
            ));
        }
        if !tokens.insert(token) {
            return Err(DomainError::invalid_input(
                "project mcp_token_file is already used by another project",
            ));
        }
    }

    Ok(())
}

/// The maximum number of symlink hops resolved before giving up.
///
/// Mirrors the kernel's `ELOOP` guard so a cyclic symlink cannot make the
/// loader loop forever. Exceeding it is a safe error, matching the reference
/// implementation where `Path.resolve(strict=False)` raises on a symlink loop
/// instead of returning a lexical path; the loader must never fail open.
const MAX_SYMLINK_HOPS: usize = 40;

/// Resolves a credential file path against `config_dir` for comparison.
///
/// An absolute path is used as-is; a relative path is joined onto the config
/// directory. The result mirrors the non-strict `Path.resolve()` of the
/// reference implementation: symlinks in every existing prefix component are
/// expanded even when the final file is missing, and `.`/`..` are normalized
/// without ever escaping the root. The path is never required to exist and its
/// contents are never read.
///
/// # Errors
///
/// Returns a safe [`DomainError`] when a symlink in the path cannot be read or
/// the walk exceeds [`MAX_SYMLINK_HOPS`]. The message never renders the path;
/// the underlying I/O diagnostic is retained only as the error source.
fn resolve_credential_path(raw: &str, config_dir: &Path) -> Result<PathBuf> {
    let candidate = Path::new(raw);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        config_dir.join(candidate)
    };

    resolve_non_strict(&absolute)
}

/// Expands existing symlinks and normalizes a possibly missing tail.
///
/// This is the Rust equivalent of Python's `Path.resolve(strict=False)`. Each
/// component is appended to the result; when the appended component is an
/// existing symlink, its target replaces it and the walk restarts for the
/// target while keeping the pending tail. A `..` pops the previously resolved
/// component, and at the root it is discarded so an absolute path can never
/// become relative. A symlink cycle is bounded by [`MAX_SYMLINK_HOPS`], and
/// either exceeding it or failing to read a detected symlink is rejected as a
/// safe error rather than silently falling back to a lexical path.
fn resolve_non_strict(path: &Path) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut pending: VecDeque<OsString> = path
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect();
    let mut hops = 0usize;

    while let Some(name) = pending.pop_front() {
        match Path::new(&name).components().next() {
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                if !resolved.pop() && !resolved.is_absolute() {
                    resolved.push(Component::ParentDir.as_os_str());
                }
            }
            Some(Component::RootDir) => resolved.push(Component::RootDir.as_os_str()),
            Some(Component::Prefix(prefix)) => resolved.push(prefix.as_os_str()),
            _ => {
                resolved.push(&name);
                let is_symlink = std::fs::symlink_metadata(&resolved)
                    .map(|metadata| metadata.file_type().is_symlink())
                    .unwrap_or(false);
                if is_symlink {
                    if hops >= MAX_SYMLINK_HOPS {
                        return Err(DomainError::internal(
                            "project credential file path could not be resolved",
                        )
                        .with_source(std::io::Error::other("symlink loop detected")));
                    }
                    hops += 1;
                    let target = std::fs::read_link(&resolved).map_err(credential_path_error)?;
                    resolved.pop();
                    let target_is_absolute = target.is_absolute();
                    let target_components: Vec<OsString> = target
                        .components()
                        .map(|component| component.as_os_str().to_os_string())
                        .collect();
                    for component in target_components.into_iter().rev() {
                        pending.push_front(component);
                    }
                    if target_is_absolute {
                        resolved.clear();
                    }
                }
            }
        }
    }

    Ok(resolved)
}

/// Validates a raw project id against `^[a-z0-9][a-z0-9_-]{0,63}$`.
///
/// The error message is static and never contains the rejected id.
fn validate_project_id(raw: &str) -> Result<ProjectId> {
    if !is_valid_project_id(raw) {
        return Err(DomainError::invalid_input("project id is invalid"));
    }
    raw.parse::<ProjectId>()
        .map_err(|_| DomainError::invalid_input("project id is invalid"))
}

/// Returns `true` when `id` matches `^[a-z0-9][a-z0-9_-]{0,63}$`.
///
/// The rule is applied on ASCII bytes, so Unicode, uppercase letters, dots and
/// whitespace are rejected. The length is 1..=64 bytes, which is equivalent to
/// characters because every accepted byte is ASCII.
fn is_valid_project_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'-'
    })
}

/// Validates the required, non-empty string `workspace` and resolves it.
///
/// The error messages are static and never contain the workspace value.
fn validate_workspace(values: &toml::Table, config_dir: &Path) -> Result<PathBuf> {
    let raw = values
        .get(WORKSPACE_KEY)
        .ok_or_else(|| DomainError::invalid_input("project workspace is missing"))?
        .as_str()
        .ok_or_else(|| DomainError::invalid_input("project workspace must be a string"))?;

    if raw.is_empty() {
        return Err(DomainError::invalid_input(
            "project workspace must not be empty",
        ));
    }

    resolve_workspace(raw, config_dir)
}

/// Resolves `raw` to a canonical, absolute directory.
///
/// Absolute paths are used as-is; relative paths are joined onto `config_dir`,
/// the directory of the specific `projects.toml`. The result must exist and be
/// a directory; [`std::fs::canonicalize`] makes it canonical and absolute,
/// which also resolves symlink aliases.
fn resolve_workspace(raw: &str, config_dir: &Path) -> Result<PathBuf> {
    let candidate = Path::new(raw);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        config_dir.join(candidate)
    };

    let canonical = std::fs::canonicalize(&absolute).map_err(workspace_error)?;
    let metadata = std::fs::metadata(&canonical).map_err(workspace_error)?;
    if !metadata.is_dir() {
        return Err(DomainError::invalid_input(
            "project workspace is not a directory",
        ));
    }

    Ok(canonical)
}

/// The reason a URL is not an acceptable loopback endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointError {
    /// The input is not a syntactically valid absolute URL.
    NotAUrl,
    /// The scheme is not `http`.
    Scheme,
    /// The host is not exactly `127.0.0.1`.
    Host,
    /// The URL embeds a username or password.
    Credentials,
    /// The URL carries a non-empty query or fragment.
    QueryOrFragment,
    /// The URL carries a path other than empty or `/`.
    Path,
    /// The URL omits the explicit port.
    MissingPort,
    /// The port is outside `1..=65535`.
    PortRange,
}

/// A failed endpoint parse, optionally carrying the parser diagnostic.
struct EndpointFailure {
    error: EndpointError,
    source: Option<url::ParseError>,
}

impl EndpointFailure {
    const fn new(error: EndpointError) -> Self {
        Self {
            error,
            source: None,
        }
    }
}

/// Which configured key is being validated, used to pick static messages.
#[derive(Debug, Clone, Copy)]
enum EndpointKey {
    Opencode,
    Mcp,
}

impl EndpointKey {
    /// Maps a failure to the static, non-sensitive message for this key.
    const fn message(self, error: EndpointError) -> &'static str {
        match (self, error) {
            (Self::Opencode, EndpointError::NotAUrl) => "project opencode_url is not a valid URL",
            (Self::Opencode, EndpointError::Scheme) => {
                "project opencode_url must use the http scheme"
            }
            (Self::Opencode, EndpointError::Host) => {
                "project opencode_url host must be exactly 127.0.0.1"
            }
            (Self::Opencode, EndpointError::Credentials) => {
                "project opencode_url must not embed credentials"
            }
            (Self::Opencode, EndpointError::QueryOrFragment) => {
                "project opencode_url must not contain a query or fragment"
            }
            (Self::Opencode, EndpointError::Path) => "project opencode_url must not contain a path",
            (Self::Opencode, EndpointError::MissingPort) => {
                "project opencode_url must include an explicit port"
            }
            (Self::Opencode, EndpointError::PortRange) => {
                "project opencode_url port must be within 1..65535"
            }
            (Self::Mcp, EndpointError::NotAUrl) => "project mcp_url is not a valid URL",
            (Self::Mcp, EndpointError::Scheme) => "project mcp_url must use the http scheme",
            (Self::Mcp, EndpointError::Host) => "project mcp_url host must be exactly 127.0.0.1",
            (Self::Mcp, EndpointError::Credentials) => "project mcp_url must not embed credentials",
            (Self::Mcp, EndpointError::QueryOrFragment) => {
                "project mcp_url must not contain a query or fragment"
            }
            (Self::Mcp, EndpointError::Path) => "project mcp_url must not contain a path",
            (Self::Mcp, EndpointError::MissingPort) => {
                "project mcp_url must include an explicit port"
            }
            (Self::Mcp, EndpointError::PortRange) => "project mcp_url port must be within 1..65535",
        }
    }
}

/// Builds a safe [`DomainError`] from an endpoint failure.
///
/// Only the static message is rendered; the parser diagnostic, when present, is
/// attached solely as the error [`source`](std::error::Error::source).
fn endpoint_error(key: EndpointKey, failure: EndpointFailure) -> DomainError {
    let error = DomainError::invalid_input(key.message(failure.error));
    match failure.source {
        Some(source) => error.with_source(source),
        None => error,
    }
}

/// The original URL components that the endpoint contract inspects.
///
/// [`Url`] is used only to reject syntactically invalid input. Its WHATWG parser
/// normalizes the default port away, canonicalizes non-standard IPv4 spellings
/// and folds dot segments, so the contract checks must run on the original
/// spelling instead. This narrow extractor mirrors the component split of
/// Python's `urllib.parse.urlsplit` for exactly the pieces the contract needs;
/// it is deliberately not a general-purpose URL parser.
struct RawEndpoint<'a> {
    /// The authority host exactly as written, before any normalization.
    host: &'a str,
    /// The explicit port text exactly as written, if any.
    port: Option<&'a str>,
    /// `true` when the authority carries a non-empty `user:pass@` prefix.
    has_credentials: bool,
    /// The raw query component, if the `?` delimiter is present.
    query: Option<&'a str>,
    /// The raw fragment component, if the `#` delimiter is present.
    fragment: Option<&'a str>,
    /// The path exactly as written, without the query or fragment.
    path: &'a str,
}

/// Splits a URL string that [`Url::parse`] already accepted into its original
/// components.
///
/// Returns `None` when the string does not have the `scheme://authority`
/// structure the contract relies on. Because the caller only reaches this after
/// a successful [`Url::parse`], that can only happen for inputs the WHATWG
/// parser accepted in a non-`urlsplit` shape, which the contract rejects.
fn split_raw_endpoint(raw: &str) -> Option<RawEndpoint<'_>> {
    let (_scheme, rest) = raw.split_once(':')?;

    let (before_fragment, fragment) = match rest.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (rest, None),
    };
    let (before_query, query) = match before_fragment.split_once('?') {
        Some((before, query)) => (before, Some(query)),
        None => (before_fragment, None),
    };

    let after_scheme_slashes = before_query.strip_prefix("//")?;
    let (authority, path) = match after_scheme_slashes.find('/') {
        Some(index) => (
            after_scheme_slashes.get(..index)?,
            after_scheme_slashes.get(index..)?,
        ),
        None => (after_scheme_slashes, ""),
    };

    let (userinfo, hostinfo) = match authority.rfind('@') {
        Some(index) => (authority.get(..index), authority.get(index + 1..)?),
        None => (None, authority),
    };
    let has_credentials = userinfo.is_some_and(|info| !info.is_empty());

    let (host, port) = match hostinfo.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (hostinfo, None),
    };

    Some(RawEndpoint {
        host,
        port,
        has_credentials,
        query,
        fragment,
        path,
    })
}

/// Parses the explicit port text as a decimal integer in `1..=65535`.
///
/// Returns `None` for empty, non-decimal or out-of-range text. The contract
/// calls for a decimal port, so no sign, whitespace or separator is accepted.
fn parse_explicit_port(raw: &str) -> Option<u16> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match raw.parse::<u16>() {
        Ok(port) if port != 0 => Some(port),
        _ => None,
    }
}

/// Parses and validates a loopback HTTP endpoint URL.
///
/// The rules mirror `_parse_endpoint` in the reference Python implementation:
/// `http` scheme, exact raw host `127.0.0.1`, no username/password, no query or
/// fragment, path empty or `/`, and an explicit raw port in `1..=65535`.
///
/// [`Url::parse`] provides the general syntactic check, but the
/// contract-sensitive host, port and path are read from the original string via
/// [`split_raw_endpoint`] so WHATWG normalization cannot widen the contract:
/// `:80` keeps its port, `127.1` is not silently rewritten to `127.0.0.1`, and
/// `/./`, `/a/..` or `/%2e%2e` are not folded to `/`.
fn parse_endpoint(raw: &str) -> std::result::Result<Endpoint, EndpointFailure> {
    let url = Url::parse(raw).map_err(|source| EndpointFailure {
        error: match source {
            url::ParseError::InvalidPort => EndpointError::PortRange,
            _ => EndpointError::NotAUrl,
        },
        source: Some(source),
    })?;

    if url.scheme() != "http" {
        return Err(EndpointFailure::new(EndpointError::Scheme));
    }

    let Some(parts) = split_raw_endpoint(raw) else {
        return Err(EndpointFailure::new(EndpointError::NotAUrl));
    };

    if parts.host != LOOPBACK_HOST {
        return Err(EndpointFailure::new(EndpointError::Host));
    }
    if parts.has_credentials {
        return Err(EndpointFailure::new(EndpointError::Credentials));
    }
    let has_query = parts.query.is_some_and(|query| !query.is_empty());
    let has_fragment = parts.fragment.is_some_and(|fragment| !fragment.is_empty());
    if has_query || has_fragment {
        return Err(EndpointFailure::new(EndpointError::QueryOrFragment));
    }
    if !matches!(parts.path, "" | "/") {
        return Err(EndpointFailure::new(EndpointError::Path));
    }
    let Some(port_raw) = parts.port else {
        return Err(EndpointFailure::new(EndpointError::MissingPort));
    };
    let Some(port) = parse_explicit_port(port_raw) else {
        return Err(EndpointFailure::new(EndpointError::PortRange));
    };

    Ok(Endpoint { port })
}

/// Validates the required `opencode_url` into a typed [`Endpoint`].
///
/// The error messages are static and never contain the URL input.
fn validate_opencode_url(values: &toml::Table) -> Result<Endpoint> {
    let raw = values
        .get(OPENCODE_URL_KEY)
        .ok_or_else(|| DomainError::invalid_input("project opencode_url is missing"))?
        .as_str()
        .ok_or_else(|| DomainError::invalid_input("project opencode_url must be a string"))?;

    parse_endpoint(raw).map_err(|failure| endpoint_error(EndpointKey::Opencode, failure))
}

/// Validates the optional `mcp_url` into a typed [`McpEndpoint`].
///
/// The value must be a string ending with [`MCP_PATH`]; the base URL before the
/// suffix obeys exactly the same rules as `opencode_url`. Pairing with
/// `mcp_token_file` and the token-file path rules are later tasks, so they are
/// intentionally not checked here.
fn validate_mcp_url(values: &toml::Table) -> Result<Option<McpEndpoint>> {
    let Some(value) = values.get(MCP_URL_KEY) else {
        return Ok(None);
    };
    let raw = value
        .as_str()
        .ok_or_else(|| DomainError::invalid_input("project mcp_url must be a string"))?;
    let base_raw = raw
        .strip_suffix(MCP_PATH)
        .ok_or_else(|| DomainError::invalid_input("project mcp_url must end with '/mcp'"))?;

    let base =
        parse_endpoint(base_raw).map_err(|failure| endpoint_error(EndpointKey::Mcp, failure))?;
    Ok(Some(McpEndpoint { base }))
}

/// Validates the required positive integer `max_rounds`.
///
/// The value must be present and a TOML integer. TOML booleans and all
/// non-integer values (strings, floats, arrays, tables) are rejected because
/// [`toml::Value::as_integer`] only succeeds for an integer. The integer must be
/// positive; zero and negatives are rejected. The conversion to [`u64`] is
/// fallible and uses [`u64::try_from`] so an unexpected negative or oversized
/// value can never wrap into a valid count. The error messages are static and
/// never contain the supplied value.
fn validate_max_rounds(values: &toml::Table) -> Result<u64> {
    let value = values
        .get(MAX_ROUNDS_KEY)
        .ok_or_else(|| DomainError::invalid_input("project max_rounds is missing"))?;
    let raw = value.as_integer().ok_or_else(|| {
        DomainError::invalid_input("project max_rounds must be a positive integer")
    })?;
    let rounds = u64::try_from(raw)
        .map_err(|_| DomainError::invalid_input("project max_rounds must be a positive integer"))?;
    if rounds == 0 {
        return Err(DomainError::invalid_input(
            "project max_rounds must be a positive integer",
        ));
    }
    Ok(rounds)
}

/// Parses the optional `opencode_model` into a typed [`OpenCodeModel`].
///
/// The key is optional; when absent the entry keeps no model and the bridge
/// sends no model field. When present the value must be a string, must not have
/// surrounding whitespace, must be `'<providerID>/<modelID>'` split on the
/// first `/` only, and neither component may be empty or carry surrounding
/// whitespace. The error messages are static and never contain the supplied
/// value.
fn validate_opencode_model(values: &toml::Table) -> Result<Option<OpenCodeModel>> {
    let Some(value) = values.get(OPENCODE_MODEL_KEY) else {
        return Ok(None);
    };
    let raw = value
        .as_str()
        .ok_or_else(|| DomainError::invalid_input("project opencode_model must be a string"))?;
    parse_opencode_model(raw).map(Some)
}

/// Splits and validates a raw `'<providerID>/<modelID>'` model selector.
///
/// Mirrors the reference implementation: the whole value and each component
/// must be free of surrounding whitespace, and a missing separator or an empty
/// provider/model is rejected. Only the first `/` separates the components, so
/// a model id may itself contain `/`.
fn parse_opencode_model(raw: &str) -> Result<OpenCodeModel> {
    if raw.trim() != raw {
        return Err(DomainError::invalid_input(
            "project opencode_model must not have surrounding whitespace",
        ));
    }
    let Some((provider, model)) = raw.split_once('/') else {
        return Err(DomainError::invalid_input(
            "project opencode_model must be '<providerID>/<modelID>'",
        ));
    };
    if provider.is_empty() || model.is_empty() {
        return Err(DomainError::invalid_input(
            "project opencode_model must be '<providerID>/<modelID>'",
        ));
    }
    if provider.trim() != provider || model.trim() != model {
        return Err(DomainError::invalid_input(
            "project opencode_model components must not have surrounding whitespace",
        ));
    }
    Ok(OpenCodeModel {
        provider: provider.to_owned(),
        model: model.to_owned(),
    })
}

/// Validates and resolves the optional `opencode_env_file` into a path.
///
/// The key is optional; when present the value must be a non-empty string. An
/// absolute path is preserved verbatim and a relative path is joined onto
/// `config_dir`, the directory that contains the specific `projects.toml`. The
/// path is resolved lexically (it is not required to exist, its contents are
/// never read and no symlink resolution is performed), matching the documented
/// contract. The error message is static and never contains the supplied value.
fn validate_opencode_env_file(values: &toml::Table, config_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(value) = values.get(OPENCODE_ENV_FILE_KEY) else {
        return Ok(None);
    };
    let raw = value.as_str().ok_or_else(|| {
        DomainError::invalid_input("project opencode_env_file must be a non-empty string")
    })?;
    if raw.is_empty() {
        return Err(DomainError::invalid_input(
            "project opencode_env_file must be a non-empty string",
        ));
    }

    let candidate = Path::new(raw);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        config_dir.join(candidate)
    };
    Ok(Some(resolved))
}

/// Validates the optional `auto_approve_permissions` TOML array.
///
/// The key is optional; when absent the project auto-approves no ordinary
/// permissions and an empty list is returned. When present the value must be a
/// TOML array, and every entry must be a string that is neither empty nor
/// whitespace-only and carries no surrounding whitespace. The reserved
/// `external_directory` name is rejected because it is controlled solely by
/// `auto_approve_external_directories`, so a plain permission name can never
/// bypass the trusted-root check. Duplicates collapse while preserving the order
/// of their first occurrence. All error messages are static and never contain a
/// project id, a supplied permission value, config contents or a path.
fn validate_auto_approve_permissions(values: &toml::Table) -> Result<Vec<String>> {
    let Some(value) = values.get(AUTO_APPROVE_PERMISSIONS_KEY) else {
        return Ok(Vec::new());
    };
    let array = value.as_array().ok_or_else(|| {
        DomainError::invalid_input("project auto_approve_permissions must be a list of strings")
    })?;

    let mut permissions: Vec<String> = Vec::new();
    for item in array {
        let raw = item.as_str().ok_or_else(|| {
            DomainError::invalid_input(
                "project auto_approve_permissions entries must be non-empty strings",
            )
        })?;
        if raw.trim().is_empty() {
            return Err(DomainError::invalid_input(
                "project auto_approve_permissions entries must be non-empty strings",
            ));
        }
        if raw.trim() != raw {
            return Err(DomainError::invalid_input(
                "project auto_approve_permissions entries must not have surrounding whitespace",
            ));
        }
        if raw == EXTERNAL_DIRECTORY_PERMISSION {
            return Err(DomainError::invalid_input(
                "project external_directory is not accepted in auto_approve_permissions; use auto_approve_external_directories instead",
            ));
        }
        if !permissions.iter().any(|existing| existing == raw) {
            permissions.push(raw.to_owned());
        }
    }
    Ok(permissions)
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

/// Maps a workspace resolution failure to a safe, typed [`DomainError`].
fn workspace_error(source: std::io::Error) -> DomainError {
    match source.kind() {
        std::io::ErrorKind::NotFound => {
            DomainError::not_found("project workspace does not exist").with_source(source)
        }
        std::io::ErrorKind::PermissionDenied => {
            DomainError::permission_denied("project workspace could not be accessed")
                .with_source(source)
        }
        _ => DomainError::internal("project workspace could not be resolved").with_source(source),
    }
}

/// Maps a credential-path resolution failure to a safe, typed [`DomainError`].
///
/// Any I/O failure while walking a detected symlink is fail-closed. The message
/// never renders the credential path; the underlying [`std::io::Error`] is kept
/// only as the error source, so its path-bearing
/// [`Display`](std::fmt::Display) text never reaches the safe output.
fn credential_path_error(source: std::io::Error) -> DomainError {
    match source.kind() {
        std::io::ErrorKind::NotFound => {
            DomainError::not_found("project credential file path does not exist")
                .with_source(source)
        }
        std::io::ErrorKind::PermissionDenied => {
            DomainError::permission_denied("project credential file path could not be accessed")
                .with_source(source)
        }
        _ => DomainError::internal("project credential file path could not be resolved")
            .with_source(source),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AUTO_APPROVE_PERMISSIONS_KEY, Config, MAX_ROUNDS_KEY, MCP_URL_KEY, OPENCODE_ENV_FILE_KEY,
        OPENCODE_MODEL_KEY, OPENCODE_URL_KEY, PROJECTS_TABLE, WORKSPACE_KEY, load_config,
        parse_projects,
    };
    use bridge_domain::{DomainError, ErrorKind, ProjectId};
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Corpus case `valid-minimal-project`: required keys only.
    const MINIMAL: &str = "[projects.proj]\nworkspace = \"ws\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = 3\n";

    /// A temporary directory removed recursively on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock must be after the Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "bridge-config-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("temporary directory must be creatable");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn mkdir(&self, name: &str) -> PathBuf {
            let path = self.path.join(name);
            std::fs::create_dir_all(&path).expect("temporary subdirectory must be creatable");
            path
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.path.join(name);
            std::fs::write(&path, text).expect("temporary config must be writable");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Builds a minimal project table with a quoted id key and `workspace`.
    fn project_toml(id: &str, workspace: &str) -> String {
        format!(
            "[projects.\"{id}\"]\nworkspace = \"{workspace}\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = 3\n"
        )
    }

    /// Builds a project table with an explicit `opencode_url` and extra keys.
    fn project_toml_with(id: &str, workspace: &str, opencode_url: &str, extra: &str) -> String {
        format!(
            "[projects.\"{id}\"]\nworkspace = \"{workspace}\"\nopencode_url = \"{opencode_url}\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = 3\n{extra}"
        )
    }

    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).expect("path must canonicalize")
    }

    /// Loads a one-project config with the given `opencode_url` and extra keys,
    /// returning the validation error and panicking if it unexpectedly loads.
    fn load_endpoint_config(id: &str, opencode_url: &str, extra: &str) -> DomainError {
        let dir = TempDir::new("endpoint-error");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                id,
                workspace.to_str().expect("utf-8 path"),
                opencode_url,
                extra,
            ),
        );
        load_config(&config_path).expect_err("config must be rejected")
    }

    #[test]
    fn crate_constants_name_the_projects_table_and_workspace_key() {
        assert_eq!(PROJECTS_TABLE, "projects");
        assert_eq!(WORKSPACE_KEY, "workspace");
    }

    // Corpus case `valid-minimal-project` (structural view).
    #[test]
    fn parses_minimal_project_structure() {
        let projects = parse_projects(MINIMAL).expect("minimal config must parse");

        assert_eq!(projects.len(), 1);
        let values = projects.get("proj").expect("project 'proj' must exist");
        assert_eq!(
            values.get("workspace").and_then(toml::Value::as_str),
            Some("ws")
        );
        assert_eq!(
            values.get("opencode_url").and_then(toml::Value::as_str),
            Some("http://127.0.0.1:4101")
        );
        assert_eq!(
            values.get("max_rounds").and_then(toml::Value::as_integer),
            Some(3)
        );
        assert!(values.contains_key("password_file"));
        assert_eq!(values.len(), 4);
    }

    #[test]
    fn parses_multiple_projects_in_deterministic_order() {
        let text = "[projects.beta]\nworkspace = \"b\"\n\n[projects.alpha]\nworkspace = \"a\"\n";
        let projects = parse_projects(text).expect("multi-project config must parse");

        let ids: Vec<&str> = projects.keys().map(String::as_str).collect();
        assert_eq!(ids, ["alpha", "beta"]);
        assert_eq!(projects.len(), 2);
    }

    // Corpus case `invalid-toml-syntax`.
    #[test]
    fn rejects_syntactically_invalid_toml() {
        let error = parse_projects("this is not = = = toml\n").expect_err("invalid TOML must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "configuration file is not valid TOML");
    }

    // Corpus case `invalid-projects-table-missing`.
    #[test]
    fn rejects_missing_projects_table() {
        let error =
            parse_projects("[other]\nkey = \"value\"\n").expect_err("missing projects must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "configuration is missing the 'projects' table"
        );
    }

    #[test]
    fn rejects_non_table_projects_value() {
        for text in [
            "projects = \"nope\"\n",
            "projects = 5\n",
            "projects = [1, 2]\n",
        ] {
            let error = parse_projects(text).expect_err("non-table projects must fail");
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert_eq!(error.to_string(), "'projects' must be a TOML table");
        }
    }

    #[test]
    fn rejects_non_table_project_entry() {
        let error =
            parse_projects("[projects]\nproj = 5\n").expect_err("non-table entry must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "each 'projects' entry must be a TOML table"
        );
    }

    #[test]
    fn empty_projects_table_is_valid() {
        let projects = parse_projects("[projects]\n").expect("empty projects table must parse");

        assert!(projects.is_empty());
    }

    #[test]
    fn unknown_top_level_keys_do_not_break_loading() {
        let text = "[other]\nkey = \"value\"\n\n[projects.proj]\nworkspace = \"ws\"\n";
        let projects = parse_projects(text).expect("unknown top-level keys must be ignored");

        assert_eq!(projects.len(), 1);
        assert!(projects.contains_key("proj"));
    }

    // Corpus case `valid-minimal-project` (validated view).
    #[test]
    fn loads_valid_project_with_absolute_workspace() {
        let dir = TempDir::new("valid-absolute");
        let workspace = dir.mkdir("workspace");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );

        let config = load_config(&config_path).expect("config from file must load");

        assert_eq!(config.len(), 1);
        let entry = config.project("proj").expect("project 'proj' must exist");
        assert_eq!(entry.id().as_str(), "proj");
        assert_eq!(entry.workspace(), canonical(&workspace).as_path());
        assert_eq!(
            entry.get("opencode_url").and_then(toml::Value::as_str),
            Some("http://127.0.0.1:4101")
        );
        assert_eq!(
            entry.get("max_rounds").and_then(toml::Value::as_integer),
            Some(3)
        );
        assert!(entry.contains_key("password_file"));
        assert_eq!(entry.values().len(), 4);
    }

    // Corpus case `valid-workspace-relative-resolved-against-config-dir`.
    #[test]
    fn resolves_relative_workspace_against_config_dir() {
        let dir = TempDir::new("relative");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write("projects.toml", &project_toml("proj", "ws"));

        let config = load_config(&config_path).expect("relative workspace must load");

        let entry = config.project("proj").expect("project must exist");
        assert_eq!(entry.workspace(), canonical(&workspace).as_path());
        assert!(entry.workspace().is_absolute());
    }

    // Corpus case `valid-workspace-symlink-canonicalized`.
    #[cfg(unix)]
    #[test]
    fn symlink_workspace_is_canonicalized() {
        let dir = TempDir::new("symlink");
        let real = dir.mkdir("real");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink must be creatable");
        let config_path = dir.write("projects.toml", &project_toml("proj", "link"));

        let config = load_config(&config_path).expect("symlinked workspace must load");

        let entry = config.project("proj").expect("project must exist");
        assert_eq!(entry.workspace(), canonical(&real).as_path());
    }

    // Corpus case `invalid-project-id-pattern`.
    #[test]
    fn rejects_invalid_project_id_patterns() {
        let dir = TempDir::new("bad-id");
        let workspace = dir.mkdir("ws");
        let ws = workspace.to_str().expect("utf-8 path");

        let invalid = [
            "Bad_ID",
            "UPPER",
            "has.dot",
            "has space",
            "юникод",
            "_leading",
            "-leading",
        ];
        for id in invalid {
            let config_path = dir.write("projects.toml", &project_toml(id, ws));
            let error = load_config(&config_path).expect_err("invalid id must fail");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "id: {id}");
            assert_eq!(error.to_string(), "project id is invalid", "id: {id}");
        }
    }

    #[test]
    fn rejects_empty_project_id() {
        let dir = TempDir::new("empty-id");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("", workspace.to_str().expect("utf-8 path")),
        );

        let error = load_config(&config_path).expect_err("empty id must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project id is invalid");
    }

    // Boundary ids: length 1 and 64 are accepted.
    #[test]
    fn accepts_project_id_boundary_lengths() {
        let dir = TempDir::new("id-boundary-ok");
        let workspace = dir.mkdir("ws");
        let ws = workspace.to_str().expect("utf-8 path");

        let one = "a";
        let sixty_four = "a".repeat(64);

        for id in [one, sixty_four.as_str()] {
            let config_path = dir.write("projects.toml", &project_toml(id, ws));
            let config = load_config(&config_path).expect("boundary id must load");
            let entry = config.project(id).expect("project must exist");
            assert_eq!(entry.id().as_str(), id);
            assert_eq!(entry.id().as_str().len(), id.len());
        }
    }

    // Corpus case `invalid-project-id-too-long`: length 65 is rejected.
    #[test]
    fn rejects_project_id_longer_than_64() {
        let dir = TempDir::new("id-too-long");
        let workspace = dir.mkdir("ws");
        let too_long = "p".repeat(65);
        let config_path = dir.write(
            "projects.toml",
            &project_toml(&too_long, workspace.to_str().expect("utf-8 path")),
        );

        let error = load_config(&config_path).expect_err("too-long id must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project id is invalid");
    }

    #[test]
    fn accepts_underscore_and_hyphen_after_first_character() {
        let dir = TempDir::new("id-punct");
        let workspace = dir.mkdir("ws");
        let id = "a-b_c-9";
        let config_path = dir.write(
            "projects.toml",
            &project_toml(id, workspace.to_str().expect("utf-8 path")),
        );

        let config = load_config(&config_path).expect("valid id must load");
        assert_eq!(
            config
                .project(id)
                .expect("project must exist")
                .id()
                .as_str(),
            id
        );
    }

    #[test]
    fn rejects_missing_workspace_key() {
        let dir = TempDir::new("missing-ws");
        let config_path = dir.write(
            "projects.toml",
            "[projects.proj]\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = 3\n",
        );

        let error = load_config(&config_path).expect_err("missing workspace must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project workspace is missing");
    }

    #[test]
    fn rejects_non_string_workspace() {
        let dir = TempDir::new("ws-type");
        let config_path = dir.write(
            "projects.toml",
            "[projects.proj]\nworkspace = 3\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = 3\n",
        );

        let error = load_config(&config_path).expect_err("non-string workspace must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project workspace must be a string");
    }

    #[test]
    fn rejects_empty_workspace() {
        let dir = TempDir::new("ws-empty");
        let config_path = dir.write("projects.toml", &project_toml("proj", ""));

        let error = load_config(&config_path).expect_err("empty workspace must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project workspace must not be empty");
    }

    // Corpus case `invalid-workspace-missing`.
    #[test]
    fn missing_workspace_path_is_a_safe_not_found_error() {
        let dir = TempDir::new("ws-missing");
        let missing = dir.path().join("does-not-exist");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", missing.to_str().expect("utf-8 path")),
        );

        let error = load_config(&config_path).expect_err("missing workspace path must fail");

        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(error.to_string(), "project workspace does not exist");
    }

    // Corpus case `invalid-workspace-not-a-directory`.
    #[test]
    fn file_workspace_is_not_a_directory() {
        let dir = TempDir::new("ws-file");
        let file = dir.write("a-file", "not a directory");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", file.to_str().expect("utf-8 path")),
        );

        let error = load_config(&config_path).expect_err("file workspace must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project workspace is not a directory");
    }

    #[test]
    fn project_id_error_does_not_leak_id_workspace_or_config_path() {
        let dir = TempDir::new("redact-id");
        let workspace = dir.mkdir("ws");
        let bad_id = "zzz-secret-project.id";
        let config_path = dir.write(
            "projects.toml",
            &project_toml(bad_id, workspace.to_str().expect("utf-8 path")),
        );

        let error = load_config(&config_path).expect_err("invalid id must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);

        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(bad_id), "leaked id: {rendered}");
        assert!(
            !rendered.contains(workspace.to_str().expect("utf-8 path")),
            "leaked workspace: {rendered}"
        );
        assert!(
            !rendered.contains(config_path.to_str().expect("utf-8 path")),
            "leaked config path: {rendered}"
        );
    }

    #[test]
    fn workspace_error_does_not_leak_workspace_id_or_config_path() {
        let dir = TempDir::new("redact-ws");
        let secret_workspace = dir.path().join("super-secret-workspace-dir");
        let id = "zzzsecretid";
        let config_path = dir.write(
            "projects.toml",
            &project_toml(id, secret_workspace.to_str().expect("utf-8 path")),
        );

        let error = load_config(&config_path).expect_err("missing workspace must fail");
        assert_eq!(error.kind(), ErrorKind::NotFound);

        let rendered = format!("{error} {error:?}");
        assert!(
            !rendered.contains(secret_workspace.to_str().expect("utf-8 path")),
            "leaked workspace: {rendered}"
        );
        assert!(!rendered.contains(id), "leaked id: {rendered}");
        assert!(
            !rendered.contains(config_path.to_str().expect("utf-8 path")),
            "leaked config path: {rendered}"
        );
    }

    #[test]
    fn missing_file_is_a_safe_not_found_error() {
        let dir = TempDir::new("missing-file");
        let path = dir.path().join("nope.toml");
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
        let dir = TempDir::new("utf8");
        let path = dir.path().join("projects.toml");
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
    }

    #[test]
    fn parse_error_does_not_leak_toml_content_or_paths() {
        const SECRET: &str = "super-secret-token-value";
        const SECRET_PATH: &str = "/abs/secret/workspace/path";

        let text = format!(
            "password_file = \"{SECRET}\"\n[projects.proj\nworkspace = \"{SECRET_PATH}\"\n"
        );
        let error = parse_projects(&text).expect_err("invalid TOML must fail");

        let display = error.to_string();
        let debug = format!("{error:?}");

        assert!(
            !display.contains(SECRET),
            "Display leaked a secret: {display}"
        );
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
        let error = parse_projects(&text).expect_err("missing projects must fail");

        let rendered = format!("{error} {error:?}");
        assert!(
            !rendered.contains(SECRET),
            "safe error output leaked a secret: {rendered}"
        );
    }

    #[test]
    fn config_debug_does_not_render_values_or_workspace() {
        const SECRET: &str = "debug-secret-value";

        let dir = TempDir::new("debug");
        let workspace = dir.mkdir("workspace-dir");
        let text = format!(
            "[projects.proj]\nworkspace = \"{}\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"{SECRET}\"\nmax_rounds = 3\n",
            workspace.to_str().expect("utf-8 path")
        );
        let config_path = dir.write("projects.toml", &text);
        let config = load_config(&config_path).expect("config must load");

        let debug = format!("{config:?}");
        assert!(
            !debug.contains(SECRET),
            "Config Debug leaked a value: {debug}"
        );
        assert!(
            !debug.contains(workspace.to_str().expect("utf-8 path")),
            "Config Debug leaked a workspace: {debug}"
        );
        assert!(debug.contains("proj"));

        let entry_debug = format!("{:?}", config.project("proj").expect("project exists"));
        assert!(
            !entry_debug.contains(SECRET),
            "ProjectEntry Debug leaked a value: {entry_debug}"
        );
        assert!(
            !entry_debug.contains(workspace.to_str().expect("utf-8 path")),
            "ProjectEntry Debug leaked a workspace: {entry_debug}"
        );
    }

    #[test]
    fn validated_project_id_is_typed() {
        fn id_str(id: &ProjectId) -> &str {
            id.as_str()
        }

        let dir = TempDir::new("typed-id");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );
        let config = load_config(&config_path).expect("config must load");

        assert_eq!(
            id_str(config.project("proj").expect("project exists").id()),
            "proj"
        );
    }

    #[test]
    fn config_is_cloneable() {
        fn assert_clone<T: Clone>() {}

        assert_clone::<Config>();
        assert_clone::<super::ProjectEntry>();
    }

    // Corpus case `valid-minimal-project` (endpoint view).
    #[test]
    fn accepts_minimal_opencode_endpoint_and_exposes_typed_port() {
        let dir = TempDir::new("endpoint-minimal");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );

        let config = load_config(&config_path).expect("valid endpoint must load");
        let entry = config.project("proj").expect("project must exist");

        let endpoint = entry.opencode_endpoint();
        assert_eq!(endpoint.port(), 4101);
        assert_eq!(endpoint.host(), "127.0.0.1");
        assert_eq!(endpoint.url(), "http://127.0.0.1:4101");
        assert_eq!(endpoint.to_string(), "http://127.0.0.1:4101");
        assert!(entry.mcp_endpoint().is_none());
    }

    // Python accepts both an empty path and `/` for the OpenCode endpoint.
    #[test]
    fn accepts_opencode_url_with_trailing_slash() {
        let dir = TempDir::new("endpoint-slash");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101/",
                "",
            ),
        );

        let config = load_config(&config_path).expect("trailing slash must load");
        assert_eq!(
            config
                .project("proj")
                .expect("project must exist")
                .opencode_endpoint()
                .port(),
            4101
        );
    }

    // Regression: WHATWG drops the default HTTP port, but the contract requires
    // an explicit port, so `:80` must be accepted and preserved as 80.
    #[test]
    fn accepts_explicit_default_port_80() {
        let dir = TempDir::new("endpoint-default-port");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:80",
                "",
            ),
        );

        let config = load_config(&config_path).expect("explicit port 80 must load");
        let endpoint = config
            .project("proj")
            .expect("project must exist")
            .opencode_endpoint();
        assert_eq!(endpoint.port(), 80);
        assert_eq!(endpoint.url(), "http://127.0.0.1:80");
    }

    #[test]
    fn accepts_explicit_default_port_80_for_mcp() {
        let dir = TempDir::new("mcp-default-port");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "mcp_url = \"http://127.0.0.1:80/mcp\"\n",
            ),
        );

        let config = load_config(&config_path).expect("mcp explicit port 80 must load");
        let mcp = config
            .project("proj")
            .expect("project must exist")
            .mcp_endpoint()
            .expect("mcp endpoint must be present");
        assert_eq!(mcp.port(), 80);
        assert_eq!(mcp.url(), "http://127.0.0.1:80/mcp");
    }

    // Regression: WHATWG canonicalizes non-standard IPv4 spellings to
    // 127.0.0.1; the reference requires the exact raw host, so reject them.
    #[test]
    fn rejects_non_standard_ipv4_spellings() {
        for url in [
            "http://127.1:4101",
            "http://0177.0.0.1:4101",
            "http://0x7f000001:4101",
        ] {
            let error = load_endpoint_config("proj", url, "");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "url: {url}");
            assert_eq!(
                error.to_string(),
                "project opencode_url host must be exactly 127.0.0.1",
                "url: {url}"
            );
        }
    }

    // Regression: WHATWG folds dot segments to `/`; the reference compares the
    // raw path, so disguised paths must be rejected.
    #[test]
    fn rejects_dot_segment_disguised_opencode_paths() {
        for url in [
            "http://127.0.0.1:4101/./",
            "http://127.0.0.1:4101/a/..",
            "http://127.0.0.1:4101/%2e%2e",
        ] {
            let error = load_endpoint_config("proj", url, "");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "url: {url}");
            assert_eq!(
                error.to_string(),
                "project opencode_url must not contain a path",
                "url: {url}"
            );
        }
    }

    // Regression: a disguised extra path before `/mcp` must not be folded away.
    #[test]
    fn rejects_dot_segment_disguised_mcp_paths() {
        for mcp in [
            "http://127.0.0.1:4201/./mcp",
            "http://127.0.0.1:4201/a/../mcp",
            "http://127.0.0.1:4201/%2e%2e/mcp",
        ] {
            let extra = format!("mcp_url = \"{mcp}\"\n");
            let error = load_endpoint_config("proj", "http://127.0.0.1:4101", &extra);
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "mcp: {mcp}");
            assert_eq!(
                error.to_string(),
                "project mcp_url must not contain a path",
                "mcp: {mcp}"
            );
        }
    }

    // Regression: malformed port text stays rejected and redacted.
    #[test]
    fn rejects_malformed_ports_without_leaking_input() {
        for url in [
            "http://127.0.0.1:abc",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:0",
        ] {
            let error = load_endpoint_config("proj", url, "");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "url: {url}");
            assert_eq!(
                error.to_string(),
                "project opencode_url port must be within 1..65535",
                "url: {url}"
            );
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains("abc"), "leaked input: {rendered}");
        }
    }

    #[test]
    fn accepts_boundary_ports_one_and_65535() {
        for port in ["1", "65535"] {
            let dir = TempDir::new("endpoint-boundary");
            let workspace = dir.mkdir("ws");
            let url = format!("http://127.0.0.1:{port}");
            let config_path = dir.write(
                "projects.toml",
                &project_toml_with("proj", workspace.to_str().expect("utf-8 path"), &url, ""),
            );

            let config = load_config(&config_path).expect("boundary port must load");
            assert_eq!(
                config
                    .project("proj")
                    .expect("project must exist")
                    .opencode_endpoint()
                    .port(),
                port.parse::<u16>().expect("port must parse")
            );
        }
    }

    // Corpus case `invalid-opencode-url-port-out-of-range` plus 0 and 65536.
    #[test]
    fn rejects_port_zero_and_out_of_range() {
        for url in [
            "http://127.0.0.1:0",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:70000",
        ] {
            let error = load_endpoint_config("proj", url, "");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "url: {url}");
            assert_eq!(
                error.to_string(),
                "project opencode_url port must be within 1..65535",
                "url: {url}"
            );
        }
    }

    // Corpus case `invalid-opencode-url-missing-port`.
    #[test]
    fn rejects_missing_port() {
        let error = load_endpoint_config("proj", "http://127.0.0.1", "");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project opencode_url must include an explicit port"
        );
    }

    // Corpus case `invalid-opencode-url-scheme`.
    #[test]
    fn rejects_non_http_scheme() {
        let error = load_endpoint_config("proj", "https://127.0.0.1:4101", "");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project opencode_url must use the http scheme"
        );
    }

    // Corpus case `invalid-opencode-url-non-loopback-host` plus other hosts.
    #[test]
    fn rejects_non_loopback_hosts() {
        for url in [
            "http://localhost:4101",
            "http://0.0.0.0:4101",
            "http://[::1]:4101",
            "http://example.com:4101",
            "http://127.0.0.2:4101",
        ] {
            let error = load_endpoint_config("proj", url, "");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "url: {url}");
            assert_eq!(
                error.to_string(),
                "project opencode_url host must be exactly 127.0.0.1",
                "url: {url}"
            );
        }
    }

    // Corpus case `invalid-opencode-url-embedded-credentials`.
    #[test]
    fn rejects_embedded_credentials_without_leaking_them() {
        let error = load_endpoint_config("proj", "http://user:pass@127.0.0.1:4101", "");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project opencode_url must not embed credentials"
        );

        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains("user"), "leaked username: {rendered}");
        assert!(!rendered.contains("pass"), "leaked password: {rendered}");
    }

    // Corpus case `invalid-opencode-url-query-or-fragment`.
    #[test]
    fn rejects_query_and_fragment() {
        for url in ["http://127.0.0.1:4101?x=1", "http://127.0.0.1:4101#frag"] {
            let error = load_endpoint_config("proj", url, "");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "url: {url}");
            assert_eq!(
                error.to_string(),
                "project opencode_url must not contain a query or fragment",
                "url: {url}"
            );
        }
    }

    // Corpus case `invalid-opencode-url-path`.
    #[test]
    fn rejects_path() {
        for url in ["http://127.0.0.1:4101/api", "http://127.0.0.1:4101//"] {
            let error = load_endpoint_config("proj", url, "");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "url: {url}");
            assert_eq!(
                error.to_string(),
                "project opencode_url must not contain a path",
                "url: {url}"
            );
        }
    }

    // Corpus case `invalid-opencode-url-non-string`.
    #[test]
    fn rejects_non_string_opencode_url() {
        let dir = TempDir::new("opencode-type");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &format!(
                "[projects.proj]\nworkspace = \"{}\"\nopencode_url = 4101\npassword_file = \"secrets/proj.password\"\nmax_rounds = 1\n",
                workspace.to_str().expect("utf-8 path")
            ),
        );

        let error = load_config(&config_path).expect_err("non-string opencode_url must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project opencode_url must be a string");
    }

    #[test]
    fn rejects_missing_opencode_url() {
        let dir = TempDir::new("opencode-missing");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &format!(
                "[projects.proj]\nworkspace = \"{}\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = 3\n",
                workspace.to_str().expect("utf-8 path")
            ),
        );

        let error = load_config(&config_path).expect_err("missing opencode_url must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project opencode_url is missing");
    }

    #[test]
    fn malformed_opencode_url_keeps_parser_diagnostic_only_in_source() {
        let error = load_endpoint_config("proj", "not a url", "");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project opencode_url is not a valid URL");

        let rendered = format!("{error} {error:?}");
        assert!(
            !rendered.contains("not a url"),
            "safe output leaked the input: {rendered}"
        );
        assert!(
            error.source().is_some(),
            "parser diagnostic must be retained as source"
        );
    }

    // Corpus case `valid-mcp-url-and-token-pairing` (endpoint view only; the
    // token pairing itself belongs to a later task).
    #[test]
    fn accepts_valid_mcp_url_and_exposes_typed_port() {
        let dir = TempDir::new("mcp-valid");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "mcp_url = \"http://127.0.0.1:4201/mcp\"\n",
            ),
        );

        let config = load_config(&config_path).expect("valid mcp_url must load");
        let mcp = config
            .project("proj")
            .expect("project must exist")
            .mcp_endpoint()
            .expect("mcp endpoint must be present");

        assert_eq!(mcp.port(), 4201);
        assert_eq!(mcp.base().port(), 4201);
        assert_eq!(mcp.base().url(), "http://127.0.0.1:4201");
        assert_eq!(mcp.url(), "http://127.0.0.1:4201/mcp");
        assert_eq!(mcp.to_string(), "http://127.0.0.1:4201/mcp");
    }

    // Corpus case `invalid-mcp-url-must-end-with-mcp`.
    #[test]
    fn rejects_mcp_url_without_mcp_suffix() {
        let error = load_endpoint_config(
            "proj",
            "http://127.0.0.1:4101",
            "mcp_url = \"http://127.0.0.1:4201/other\"\n",
        );
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project mcp_url must end with '/mcp'");
    }

    // Corpus case `invalid-mcp-url-non-loopback-host`.
    #[test]
    fn rejects_mcp_url_non_loopback_host() {
        let error = load_endpoint_config(
            "proj",
            "http://127.0.0.1:4101",
            "mcp_url = \"http://0.0.0.0:4201/mcp\"\n",
        );
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project mcp_url host must be exactly 127.0.0.1"
        );
    }

    #[test]
    fn rejects_mcp_url_wrong_type() {
        let dir = TempDir::new("mcp-type");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "mcp_url = 4201\n",
            ),
        );

        let error = load_config(&config_path).expect_err("non-string mcp_url must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project mcp_url must be a string");
    }

    #[test]
    fn rejects_mcp_url_empty() {
        let error = load_endpoint_config("proj", "http://127.0.0.1:4101", "mcp_url = \"\"\n");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project mcp_url must end with '/mcp'");
    }

    #[test]
    fn rejects_mcp_url_bad_parts() {
        for (extra, expected) in [
            (
                "mcp_url = \"https://127.0.0.1:4201/mcp\"\n",
                "project mcp_url must use the http scheme",
            ),
            (
                "mcp_url = \"http://user:pass@127.0.0.1:4201/mcp\"\n",
                "project mcp_url must not embed credentials",
            ),
            (
                "mcp_url = \"http://127.0.0.1/mcp\"\n",
                "project mcp_url must include an explicit port",
            ),
            (
                "mcp_url = \"http://127.0.0.1:0/mcp\"\n",
                "project mcp_url port must be within 1..65535",
            ),
            (
                "mcp_url = \"http://127.0.0.1:70000/mcp\"\n",
                "project mcp_url port must be within 1..65535",
            ),
            (
                "mcp_url = \"http://127.0.0.1:4201/mcp?x=1\"\n",
                "project mcp_url must end with '/mcp'",
            ),
            (
                "mcp_url = \"http://127.0.0.1:4201/mcp#f\"\n",
                "project mcp_url must end with '/mcp'",
            ),
            (
                "mcp_url = \"http://127.0.0.1:4201/mcp/x\"\n",
                "project mcp_url must end with '/mcp'",
            ),
        ] {
            let error = load_endpoint_config("proj", "http://127.0.0.1:4101", extra);
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "extra: {extra}");
            assert_eq!(error.to_string(), expected, "extra: {extra}");
        }
    }

    #[test]
    fn preserves_raw_endpoint_values_for_later_tasks() {
        let dir = TempDir::new("raw-endpoints");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101/",
                "mcp_url = \"http://127.0.0.1:4201/mcp\"\nmcp_token_file = \"secrets/proj.mcp-token\"\n",
            ),
        );

        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");

        assert_eq!(
            entry.get(OPENCODE_URL_KEY).and_then(toml::Value::as_str),
            Some("http://127.0.0.1:4101/")
        );
        assert_eq!(
            entry.get(MCP_URL_KEY).and_then(toml::Value::as_str),
            Some("http://127.0.0.1:4201/mcp")
        );
        assert!(entry.contains_key("mcp_token_file"));
    }

    #[test]
    fn endpoint_errors_do_not_leak_url_project_workspace_or_config_path() {
        const SECRET_URL: &str = "http://user:secret-pass@127.0.0.1:4101";
        let dir = TempDir::new("endpoint-redact");
        let workspace = dir.mkdir("secret-workspace");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                SECRET_URL,
                "",
            ),
        );

        let error = load_config(&config_path).expect_err("credentials must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);

        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(SECRET_URL), "leaked URL: {rendered}");
        assert!(
            !rendered.contains("secret-pass"),
            "leaked password: {rendered}"
        );
        assert!(
            !rendered.contains(workspace.to_str().expect("utf-8 path")),
            "leaked workspace: {rendered}"
        );
        assert!(
            !rendered.contains(config_path.to_str().expect("utf-8 path")),
            "leaked config path: {rendered}"
        );
    }

    /// Builds a two-project table with explicit per-project endpoints and extras.
    fn two_project_toml(
        a_ws: &str,
        a_url: &str,
        a_extra: &str,
        b_ws: &str,
        b_url: &str,
        b_extra: &str,
    ) -> String {
        format!(
            "[projects.a]\nworkspace = \"{a_ws}\"\nopencode_url = \"{a_url}\"\npassword_file = \"secrets/a.password\"\nmax_rounds = 3\n{a_extra}\n[projects.b]\nworkspace = \"{b_ws}\"\nopencode_url = \"{b_url}\"\npassword_file = \"secrets/b.password\"\nmax_rounds = 3\n{b_extra}\n"
        )
    }

    // Corpus case `invalid-workspace-duplicate`.
    #[test]
    fn rejects_duplicate_canonical_workspace_across_projects() {
        let dir = TempDir::new("dup-workspace");
        let workspace = dir.mkdir("ws");
        let ws = workspace.to_str().expect("utf-8 path");
        let config_path = dir.write(
            "projects.toml",
            &two_project_toml(
                ws,
                "http://127.0.0.1:4101",
                "",
                ws,
                "http://127.0.0.1:4102",
                "",
            ),
        );

        let error = load_config(&config_path).expect_err("duplicate workspace must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project workspace is already used by another project"
        );
    }

    // Canonical uniqueness also collapses a symlink alias of the same directory.
    #[cfg(unix)]
    #[test]
    fn rejects_workspace_symlink_alias_across_projects() {
        let dir = TempDir::new("dup-workspace-link");
        let real = dir.mkdir("real");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink must be creatable");
        let config_path = dir.write(
            "projects.toml",
            &two_project_toml(
                real.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "",
                link.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4102",
                "",
            ),
        );

        let error = load_config(&config_path).expect_err("workspace alias must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project workspace is already used by another project"
        );
    }

    // Corpus case `invalid-endpoint-duplicate-across-projects` plus the other
    // collision directions the contract requires.
    #[test]
    fn rejects_reused_server_endpoints_table_driven() {
        let dir = TempDir::new("dup-endpoint");
        let ws_a = dir.mkdir("a");
        let ws_b = dir.mkdir("b");
        let a = ws_a.to_str().expect("utf-8 path");
        let b = ws_b.to_str().expect("utf-8 path");

        let mcp_a = "mcp_url = \"http://127.0.0.1:4201/mcp\"\nmcp_token_file = \"secrets/a.mcp\"\n";
        let mcp_b = "mcp_url = \"http://127.0.0.1:4201/mcp\"\nmcp_token_file = \"secrets/b.mcp\"\n";

        let cases = [
            (
                "opencode/opencode",
                two_project_toml(
                    a,
                    "http://127.0.0.1:4101",
                    "",
                    b,
                    "http://127.0.0.1:4101",
                    "",
                ),
            ),
            (
                "mcp/mcp",
                two_project_toml(
                    a,
                    "http://127.0.0.1:4101",
                    mcp_a,
                    b,
                    "http://127.0.0.1:4102",
                    mcp_b,
                ),
            ),
            (
                "mcp/opencode",
                two_project_toml(
                    a,
                    "http://127.0.0.1:4101",
                    "",
                    b,
                    "http://127.0.0.1:4102",
                    "mcp_url = \"http://127.0.0.1:4101/mcp\"\nmcp_token_file = \"secrets/b.mcp\"\n",
                ),
            ),
        ];

        for (name, text) in cases {
            let config_path = dir.write("projects.toml", &text);
            let error = load_config(&config_path).expect_err("reused endpoint must fail");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "case: {name}");
            assert_eq!(
                error.to_string(),
                "project endpoint is already used by another project",
                "case: {name}"
            );
        }
    }

    // Corpus case `invalid-mcp-endpoint-collides-with-opencode-endpoint`.
    #[test]
    fn rejects_mcp_endpoint_colliding_with_own_opencode_endpoint() {
        let dir = TempDir::new("dup-endpoint-self");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "mcp_url = \"http://127.0.0.1:4101/mcp\"\nmcp_token_file = \"secrets/proj.mcp\"\n",
            ),
        );

        let error = load_config(&config_path).expect_err("self endpoint collision must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project endpoint is already used by another project"
        );
    }

    // Corpus case `invalid-mcp-token-file-duplicate-across-projects`.
    #[test]
    fn rejects_duplicate_mcp_token_file_across_projects() {
        let dir = TempDir::new("dup-token");
        let ws_a = dir.mkdir("a");
        let ws_b = dir.mkdir("b");
        let config_path = dir.write(
            "projects.toml",
            &two_project_toml(
                ws_a.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "mcp_url = \"http://127.0.0.1:4201/mcp\"\nmcp_token_file = \"secrets/shared.mcp-token\"\n",
                ws_b.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4102",
                "mcp_url = \"http://127.0.0.1:4202/mcp\"\nmcp_token_file = \"secrets/shared.mcp-token\"\n",
            ),
        );

        let error = load_config(&config_path).expect_err("duplicate token must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project mcp_token_file is already used by another project"
        );
    }

    // Corpus case `invalid-mcp-token-file-equals-password-file`, plus the
    // cross-project variant of the same separation rule.
    #[test]
    fn rejects_mcp_token_file_matching_a_password_file() {
        let dir = TempDir::new("token-eq-password");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "mcp_url = \"http://127.0.0.1:4201/mcp\"\nmcp_token_file = \"secrets/proj.password\"\n",
            ),
        );
        let error = load_config(&config_path).expect_err("token equal to own password must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project mcp_token_file must differ from the project password_file"
        );

        let ws_a = dir.mkdir("a");
        let ws_b = dir.mkdir("b");
        let cross_path = dir.write(
            "cross.toml",
            &two_project_toml(
                ws_a.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "",
                ws_b.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4102",
                "mcp_url = \"http://127.0.0.1:4202/mcp\"\nmcp_token_file = \"secrets/a.password\"\n",
            ),
        );
        let cross_error =
            load_config(&cross_path).expect_err("token equal to another password must fail");
        assert_eq!(cross_error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            cross_error.to_string(),
            "project mcp_token_file must differ from the project password_file"
        );
    }

    // Review round 2: a missing token file reached through a symlinked parent
    // directory must still collapse onto the real parent, mirroring
    // `Path.resolve(strict=False)`.
    #[cfg(unix)]
    #[test]
    fn rejects_duplicate_missing_token_files_through_symlink_parent() {
        let dir = TempDir::new("dup-token-link");
        let real = dir.mkdir("real");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink must be creatable");
        let ws_a = dir.mkdir("a");
        let ws_b = dir.mkdir("b");

        let text = format!(
            "[projects.a]\nworkspace = \"{a}\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"secrets/a.password\"\nmax_rounds = 3\nmcp_url = \"http://127.0.0.1:4201/mcp\"\nmcp_token_file = \"real/shared.token\"\n\n[projects.b]\nworkspace = \"{b}\"\nopencode_url = \"http://127.0.0.1:4102\"\npassword_file = \"secrets/b.password\"\nmax_rounds = 3\nmcp_url = \"http://127.0.0.1:4202/mcp\"\nmcp_token_file = \"link/shared.token\"\n",
            a = ws_a.to_str().expect("utf-8 path"),
            b = ws_b.to_str().expect("utf-8 path"),
        );
        let config_path = dir.write("projects.toml", &text);

        let error = load_config(&config_path).expect_err("symlinked token alias must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project mcp_token_file is already used by another project"
        );
    }

    // Review round 2: a token reached through a symlinked parent must match a
    // password reached through the real parent.
    #[cfg(unix)]
    #[test]
    fn rejects_token_through_symlink_parent_matching_password_through_real_parent() {
        let dir = TempDir::new("token-link-password");
        let real = dir.mkdir("real");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink must be creatable");
        let ws_a = dir.mkdir("a");
        let ws_b = dir.mkdir("b");

        let text = format!(
            "[projects.a]\nworkspace = \"{a}\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"real/shared.cred\"\nmax_rounds = 3\n\n[projects.b]\nworkspace = \"{b}\"\nopencode_url = \"http://127.0.0.1:4102\"\npassword_file = \"secrets/b.password\"\nmax_rounds = 3\nmcp_url = \"http://127.0.0.1:4202/mcp\"\nmcp_token_file = \"link/shared.cred\"\n",
            a = ws_a.to_str().expect("utf-8 path"),
            b = ws_b.to_str().expect("utf-8 path"),
        );
        let config_path = dir.write("projects.toml", &text);

        let error = load_config(&config_path).expect_err("token/password alias must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project mcp_token_file must differ from the project password_file"
        );
    }

    // Review round 2: `..` above the root must not turn an absolute credential
    // path relative; it normalizes to the root-relative path and still collides.
    #[test]
    fn normalizes_parent_dir_above_root_for_credentials() {
        let dir = TempDir::new("root-parent");
        let ws_a = dir.mkdir("a");
        let ws_b = dir.mkdir("b");

        let text = format!(
            "[projects.a]\nworkspace = \"{a}\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"/bridge-config-review2/shared.cred\"\nmax_rounds = 3\n\n[projects.b]\nworkspace = \"{b}\"\nopencode_url = \"http://127.0.0.1:4102\"\npassword_file = \"secrets/b.password\"\nmax_rounds = 3\nmcp_url = \"http://127.0.0.1:4202/mcp\"\nmcp_token_file = \"/../bridge-config-review2/shared.cred\"\n",
            a = ws_a.to_str().expect("utf-8 path"),
            b = ws_b.to_str().expect("utf-8 path"),
        );
        let config_path = dir.write("projects.toml", &text);

        let error = load_config(&config_path).expect_err("root-escape token must collide");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project mcp_token_file must differ from the project password_file"
        );
    }

    // Review round 3: a cyclic symlink in a credential path must fail closed
    // with a safe error, mirroring `Path.resolve(strict=False)`, which raises
    // on a symlink loop instead of returning a lexical path. Both the password
    // and the MCP token resolution paths are exercised, and the safe output
    // must not leak the cyclic path or the config path.
    #[cfg(unix)]
    #[test]
    fn rejects_cyclic_symlink_in_credential_path_without_leaking_path() {
        for (name, password, token) in [
            ("password", "loop-a/secret.cred", "secrets/proj.mcp-token"),
            ("token", "secrets/proj.password", "loop-a/secret.cred"),
        ] {
            let dir = TempDir::new("cyclic-cred");
            let workspace = dir.mkdir("ws");
            let loop_a = dir.path().join("loop-a");
            let loop_b = dir.path().join("loop-b");
            std::os::unix::fs::symlink(&loop_b, &loop_a).expect("symlink must be creatable");
            std::os::unix::fs::symlink(&loop_a, &loop_b).expect("symlink must be creatable");
            let text = format!(
                "[projects.proj]\nworkspace = \"{ws}\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"{password}\"\nmax_rounds = 3\nmcp_url = \"http://127.0.0.1:4201/mcp\"\nmcp_token_file = \"{token}\"\n",
                ws = workspace.to_str().expect("utf-8 path"),
            );
            let config_path = dir.write("projects.toml", &text);

            let error = load_config(&config_path).expect_err("cyclic symlink must be rejected");
            assert_eq!(error.kind(), ErrorKind::Internal, "case: {name}");
            assert_eq!(
                error.to_string(),
                "project credential file path could not be resolved",
                "case: {name}"
            );

            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains("loop-a"), "leaked path: {rendered}");
            assert!(!rendered.contains("loop-b"), "leaked path: {rendered}");
            assert!(!rendered.contains("secret.cred"), "leaked path: {rendered}");
            assert!(
                !rendered.contains(config_path.to_str().expect("utf-8 path")),
                "leaked config path: {rendered}"
            );
        }
    }

    // Corpus case `invalid-mcp-token-file-empty`.
    #[test]
    fn rejects_empty_mcp_token_file() {
        let error = load_endpoint_config(
            "proj",
            "http://127.0.0.1:4101",
            "mcp_url = \"http://127.0.0.1:4201/mcp\"\nmcp_token_file = \"\"\n",
        );

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project mcp_token_file must not be empty"
        );
    }

    #[test]
    fn rejects_non_string_mcp_token_file() {
        let error = load_endpoint_config(
            "proj",
            "http://127.0.0.1:4101",
            "mcp_url = \"http://127.0.0.1:4201/mcp\"\nmcp_token_file = 7\n",
        );

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project mcp_token_file must be a string");
    }

    // Valid multi-project configuration with distinct workspaces, endpoints and
    // credential files must keep loading.
    #[test]
    fn accepts_valid_multi_project_config() {
        let dir = TempDir::new("valid-multi");
        let ws_a = dir.mkdir("a");
        let ws_b = dir.mkdir("b");
        let config_path = dir.write(
            "projects.toml",
            &two_project_toml(
                ws_a.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "",
                ws_b.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4102",
                "mcp_url = \"http://127.0.0.1:4202/mcp\"\nmcp_token_file = \"secrets/b.mcp-token\"\n",
            ),
        );

        let config = load_config(&config_path).expect("valid multi-project config must load");

        assert_eq!(config.len(), 2);
        assert!(config.project("a").is_some());
        let b = config.project("b").expect("project 'b' must exist");
        assert!(b.mcp_endpoint().is_some());
        assert_eq!(b.opencode_endpoint().port(), 4102);
    }

    #[test]
    fn uniqueness_errors_do_not_leak_ids_workspaces_urls_or_paths() {
        let dir = TempDir::new("unique-redact");
        let workspace = dir.mkdir("secret-workspace");
        let ws = workspace.to_str().expect("utf-8 path");
        let config_path = dir.write(
            "projects.toml",
            &two_project_toml(
                ws,
                "http://127.0.0.1:4101",
                "",
                ws,
                "http://127.0.0.1:4102",
                "",
            ),
        );

        let error = load_config(&config_path).expect_err("duplicate workspace must fail");
        let rendered = format!("{error} {error:?}");

        assert!(
            !rendered.contains("secret-workspace"),
            "leaked workspace: {rendered}"
        );
        assert!(!rendered.contains("4101"), "leaked endpoint: {rendered}");
        assert!(
            !rendered.contains("secrets/a.password"),
            "leaked path: {rendered}"
        );
        assert!(
            !rendered.contains(config_path.to_str().expect("utf-8 path")),
            "leaked config path: {rendered}"
        );
    }

    /// Builds a project table with an explicit `max_rounds` literal and extras.
    fn project_toml_max(id: &str, workspace: &str, max_rounds: &str, extra: &str) -> String {
        format!(
            "[projects.\"{id}\"]\nworkspace = \"{workspace}\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = {max_rounds}\n{extra}"
        )
    }

    /// Loads a one-project config with the given `max_rounds` literal and extras.
    fn load_max_rounds_config(max_rounds: &str, extra: &str) -> DomainError {
        let dir = TempDir::new("max-rounds-error");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_max(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                max_rounds,
                extra,
            ),
        );
        load_config(&config_path).expect_err("config must be rejected")
    }

    /// Loads a one-project config with the given `opencode_model` literal.
    fn load_model_config(model: &str) -> DomainError {
        let extra = format!("opencode_model = {model}\n");
        let dir = TempDir::new("model-error");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                &extra,
            ),
        );
        load_config(&config_path).expect_err("config must be rejected")
    }

    // Corpus case `valid-minimal-project` (typed max_rounds/model/env view).
    #[test]
    fn exposes_typed_max_rounds_and_absent_optionals() {
        let dir = TempDir::new("typed-minimal");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );

        let config = load_config(&config_path).expect("valid project must load");
        let entry = config.project("proj").expect("project must exist");

        assert_eq!(entry.max_rounds(), 3);
        assert!(entry.opencode_model().is_none());
        assert!(entry.opencode_env_file().is_none());
    }

    #[test]
    fn accepts_max_rounds_boundary_values() {
        for rounds in ["1", "9223372036854775807"] {
            let dir = TempDir::new("max-rounds-boundary");
            let workspace = dir.mkdir("ws");
            let config_path = dir.write(
                "projects.toml",
                &project_toml_max("proj", workspace.to_str().expect("utf-8 path"), rounds, ""),
            );

            let config = load_config(&config_path).expect("positive max_rounds must load");
            assert_eq!(
                config
                    .project("proj")
                    .expect("project must exist")
                    .max_rounds(),
                rounds.parse::<u64>().expect("rounds must parse")
            );
        }
    }

    #[test]
    fn rejects_missing_max_rounds() {
        let dir = TempDir::new("max-rounds-missing");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &format!(
                "[projects.proj]\nworkspace = \"{}\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"secrets/proj.password\"\n",
                workspace.to_str().expect("utf-8 path")
            ),
        );

        let error = load_config(&config_path).expect_err("missing max_rounds must fail");

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project max_rounds is missing");
    }

    // Corpus case `invalid-max-rounds-non-integer`: strings, booleans, floats,
    // arrays and tables are all rejected as non-integers.
    #[test]
    fn rejects_non_integer_max_rounds() {
        for literal in ["\"3\"", "true", "false", "3.5", "3.0", "[1]", "{ x = 1 }"] {
            let error = load_max_rounds_config(literal, "");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "literal: {literal}");
            assert_eq!(
                error.to_string(),
                "project max_rounds must be a positive integer",
                "literal: {literal}"
            );
        }
    }

    // Corpus case `invalid-max-rounds-not-positive` plus the negative boundary.
    #[test]
    fn rejects_non_positive_max_rounds() {
        for literal in ["0", "-1", "-9223372036854775808"] {
            let error = load_max_rounds_config(literal, "");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "literal: {literal}");
            assert_eq!(
                error.to_string(),
                "project max_rounds must be a positive integer",
                "literal: {literal}"
            );
        }
    }

    // Corpus case `valid-opencode-model-splits-on-first-slash`.
    #[test]
    fn accepts_opencode_model_and_splits_on_first_slash() {
        let dir = TempDir::new("model-first-slash");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "opencode_model = \"opencode/grok-code/1.0\"\n",
            ),
        );

        let config = load_config(&config_path).expect("valid model must load");
        let model = config
            .project("proj")
            .expect("project must exist")
            .opencode_model()
            .expect("model must be present");

        assert_eq!(model.provider(), "opencode");
        assert_eq!(model.model(), "grok-code/1.0");
    }

    #[test]
    fn accepts_simple_opencode_model() {
        let dir = TempDir::new("model-simple");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "opencode_model = \"anthropic/claude-sonnet\"\n",
            ),
        );

        let config = load_config(&config_path).expect("valid model must load");
        let model = config
            .project("proj")
            .expect("project must exist")
            .opencode_model()
            .expect("model must be present");

        assert_eq!(model.provider(), "anthropic");
        assert_eq!(model.model(), "claude-sonnet");
    }

    // Corpus case `invalid-opencode-model-non-string`.
    #[test]
    fn rejects_non_string_opencode_model() {
        let error = load_model_config("3");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project opencode_model must be a string");
    }

    // Corpus case `invalid-opencode-model-missing-separator`.
    #[test]
    fn rejects_opencode_model_missing_separator() {
        let error = load_model_config("\"noslash\"");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project opencode_model must be '<providerID>/<modelID>'"
        );
    }

    #[test]
    fn rejects_opencode_model_empty_components() {
        for raw in ["/b", "a/", "/", "//"] {
            let error = load_model_config(&format!("\"{raw}\""));
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "raw: {raw}");
            assert_eq!(
                error.to_string(),
                "project opencode_model must be '<providerID>/<modelID>'",
                "raw: {raw}"
            );
        }
    }

    // Corpus case `invalid-opencode-model-surrounding-whitespace`.
    #[test]
    fn rejects_opencode_model_surrounding_whitespace() {
        for raw in [" a/b", "a/b ", " a/b ", " /b", "a/ "] {
            let error = load_model_config(&format!("\"{raw}\""));
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "raw: {raw}");
            assert_eq!(
                error.to_string(),
                "project opencode_model must not have surrounding whitespace",
                "raw: {raw}"
            );
        }
    }

    #[test]
    fn rejects_opencode_model_component_whitespace() {
        for raw in ["a /b", "a/ b"] {
            let error = load_model_config(&format!("\"{raw}\""));
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "raw: {raw}");
            assert_eq!(
                error.to_string(),
                "project opencode_model components must not have surrounding whitespace",
                "raw: {raw}"
            );
        }
    }

    // Corpus case `valid-opencode-env-file-relative`.
    #[test]
    fn resolves_relative_opencode_env_file_against_config_dir() {
        let dir = TempDir::new("env-relative");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "opencode_env_file = \"secrets/proj.env\"\n",
            ),
        );

        let config = load_config(&config_path).expect("relative env file must load");
        let entry = config.project("proj").expect("project must exist");

        assert_eq!(
            entry.opencode_env_file(),
            Some(dir.path().join("secrets/proj.env").as_path())
        );
        assert!(entry.opencode_env_file().expect("path").is_absolute());
    }

    // Corpus case `valid-opencode-env-file-absolute`.
    #[test]
    fn preserves_absolute_opencode_env_file_verbatim() {
        let dir = TempDir::new("env-absolute");
        let workspace = dir.mkdir("ws");
        let env_file = dir.path().join("absolute.env");
        let extra = format!(
            "opencode_env_file = \"{}\"\n",
            env_file.to_str().expect("utf-8 path")
        );
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                &extra,
            ),
        );

        let config = load_config(&config_path).expect("absolute env file must load");
        let entry = config.project("proj").expect("project must exist");

        assert_eq!(entry.opencode_env_file(), Some(env_file.as_path()));
    }

    // Corpus case `invalid-opencode-env-file-non-string`.
    #[test]
    fn rejects_non_string_opencode_env_file() {
        let error =
            load_endpoint_config("proj", "http://127.0.0.1:4101", "opencode_env_file = 3\n");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project opencode_env_file must be a non-empty string"
        );
    }

    #[test]
    fn rejects_empty_opencode_env_file() {
        let error = load_endpoint_config(
            "proj",
            "http://127.0.0.1:4101",
            "opencode_env_file = \"\"\n",
        );
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project opencode_env_file must be a non-empty string"
        );
    }

    #[test]
    fn max_rounds_model_and_env_errors_do_not_leak_supplied_values() {
        const SECRET_ROUNDS: &str = "secret-rounds-value";
        const SECRET_MODEL: &str = "secret-provider/secret-model";

        let rounds_error = load_max_rounds_config(&format!("\"{SECRET_ROUNDS}\""), "");
        let rendered = format!("{rounds_error} {rounds_error:?}");
        assert!(
            !rendered.contains(SECRET_ROUNDS),
            "leaked max_rounds: {rendered}"
        );

        let model_error = load_model_config(&format!("\"{SECRET_MODEL} \""));
        let rendered = format!("{model_error} {model_error:?}");
        assert!(
            !rendered.contains("secret-provider"),
            "leaked model: {rendered}"
        );
        assert!(
            !rendered.contains("secret-model"),
            "leaked model: {rendered}"
        );
    }

    #[test]
    fn project_entry_debug_does_not_render_typed_values_or_paths() {
        const SECRET_MODEL: &str = "secret-provider/secret-model";
        let dir = TempDir::new("debug-typed");
        let workspace = dir.mkdir("ws");
        let env_file = dir.path().join("secret-env.env");
        let extra = format!(
            "opencode_model = \"{SECRET_MODEL}\"\nopencode_env_file = \"{}\"\n",
            env_file.to_str().expect("utf-8 path")
        );
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                &extra,
            ),
        );

        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");
        let debug = format!("{entry:?}");

        assert!(!debug.contains(SECRET_MODEL), "Debug leaked model: {debug}");
        assert!(
            !debug.contains("secret-env.env"),
            "Debug leaked env path: {debug}"
        );
        assert!(debug.contains(MAX_ROUNDS_KEY));
        assert!(debug.contains(OPENCODE_MODEL_KEY));
        assert!(debug.contains(OPENCODE_ENV_FILE_KEY));
    }

    #[test]
    fn crate_constant_names_the_auto_approve_permissions_key() {
        assert_eq!(AUTO_APPROVE_PERMISSIONS_KEY, "auto_approve_permissions");
    }

    /// Loads a one-project config with the given `auto_approve_permissions`
    /// literal, returning the validation error and panicking if it loads.
    fn load_auto_approve_config(literal: &str) -> DomainError {
        let extra = format!("auto_approve_permissions = {literal}\n");
        load_endpoint_config("proj", "http://127.0.0.1:4101", &extra)
    }

    /// Loads a one-project config with the given `auto_approve_permissions`
    /// literal and returns the typed, validated permission names.
    fn load_auto_approve_permissions(literal: &str) -> Vec<String> {
        let dir = TempDir::new("auto-approve");
        let workspace = dir.mkdir("ws");
        let extra = format!("auto_approve_permissions = {literal}\n");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                &extra,
            ),
        );
        let config = load_config(&config_path).expect("valid permissions must load");
        config
            .project("proj")
            .expect("project must exist")
            .auto_approve_permissions()
            .to_vec()
    }

    // Corpus case `valid-minimal-project`: the optional key defaults to empty.
    #[test]
    fn auto_approve_permissions_default_to_empty() {
        let dir = TempDir::new("auto-approve-default");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );

        let config = load_config(&config_path).expect("minimal config must load");
        let entry = config.project("proj").expect("project must exist");

        assert!(entry.auto_approve_permissions().is_empty());
    }

    #[test]
    fn parses_auto_approve_permissions_in_order() {
        let permissions = load_auto_approve_permissions("[\"bash\", \"edit\", \"webfetch\"]");
        assert_eq!(permissions, ["bash", "edit", "webfetch"]);
    }

    // Corpus case `valid-auto-approve-permissions-deduplicated`.
    #[test]
    fn collapses_duplicate_auto_approve_permissions_stably() {
        let permissions = load_auto_approve_permissions("[\"bash\", \"bash\", \"edit\"]");
        assert_eq!(permissions, ["bash", "edit"]);
    }

    #[test]
    fn preserves_first_seen_order_across_duplicates() {
        let permissions = load_auto_approve_permissions("[\"edit\", \"bash\", \"edit\", \"bash\"]");
        assert_eq!(permissions, ["edit", "bash"]);
    }

    #[test]
    fn accepts_empty_auto_approve_permissions_array() {
        let permissions = load_auto_approve_permissions("[]");
        assert!(permissions.is_empty());
    }

    // Corpus case `invalid-auto-approve-permissions-non-list`.
    #[test]
    fn rejects_non_array_auto_approve_permissions() {
        for literal in ["\"bash\"", "3", "true", "3.5", "{ x = 1 }"] {
            let error = load_auto_approve_config(literal);
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "literal: {literal}");
            assert_eq!(
                error.to_string(),
                "project auto_approve_permissions must be a list of strings",
                "literal: {literal}"
            );
        }
    }

    #[test]
    fn rejects_non_string_auto_approve_permission_entries() {
        for literal in ["[1]", "[\"bash\", 2]", "[true]", "[[\"bash\"]]"] {
            let error = load_auto_approve_config(literal);
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "literal: {literal}");
            assert_eq!(
                error.to_string(),
                "project auto_approve_permissions entries must be non-empty strings",
                "literal: {literal}"
            );
        }
    }

    // Corpus case `invalid-auto-approve-permissions-empty-entry`.
    #[test]
    fn rejects_empty_or_whitespace_only_auto_approve_permission_entries() {
        for literal in ["[\"\"]", "[\"   \"]", "[\"bash\", \"\\t\"]", "[\"\\n\"]"] {
            let error = load_auto_approve_config(literal);
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "literal: {literal}");
            assert_eq!(
                error.to_string(),
                "project auto_approve_permissions entries must be non-empty strings",
                "literal: {literal}"
            );
        }
    }

    #[test]
    fn rejects_auto_approve_permission_entries_with_surrounding_whitespace() {
        for literal in [
            "[\" bash\"]",
            "[\"bash \"]",
            "[\" bash \"]",
            "[\"bash\", \" edit\"]",
        ] {
            let error = load_auto_approve_config(literal);
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "literal: {literal}");
            assert_eq!(
                error.to_string(),
                "project auto_approve_permissions entries must not have surrounding whitespace",
                "literal: {literal}"
            );
        }
    }

    // Corpus case `invalid-auto-approve-permissions-external-directory`.
    #[test]
    fn rejects_external_directory_permission_with_guidance() {
        let error = load_auto_approve_config("[\"external_directory\"]");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project external_directory is not accepted in auto_approve_permissions; use auto_approve_external_directories instead"
        );
    }

    #[test]
    fn external_directory_rejection_precedes_deduplication() {
        let error = load_auto_approve_config("[\"bash\", \"external_directory\"]");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project external_directory is not accepted in auto_approve_permissions; use auto_approve_external_directories instead"
        );
    }

    #[test]
    fn auto_approve_permission_errors_do_not_leak_supplied_values() {
        const SECRET: &str = "secret-permission-value";

        let entry_error = load_auto_approve_config(&format!("[\"{SECRET} \"]"));
        let rendered = format!("{entry_error} {entry_error:?}");
        assert!(!rendered.contains(SECRET), "leaked permission: {rendered}");

        let type_error = load_auto_approve_config(&format!("\"{SECRET}\""));
        let rendered = format!("{type_error} {type_error:?}");
        assert!(!rendered.contains(SECRET), "leaked permission: {rendered}");
    }
}
