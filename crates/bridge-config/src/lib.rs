//! Loader and validation for the `projects.toml` configuration file.
//!
//! The crate implements the structural loading stage (task 2.1), the
//! project-id/workspace validation group (task 2.2), the endpoint/port
//! validation group (task 2.3), the cross-project uniqueness group (task 2.4),
//! the max-rounds/model/optional-path group (task 2.5), the
//! auto-approve-permissions group (task 2.6), the trusted-external-directory
//! group (task 2.7) and the credential-file reader (task 2.8). It reads a
//! UTF-8 TOML file from an explicit path, requires a top-level `projects` table
//! and, for every project entry, validates the
//! project id against `^[a-z0-9][a-z0-9_-]{0,63}$`, resolves the `workspace` to
//! an existing directory, parses the required `opencode_url` and the optional
//! `mcp_url` into typed loopback endpoints, requires a positive integer
//! `max_rounds`, parses the optional `opencode_model` and `opencode_env_file`
//! and collects the optional `auto_approve_permissions` and
//! `auto_approve_external_directories`. Relative workspaces and relative
//! `opencode_env_file` paths resolve against the directory that contains the
//! specific `projects.toml`, never against the process working directory. After
//! the individual projects pass, the loader rejects a canonical workspace, a
//! server endpoint or an MCP token file that is reused, and an MCP token file
//! that coincides with a password file. Relative `password_file` and
//! `mcp_token_file` paths are resolved the same lexical way and exposed as
//! typed [`CredentialPath`] values.
//!
//! `auto_approve_permissions` is an optional TOML array of ordinary permission
//! names. Every entry must be a non-empty string without surrounding whitespace,
//! the reserved name `external_directory` is rejected in favour of
//! `auto_approve_external_directories`, and duplicates collapse while preserving
//! the first-seen order. An absent key yields an empty collection.
//!
//! `auto_approve_external_directories` is an optional TOML array of trusted
//! external directory roots. Every entry must be a non-empty string that names
//! an existing, absolute directory; the filesystem root `/` is rejected, each
//! entry is canonicalized (resolving symlink aliases) and duplicates collapse
//! while preserving the first-seen order. An absent key yields an empty
//! collection. [`Config::linked_projects`] then binds a trusted root to a
//! registered project whose canonical workspace is exactly that root; a trusted
//! root without a registered project never becomes a task target.
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
//! The credentials reader (task 2.8) reads the resolved `password_file` and the
//! optional `mcp_token_file` through [`ProjectEntry::read_password`] and
//! [`ProjectEntry::read_mcp_token`], returning a redacting [`Secret`]. The read
//! is fail-closed and mirrors the reference implementation
//! (`src/agent_bridge/credentials.py`): the file must exist, must be a regular
//! file that is not a symlink, must be owned by the current user and must not
//! grant group/other access; its bytes must be UTF-8 and hold exactly one
//! non-empty line (one trailing newline is removed). The project env reader
//! (task 2.9) reads the resolved `opencode_env_file` through
//! [`ProjectEntry::read_opencode_env`] (or [`ProjectEnvFile::read`]), returning
//! a redacting [`ProjectEnv`]. It mirrors the reference implementation
//! (`src/agent_bridge/project_env.py`): the file is opened exactly once without
//! following a final-component symlink, the same descriptor is validated
//! (regular file, current-user owner, mode exactly `0600`) and read, the bytes
//! must be UTF-8 and the content is a flat `NAME=value` mapping with comments,
//! blank lines, first-`=` splitting, no expansion or shell interpretation and
//! fail-closed rejection of NUL bytes, invalid names, duplicate names and
//! reserved bridge service names. The raw per-project table is preserved on
//! [`ProjectEntry::values`] so that a later task can inspect every key and value
//! without re-parsing.
//!
//! The execution-mode group (task 2.10) exposes a typed [`ExecutionMode`] via
//! [`ProjectEntry::execution_mode`]. An absent key defaults to `direct`; only
//! exact `direct` and `worktree` strings are accepted. The optional boolean
//! `allow_parallel_writers` may be true only in worktree mode. Admission
//! settings (task 2.11) expose a positive `max_active_tasks` (default `1`) and
//! the validated `allow_parallel_writers` flag (default `false`). These settings
//! do not reserve writer slots or start concurrent workers.
//!
//! Executor profiles (task 2.12) merge built-in and validated custom
//! [`ProfileDefinition`] values. [`ProjectEntry::resolve_profile`] chooses an
//! explicit request, the project default or the historical implementer;
//! [`ProjectEntry::profile_snapshot`] pins that selection and its effective
//! model, preferring the profile model over the project model. Profile
//! instructions reject detected secrets, disallowed controls and excessive
//! length. Definitions cannot add permissions or expand task scope.
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

use bridge_domain::{DeliveryMode, DomainError, ExecutionMode, ProjectId, Result};
mod delivery;
mod state_approval;
pub use state_approval::state_directory_permission_pattern;
use url::Url;

mod profile_secrets;
mod profiles;
pub use profiles::{
    BUILTIN_PROFILE_VERSION, CUSTOM_PROFILE_VERSION, DEFAULT_PROFILE_ID, ProfileDefinition,
    ResolvedProfile, builtin_profiles,
};

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

/// The optional per-project key that selects direct or worktree execution.
pub const EXECUTION_MODE_KEY: &str = "execution_mode";

/// The optional opt-in key subject to the worktree-only parallel-writer gate.
pub const ALLOW_PARALLEL_WRITERS_KEY: &str = "allow_parallel_writers";

/// The optional per-project bound on unfinished tasks, including waiting tasks.
pub const MAX_ACTIVE_TASKS_KEY: &str = "max_active_tasks";

/// Historical admission default: at most one unfinished task per project.
pub const DEFAULT_MAX_ACTIVE_TASKS: u64 = 1;

/// The optional per-project key that selects the OpenCode model.
pub const OPENCODE_MODEL_KEY: &str = "opencode_model";

/// The optional per-project key that names the OpenCode env file.
pub const OPENCODE_ENV_FILE_KEY: &str = "opencode_env_file";

/// The optional per-project key that lists ordinary permissions to auto-approve.
pub const AUTO_APPROVE_PERMISSIONS_KEY: &str = "auto_approve_permissions";

/// The optional per-project key that lists trusted external directory roots.
pub const AUTO_APPROVE_EXTERNAL_DIRECTORIES_KEY: &str = "auto_approve_external_directories";

/// The reserved permission name that must go through the trusted-directory key.
const EXTERNAL_DIRECTORY_PERMISSION: &str = "external_directory";

/// The per-project key that names the OpenCode password file.
const PASSWORD_FILE_KEY: &str = "password_file";

/// The optional per-project key that names the MCP bearer-token file.
const MCP_TOKEN_FILE_KEY: &str = "mcp_token_file";

/// The bridge service variables an env file must never define.
///
/// The parser rejects these fail-closed; the reference overlay additionally
/// restores them from the inherited environment. A project env file can
/// therefore never shadow the bridge's own service credentials.
const PROTECTED_ENV_NAMES: [&str; 3] = [
    "OPENCODE_SERVER_PASSWORD",
    "OPENCODE_SERVER_USERNAME",
    "AGENT_BRIDGE_MCP_TOKEN",
];

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
    /// Constructs a canonical loopback endpoint for a task-scoped runtime.
    /// # Errors
    /// Port zero is never a usable persisted server endpoint.
    pub fn loopback(port: u16) -> Result<Self> {
        if port == 0 {
            return Err(DomainError::invalid_input("endpoint port must be positive"));
        }
        Ok(Self { port })
    }
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
    /// Validates a frozen or configured `provider/model` selector.
    /// # Errors
    /// Rejects absent components and surrounding whitespace.
    pub fn parse(raw: &str) -> Result<Self> {
        parse_opencode_model(raw)
    }

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

/// A resolved credential file path.
///
/// Credential files are resolved once while the configuration loads: an
/// absolute path is used as-is and a relative path is joined onto the directory
/// that contains the specific `projects.toml`. The spelling is kept lexical so
/// the reader can still detect a symlinked file; the cross-project uniqueness
/// rules compare a separate, symlink-expanded resolution of the same path. The
/// file itself is not required to exist until it is read.
///
/// The [`Debug`](fmt::Debug) representation never renders the path, because a
/// credential location can itself be sensitive.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialPath {
    path: PathBuf,
}

impl CredentialPath {
    /// Wraps an already resolved credential path.
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Returns the resolved path.
    ///
    /// A relative configured path has already been joined onto the directory
    /// that contains `projects.toml`.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// Reads and validates the credential file, returning its secret value.
    ///
    /// The file is opened exactly once with a fail-closed, no-symlink open; the
    /// same descriptor is validated and then read, so the checks always
    /// describe the inode that was actually read.
    ///
    /// # Errors
    ///
    /// Returns a safe [`DomainError`] when the file is missing, is a symlink,
    /// is not a regular file, is not owned by the current user, has group/other
    /// permission bits set, is not valid UTF-8, is empty or holds more than one
    /// line. The error never renders the path or the file contents.
    pub fn read(&self) -> Result<Secret> {
        read_credential(&self.path)
    }
}

impl fmt::Debug for CredentialPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialPath([redacted])")
    }
}

/// A validated credential value.
///
/// The value is the single non-empty line read from a credential file. It is
/// never rendered by [`Debug`](fmt::Debug) or [`Display`](fmt::Display); a
/// caller must request it explicitly through [`Secret::expose_secret`], so a
/// secret cannot leak into logs, commands or process records by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    value: String,
}

impl Secret {
    /// Wraps an already validated credential value.
    fn new(value: String) -> Self {
        Self { value }
    }

    /// Exposes the secret value.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.value
    }

    /// Returns `true` when the value is empty.
    ///
    /// A [`Secret`] produced by the reader is never empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// An optional, resolved `opencode_env_file` location.
///
/// The path is stored exactly as the loader resolved it: an absolute configured
/// path is preserved verbatim and a relative one is joined onto the directory
/// that contains `projects.toml`. The file itself is not required to exist until
/// it is read. The [`Debug`](fmt::Debug) representation never renders the path,
/// because an env file location can itself be sensitive.
#[derive(Clone, PartialEq, Eq)]
pub struct ProjectEnvFile {
    path: PathBuf,
}

impl ProjectEnvFile {
    /// Wraps an already resolved env-file path.
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Returns the resolved path.
    ///
    /// A relative configured path has already been joined onto the directory
    /// that contains `projects.toml`.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// Reads and validates the env file, returning its redacting mapping.
    ///
    /// The file is opened exactly once with a fail-closed, no-symlink open; the
    /// same descriptor is validated and then read, so the checks always describe
    /// the inode that was actually read.
    ///
    /// # Errors
    ///
    /// Returns a safe [`DomainError`] when the file is missing, is a symlink, is
    /// not a regular file, is not owned by the current user, does not have mode
    /// exactly `0600`, is not valid UTF-8, or holds a malformed, unsafe or
    /// duplicate env line. The error never renders the path, a variable name or
    /// a variable value.
    pub fn read(&self) -> Result<ProjectEnv> {
        read_project_env(&self.path)
    }
}

impl fmt::Debug for ProjectEnvFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProjectEnvFile([redacted])")
    }
}

/// A validated per-project OpenCode environment mapping.
///
/// The mapping holds the `NAME=value` pairs of one `opencode_env_file`, with
/// names in deterministic (lexicographic) order. Values are provider API keys,
/// so neither [`Debug`](fmt::Debug) nor [`Display`](fmt::Display) renders them;
/// a caller must request a value explicitly through [`ProjectEnv::get`] or
/// [`ProjectEnv::iter`]. Names are not secret and can be enumerated safely
/// through [`ProjectEnv::names`].
#[derive(Clone, PartialEq, Eq)]
pub struct ProjectEnv {
    variables: BTreeMap<String, String>,
}

impl ProjectEnv {
    /// Wraps an already validated variable mapping.
    fn new(variables: BTreeMap<String, String>) -> Self {
        Self { variables }
    }

    /// Returns `true` when no variable is defined.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty()
    }

    /// Returns the number of defined variables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.variables.len()
    }

    /// Returns the value of `name`, if defined.
    ///
    /// This is the explicit accessor for a value that may be a secret.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.variables.get(name).map(String::as_str)
    }

    /// Returns `true` when `name` is defined.
    #[must_use]
    pub fn contains_key(&self, name: &str) -> bool {
        self.variables.contains_key(name)
    }

    /// Iterates over the defined variable names in deterministic order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.variables.keys().map(String::as_str)
    }

    /// Iterates over the `(name, value)` pairs in deterministic order.
    ///
    /// The values may be secrets and are exposed only through this explicit
    /// iterator.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.variables
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

impl fmt::Debug for ProjectEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectEnv")
            .field("variables", &self.variables.len())
            .finish()
    }
}

impl fmt::Display for ProjectEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// A validated project entry.
///
/// The entry carries the typed, validated [`ProjectId`], the canonical,
/// absolute workspace [`Path`], the typed [`Endpoint`]/[`McpEndpoint`] values,
/// the positive `max_rounds`, the optional [`OpenCodeModel`], the optional
/// resolved [`ProjectEnvFile`], the resolved optional [`CredentialPath`]
/// password/token locations, the deduplicated `auto_approve_permissions` list
/// and the deduplicated, canonical `auto_approve_external_directories` list, so
/// consumers never have to repeat the task 2.2–2.9 validation or re-parse raw
/// values. The raw TOML table is preserved verbatim for any later validation
/// group, which inspects every key and value. Task 2.10 also validates and
/// stores the execution mode, with `direct` as the historical default. Task
/// 2.11 stores the positive unfinished-task bound and parallel-writer opt-in,
/// defaulting to one unfinished task and no parallel writers.
/// Task 2.12 stores merged profile definitions and the optional project default,
/// with immutable profile resolution and effective snapshot creation.
#[derive(Clone)]
pub struct ProjectEntry {
    id: ProjectId,
    workspace: PathBuf,
    opencode_endpoint: Endpoint,
    mcp_endpoint: Option<McpEndpoint>,
    max_rounds: u64,
    execution_mode: ExecutionMode,
    delivery_mode: DeliveryMode,
    max_active_tasks: u64,
    allow_parallel_writers: bool,
    default_profile: Option<String>,
    profile_definitions: BTreeMap<String, ProfileDefinition>,
    opencode_model: Option<OpenCodeModel>,
    opencode_env_file: Option<ProjectEnvFile>,
    password_file: Option<CredentialPath>,
    mcp_token_file: Option<CredentialPath>,
    auto_approve_state_directory: bool,
    auto_approve_permissions: Vec<String>,
    auto_approve_external_directories: Vec<PathBuf>,
    values: toml::Table,
}

impl ProjectEntry {
    /// Returns an execution view with the same credentials and policies, bound
    /// to an already proven task checkout and loopback runtime. No files change.
    /// # Errors
    /// Requires an existing canonical directory; binding proof belongs to caller.
    pub fn execution_view(&self, workspace: &Path, endpoint: Endpoint) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace)
            .map_err(|_| DomainError::invalid_input("execution workspace unavailable"))?;
        if !workspace.is_dir() {
            return Err(DomainError::invalid_input(
                "execution workspace unavailable",
            ));
        }
        let mut view = self.clone();
        view.workspace = workspace;
        view.opencode_endpoint = endpoint;
        Ok(view)
    }
    /// Opt-in state access is separate from trusted external Git directories.
    #[must_use]
    pub fn auto_approve_state_directory(&self) -> bool {
        self.auto_approve_state_directory
    }
    /// Policy used for new submissions; saved task policy remains immutable.
    #[must_use]
    pub fn delivery_mode(&self) -> DeliveryMode {
        self.delivery_mode
    }
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

    /// Returns the validated mode, defaulting to direct when the key is absent.
    /// This setting alone does not start a worktree executor.
    #[must_use]
    pub fn execution_mode(&self) -> ExecutionMode {
        self.execution_mode
    }

    /// Returns the positive bound on unfinished tasks, including waiting tasks.
    /// Defaults to one. A larger bound does not by itself permit parallel writers.
    #[must_use]
    pub fn max_active_tasks(&self) -> u64 {
        self.max_active_tasks
    }

    /// Returns the parallel-writer opt-in, defaulting to false.
    /// A true value is valid only in worktree mode; runtime admission is separate.
    #[must_use]
    pub fn allow_parallel_writers(&self) -> bool {
        self.allow_parallel_writers
    }

    /// Returns the configured default profile, absent for the historical default.
    #[must_use]
    pub fn default_profile(&self) -> Option<&str> {
        self.default_profile.as_deref()
    }

    /// Known immutable definitions, with custom definitions overriding built-ins.
    #[must_use]
    pub fn profile_definitions(&self) -> &BTreeMap<String, ProfileDefinition> {
        &self.profile_definitions
    }

    /// Resolves an explicit id, then the project default, then implementer.
    /// Empty requests mean no explicit selection; unknown ids return None.
    #[must_use]
    pub fn resolve_profile(&self, requested: Option<&str>) -> Option<ResolvedProfile<'_>> {
        profiles::resolve_profile(self, requested)
    }

    /// Builds a submit-time snapshot with the effective model pinned.
    /// Unknown explicit selections return a safe NotFound error. No state is written.
    pub fn profile_snapshot(
        &self,
        requested: Option<&str>,
    ) -> Result<bridge_domain::ProfileSnapshot> {
        self.resolve_profile(requested)
            .map(|resolved| resolved.snapshot(self.opencode_model()))
            .ok_or_else(|| DomainError::not_found("requested profile is unknown"))
    }

    /// Returns the validated optional OpenCode model.
    #[must_use]
    pub fn opencode_model(&self) -> Option<&OpenCodeModel> {
        self.opencode_model.as_ref()
    }

    /// Returns the optional resolved `opencode_env_file`.
    ///
    /// A relative configured path is resolved against the directory that
    /// contains `projects.toml`; an absolute path is preserved verbatim. The
    /// [`ProjectEnvFile`] keeps the path redacted in its [`Debug`](fmt::Debug)
    /// output; the path itself is available explicitly through
    /// [`ProjectEnvFile::as_path`].
    #[must_use]
    pub fn opencode_env_file(&self) -> Option<&ProjectEnvFile> {
        self.opencode_env_file.as_ref()
    }

    /// Returns the resolved `password_file` path, if configured.
    ///
    /// A relative configured path is resolved against the directory that
    /// contains `projects.toml`; an absolute path is preserved verbatim.
    #[must_use]
    pub fn password_file(&self) -> Option<&CredentialPath> {
        self.password_file.as_ref()
    }

    /// Returns the resolved optional `mcp_token_file` path.
    ///
    /// A relative configured path is resolved against the directory that
    /// contains `projects.toml`; an absolute path is preserved verbatim. The
    /// result is `None` when the key is absent.
    #[must_use]
    pub fn mcp_token_file(&self) -> Option<&CredentialPath> {
        self.mcp_token_file.as_ref()
    }

    /// Reads and validates the project password file.
    ///
    /// # Errors
    ///
    /// Returns [`bridge_domain::ErrorKind::NotFound`] when `password_file` is
    /// not configured, plus any error of [`CredentialPath::read`] when it is.
    pub fn read_password(&self) -> Result<Secret> {
        match &self.password_file {
            Some(path) => path.read(),
            None => Err(DomainError::not_found(
                "project password_file is not configured",
            )),
        }
    }

    /// Reads and validates the optional project MCP token file.
    ///
    /// Returns `Ok(None)` when `mcp_token_file` is absent.
    ///
    /// # Errors
    ///
    /// Returns any error of [`CredentialPath::read`] when the key is present.
    pub fn read_mcp_token(&self) -> Result<Option<Secret>> {
        match &self.mcp_token_file {
            Some(path) => path.read().map(Some),
            None => Ok(None),
        }
    }

    /// Reads and validates the optional project OpenCode env file.
    ///
    /// Returns `Ok(None)` when `opencode_env_file` is absent.
    ///
    /// # Errors
    ///
    /// Returns any error of [`ProjectEnvFile::read`] when the key is present.
    pub fn read_opencode_env(&self) -> Result<Option<ProjectEnv>> {
        match &self.opencode_env_file {
            Some(path) => path.read().map(Some),
            None => Ok(None),
        }
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

    /// Returns the trusted external directory roots.
    ///
    /// The slice is empty when the key is absent. Every entry is an existing,
    /// absolute, canonical directory, so symlink aliases have been collapsed and
    /// the filesystem root `/` never appears. Duplicate canonical directories
    /// are collapsed while preserving the order of their first occurrence.
    #[must_use]
    pub fn auto_approve_external_directories(&self) -> &[PathBuf] {
        &self.auto_approve_external_directories
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

    /// Returns the registered projects linked to `id` through its
    /// `auto_approve_external_directories`.
    ///
    /// A linked project is **another** configured project whose canonical
    /// workspace is exactly one of this project's canonical trusted external
    /// directory entries. Matching is exact, so a trusted directory that merely
    /// *contains* a registered workspace, or that has no registered project at
    /// all, never links anything and never becomes a task target. The source
    /// project itself is never linked through its own trusted roots.
    ///
    /// An unknown `id` yields an empty list. The result is ordered by project id
    /// because the backing [`BTreeMap`] iterates in that order, which makes
    /// launcher wiring stable for a given configuration.
    #[must_use]
    pub fn linked_projects(&self, id: &str) -> Vec<&ProjectEntry> {
        let Some(entry) = self.projects.get(id) else {
            return Vec::new();
        };
        self.projects
            .values()
            .filter(|candidate| {
                candidate.id.as_str() != id
                    && entry
                        .auto_approve_external_directories
                        .contains(&candidate.workspace)
            })
            .collect()
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
/// * [`bridge_domain::ErrorKind::NotFound`] when the file does not exist, a
///   configured workspace path does not exist or a configured trusted external
///   directory does not exist;
/// * [`bridge_domain::ErrorKind::PermissionDenied`] when the file, a workspace
///   or a trusted external directory cannot be accessed because of permissions;
/// * [`bridge_domain::ErrorKind::Internal`] for any other I/O failure,
///   including an unresolvable symlink (a loop or an unreadable link) in a
///   credential path or a trusted external directory;
/// * [`bridge_domain::ErrorKind::InvalidInput`] when the bytes are not UTF-8,
///   when the TOML is syntactically invalid, when the `projects` table is
///   missing, when the `projects` value or a project entry has the wrong
///   shape, when a project id does not match
///   `^[a-z0-9][a-z0-9_-]{0,63}$`, when a workspace is missing, is not a
///   string, is empty or is not a directory, when `opencode_url`/`mcp_url`
///   are missing, have the wrong type or are not a loopback `http` endpoint
///   with an explicit port in `1..=65535` and the required path, when
///   `max_rounds` is missing, is not a TOML integer or is not positive, when
///   `execution_mode` is not exactly `direct` or `worktree`, when
///   `max_active_tasks` is not a positive TOML integer, when
///   `allow_parallel_writers` is not a boolean or is true outside worktree mode, when
///   profile definitions have invalid ids, fields, models, text or detected
///   secrets, or `default_profile` does not name a known profile, when
///   `opencode_model` is not a string, is not `'<providerID>/<modelID>'` or has
///   surrounding whitespace around the value or a component, when
///   `opencode_env_file` is not a non-empty string, when
///   `auto_approve_permissions` is not a TOML array of non-empty strings without
///   surrounding whitespace or contains the reserved `external_directory` name,
///   when `auto_approve_external_directories` is not a TOML array of non-empty
///   strings, is not an absolute path, does not exist, is not a directory or is
///   the filesystem root, when an MCP token file is not a non-empty string, or
///   when a canonical workspace, a server endpoint or an MCP token file is
///   reused across projects or an MCP token file equals a password file.
///
/// None of these errors renders the file contents, credential values, project
/// ids, workspace paths, URL inputs or the absolute input path.
pub fn load_config(path: &Path) -> Result<Config> {
    load_config_inner(path, None)
}

/// Loads config with the explicit Rust state namespace for approval validation.
/// Does not create/read runtime state; opt-in requires a narrowly scoped root.
/// # Errors
/// Ordinary config errors and unsafe approval roots return redacted errors.
pub fn load_config_with_state_root(path: &Path, state_root: &Path) -> Result<Config> {
    load_config_inner(path, Some(state_root))
}
fn load_config_inner(path: &Path, state_root: Option<&Path>) -> Result<Config> {
    let bytes = std::fs::read(path).map_err(read_error)?;
    let text = String::from_utf8(bytes).map_err(|source| {
        DomainError::invalid_input("configuration file is not valid UTF-8").with_source(source)
    })?;
    let projects = parse_projects(&text)?;
    let config_dir = path.parent().unwrap_or_else(|| Path::new(""));
    let config = validate_projects(projects, config_dir)?;
    if config
        .projects
        .values()
        .any(|p| p.auto_approve_state_directory)
    {
        let root = state_root.ok_or_else(|| {
            DomainError::invalid_input("state directory approval requires explicit Rust state root")
        })?;
        state_directory_permission_pattern(root)?;
    }
    Ok(config)
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
/// Each project is validated independently first (tasks 2.2/2.3/2.5/2.6/2.7). Only
/// then are the cross-project uniqueness rules of task 2.4 applied: a canonical
/// workspace may not be bound to two projects, a server endpoint may not be
/// reused (neither between two projects nor by the OpenCode and MCP endpoints
/// of the same project, because both use the loopback host and are compared by
/// port), and an MCP token file may not repeat another token file or coincide
/// with a password file.
///
/// The credential paths are resolved like the reference implementation: an
/// absolute path is used as-is and a relative path is joined onto `config_dir`,
/// and the stored [`CredentialPath`] keeps that lexical spelling so the reader
/// can still reject a symlinked file. The uniqueness rules compare a separate,
/// symlink-expanded resolution of the same paths. The files themselves are not
/// required to exist and their contents are never read during loading. The
/// optional `opencode_env_file` is resolved the same lexical way (absolute
/// preserved, relative joined onto `config_dir`) but is not read. Errors are
/// static and never render a project id, workspace, URL, credential path, model
/// value, permission value or config path.
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
        let execution_mode = validate_execution_mode(&values)?;
        let delivery_mode = delivery::parse(&values, execution_mode)?;
        let max_active_tasks = validate_max_active_tasks(&values)?;
        let allow_parallel_writers = validate_allow_parallel_writers(&values, execution_mode)?;
        let opencode_model = validate_opencode_model(&values)?;
        let profile_definitions = profiles::parse_definitions(&values)?;
        let default_profile = profiles::parse_default(&values, &profile_definitions)?;
        let opencode_env_file = validate_opencode_env_file(&values, config_dir)?;
        let auto_approve_state_directory = state_approval::parse(&values)?;
        let auto_approve_permissions = validate_auto_approve_permissions(&values)?;
        let auto_approve_external_directories =
            validate_auto_approve_external_directories(&values)?;

        let password = resolve_password_file(&values, config_dir);
        let token = resolve_mcp_token_file(&values, config_dir)?;
        credentials.push((
            resolve_credential_for_comparison(&password)?,
            resolve_credential_for_comparison(&token)?,
        ));

        projects.insert(
            raw_id,
            ProjectEntry {
                id,
                workspace,
                opencode_endpoint,
                mcp_endpoint,
                max_rounds,
                execution_mode,
                delivery_mode,
                max_active_tasks,
                allow_parallel_writers,
                default_profile,
                profile_definitions,
                opencode_model,
                opencode_env_file: opencode_env_file.map(ProjectEnvFile::new),
                password_file: password.map(CredentialPath::new),
                mcp_token_file: token.map(CredentialPath::new),
                auto_approve_state_directory,
                auto_approve_permissions,
                auto_approve_external_directories,
                values,
            },
        );
    }

    validate_credentials(&credentials)?;

    Ok(Config { projects })
}

/// Parses the exact domain vocabulary without whitespace normalization.
fn validate_execution_mode(values: &toml::Table) -> Result<ExecutionMode> {
    let Some(value) = values.get(EXECUTION_MODE_KEY) else {
        return Ok(ExecutionMode::default());
    };
    let raw = value
        .as_str()
        .ok_or_else(|| DomainError::invalid_input("project execution_mode must be a string"))?;
    if raw.trim() != raw {
        return Err(DomainError::invalid_input(
            "project execution_mode must not have surrounding whitespace",
        ));
    }
    ExecutionMode::try_from(raw.to_owned()).map_err(|_| {
        DomainError::invalid_input("project execution_mode must be direct or worktree")
    })
}

/// Parses the optional positive TOML integer without boolean/string coercion.
fn validate_max_active_tasks(values: &toml::Table) -> Result<u64> {
    let Some(value) = values.get(MAX_ACTIVE_TASKS_KEY) else {
        return Ok(DEFAULT_MAX_ACTIVE_TASKS);
    };
    let count = value.as_integer().and_then(|raw| u64::try_from(raw).ok());
    count.filter(|count| *count > 0).ok_or_else(|| {
        DomainError::invalid_input("project max_active_tasks must be a positive integer")
    })
}

/// Parses the opt-in with its historical default and the task 2.10 mode gate.
fn validate_allow_parallel_writers(values: &toml::Table, mode: ExecutionMode) -> Result<bool> {
    let Some(value) = values.get(ALLOW_PARALLEL_WRITERS_KEY) else {
        return Ok(false);
    };
    let parallel = value.as_bool().ok_or_else(|| {
        DomainError::invalid_input("project allow_parallel_writers must be a boolean")
    })?;
    if parallel && mode != ExecutionMode::Worktree {
        return Err(DomainError::invalid_input(
            "project allow_parallel_writers=true requires execution_mode=worktree",
        ));
    }
    Ok(parallel)
}

/// The static error for a reused server endpoint.
fn duplicate_endpoint_error() -> DomainError {
    DomainError::invalid_input("project endpoint is already used by another project")
}

/// Resolves the optional `password_file` into its lexical configured path.
///
/// The key is optional here because the required-key rule belongs to a later
/// task; when it is absent or not a string this helper simply reports no
/// password path. The value is never rendered. The returned path is only
/// joined onto `config_dir`; symlink expansion happens later and only for the
/// uniqueness comparison, so the reader can still reject a symlinked file.
fn resolve_password_file(values: &toml::Table, config_dir: &Path) -> Option<PathBuf> {
    let raw = values.get(PASSWORD_FILE_KEY)?.as_str()?;
    Some(join_credential_path(raw, config_dir))
}

/// Resolves the optional `mcp_token_file` into its lexical configured path.
///
/// The token must be a non-empty string when present. Its contents are never
/// read; only a symlink-expanded form of the path participates in the
/// uniqueness rules. The returned path is only joined onto `config_dir`;
/// symlink expansion happens later and only for the uniqueness comparison.
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
    Ok(Some(join_credential_path(raw, config_dir)))
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

/// Joins a configured credential path onto the config directory.
///
/// An absolute path is used as-is; a relative path is joined onto the directory
/// that contains the specific `projects.toml`. This is the lexical path the
/// reference implementation stores on its project config and later opens, so a
/// symlinked credential file is still visible to the reader.
fn join_credential_path(raw: &str, config_dir: &Path) -> PathBuf {
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        config_dir.join(candidate)
    }
}

/// Resolves an optional credential path for the uniqueness comparison.
///
/// The reference implementation compares `Path.resolve()` values, so an
/// existing symlink alias or `..` must collapse before two credential files are
/// compared. This never changes the path stored for reading.
///
/// # Errors
///
/// Returns a safe [`DomainError`] when a symlink in the path cannot be read or
/// the walk exceeds [`MAX_SYMLINK_HOPS`].
fn resolve_credential_for_comparison(path: &Option<PathBuf>) -> Result<Option<PathBuf>> {
    path.as_deref().map(resolve_non_strict).transpose()
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

/// Validates the optional `auto_approve_external_directories` TOML array.
///
/// The key is optional; when absent the project trusts no external directory and
/// an empty list is returned. When present the value must be a TOML array and
/// every entry must be a non-empty string naming an existing, absolute
/// directory. The entry is canonicalized with a strict resolve, which expands
/// every symlink alias and requires the whole path to exist, so a symlinked
/// spelling is stored as its real target and cannot widen the trusted area. The
/// filesystem root `/` is rejected. Duplicates collapse onto the first-seen
/// canonical directory. All error messages are static and never contain a
/// project id, a supplied directory value, config contents or a path.
fn validate_auto_approve_external_directories(values: &toml::Table) -> Result<Vec<PathBuf>> {
    let Some(value) = values.get(AUTO_APPROVE_EXTERNAL_DIRECTORIES_KEY) else {
        return Ok(Vec::new());
    };
    let array = value.as_array().ok_or_else(|| {
        DomainError::invalid_input(
            "project auto_approve_external_directories must be a list of strings",
        )
    })?;

    let mut directories: Vec<PathBuf> = Vec::new();
    for item in array {
        let raw = item.as_str().ok_or_else(|| {
            DomainError::invalid_input(
                "project auto_approve_external_directories entries must be non-empty strings",
            )
        })?;
        if raw.trim().is_empty() {
            return Err(DomainError::invalid_input(
                "project auto_approve_external_directories entries must be non-empty strings",
            ));
        }
        let candidate = Path::new(raw);
        if !candidate.is_absolute() {
            return Err(DomainError::invalid_input(
                "project auto_approve_external_directories entries must be absolute paths",
            ));
        }

        let canonical = std::fs::canonicalize(candidate).map_err(external_directory_error)?;
        if !canonical.is_dir() {
            return Err(DomainError::invalid_input(
                "project external directory is not a directory",
            ));
        }
        if is_filesystem_root(&canonical) {
            return Err(DomainError::invalid_input(
                "project filesystem root is not a trusted external directory",
            ));
        }
        if !directories.contains(&canonical) {
            directories.push(canonical);
        }
    }
    Ok(directories)
}

/// Returns `true` when `path` is the root of a filesystem (`/` or `C:\`).
///
/// A root path has exactly one component, which is the [`Component::RootDir`] or
/// a [`Component::Prefix`]. This is checked on the canonical path so that an
/// alias like `/./` or `/tmp/..` cannot disguise the root.
fn is_filesystem_root(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        components.next(),
        Some(Component::RootDir | Component::Prefix(_))
    ) && components.next().is_none()
}

/// Maps an external-directory resolution failure to a safe, typed
/// [`DomainError`].
///
/// The message never renders the directory value; the underlying
/// [`std::io::Error`] is kept only as the error source, so its path-bearing
/// [`Display`](std::fmt::Display) text never reaches the safe output.
fn external_directory_error(source: std::io::Error) -> DomainError {
    match source.kind() {
        std::io::ErrorKind::NotFound => {
            DomainError::not_found("project external directory does not exist").with_source(source)
        }
        std::io::ErrorKind::PermissionDenied => {
            DomainError::permission_denied("project external directory could not be accessed")
                .with_source(source)
        }
        _ => DomainError::internal("project external directory could not be resolved")
            .with_source(source),
    }
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

/// The `O_NOFOLLOW` open flag.
///
/// The standard library does not expose this flag and the crate avoids a native
/// `libc` dependency, so the stable `fcntl.h` value is spelled out per Unix
/// family. The flag makes the credential open fail closed instead of following
/// a final-component symlink.
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0o400000;

#[cfg(all(unix, not(target_os = "linux")))]
const O_NOFOLLOW: i32 = 0o100;

/// Opens a credential file without ever following a final-component symlink.
///
/// The open is fail-closed: a symlink at the credential path is refused by the
/// kernel instead of being resolved, so a swap between validation and reading
/// cannot redirect the read. The returned descriptor is validated and read
/// exactly once; the path is never reopened.
#[cfg(unix)]
fn open_credential(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|source| credential_open_error(path, source))
}

#[cfg(not(unix))]
fn open_credential(path: &Path) -> Result<std::fs::File> {
    std::fs::File::open(path).map_err(|source| credential_open_error(path, source))
}

/// Maps a credential-file open failure to a safe, typed [`DomainError`].
///
/// The message never renders the path; the underlying [`std::io::Error`] is
/// retained only as the error source. A refused symlink (`ELOOP`, or a plain
/// symlink on a platform without `O_NOFOLLOW`) is reported as the symlink
/// violation of the reference reader.
fn credential_open_error(path: &Path, source: std::io::Error) -> DomainError {
    match source.kind() {
        std::io::ErrorKind::NotFound => {
            DomainError::not_found("credential file not found").with_source(source)
        }
        std::io::ErrorKind::PermissionDenied => {
            DomainError::permission_denied("credential file could not be read").with_source(source)
        }
        _ => {
            let is_symlink = std::fs::symlink_metadata(path)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false);
            if is_symlink {
                DomainError::invalid_input("credential file must not be a symlink")
                    .with_source(source)
            } else {
                DomainError::internal("credential file could not be read").with_source(source)
            }
        }
    }
}

/// Validates the metadata of the already-opened credential descriptor.
///
/// The checks mirror `credentials._validate` on the inode that was actually
/// opened: a regular file owned by the current user with no group/other
/// permission bits. The message never renders the path.
#[cfg(unix)]
fn validate_credential_metadata(metadata: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.is_file() {
        return Err(DomainError::invalid_input(
            "credential path is not a regular file",
        ));
    }
    if metadata.uid() != current_effective_uid()? {
        return Err(DomainError::invalid_input(
            "credential file must be owned by the current user",
        ));
    }
    if (metadata.mode() & 0o077) != 0 {
        return Err(DomainError::invalid_input(
            "credential file permissions too open (want 600)",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_credential_metadata(metadata: &std::fs::Metadata) -> Result<()> {
    if !metadata.is_file() {
        return Err(DomainError::invalid_input(
            "credential path is not a regular file",
        ));
    }
    Ok(())
}

/// Returns the effective user id of the current process.
///
/// It is read from `/proc/self/status` so the reader needs no native `geteuid`
/// binding and stays free of `unsafe`. The lookup fails closed when the
/// effective uid cannot be determined, because the ownership rule must never
/// silently pass.
#[cfg(all(unix, target_os = "linux"))]
fn current_effective_uid() -> Result<u32> {
    let status = std::fs::read_to_string("/proc/self/status").map_err(owner_check_error)?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:")
            && let Some(uid) = rest
                .split_whitespace()
                .nth(1)
                .and_then(|raw| raw.parse().ok())
        {
            return Ok(uid);
        }
    }
    Err(owner_check_error(std::io::Error::other(
        "effective uid is unavailable",
    )))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn current_effective_uid() -> Result<u32> {
    Err(DomainError::internal(
        "credential file owner could not be verified",
    ))
}

/// Maps a failed effective-uid lookup to a safe, typed [`DomainError`].
#[cfg(all(unix, target_os = "linux"))]
fn owner_check_error(source: std::io::Error) -> DomainError {
    DomainError::internal("credential file owner could not be verified").with_source(source)
}

/// Reads and validates a credential file, returning its secret value.
///
/// The file is opened once without following a symlink, the descriptor is
/// validated, and the same descriptor is read, so a path swap cannot make the
/// validation describe a different inode than the one read. The bytes must be
/// UTF-8 and hold exactly one non-empty line; one trailing newline is removed,
/// matching the reference reader.
fn read_credential(path: &Path) -> Result<Secret> {
    let mut file = open_credential(path)?;
    let metadata = file.metadata().map_err(credential_read_error)?;
    validate_credential_metadata(&metadata)?;
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut data).map_err(credential_read_error)?;
    decode_credential(&data).map(Secret::new)
}

/// Maps a credential read failure to a safe, typed [`DomainError`].
fn credential_read_error(source: std::io::Error) -> DomainError {
    DomainError::internal("credential file could not be read").with_source(source)
}

/// Decodes credential bytes with the reference reader's content semantics.
///
/// Exactly one trailing `\n` is removed; the remaining value must be non-empty
/// and must not contain a further `\n` or any `\r`.
fn decode_credential(data: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(data).map_err(|source| {
        DomainError::invalid_input("credential file is not valid UTF-8").with_source(source)
    })?;
    let value = text.strip_suffix('\n').unwrap_or(text);
    if value.is_empty() {
        return Err(DomainError::invalid_input("credential file is empty"));
    }
    if value.contains('\n') || value.contains('\r') {
        return Err(DomainError::invalid_input(
            "credential file must contain a single line",
        ));
    }
    Ok(value.to_owned())
}

/// Opens an env file without ever following a final-component symlink.
///
/// The open is fail-closed: a symlink at the env path is refused by the kernel
/// instead of being resolved, so a swap between validation and reading cannot
/// redirect the read. The returned descriptor is validated and read exactly
/// once; the path is never reopened.
#[cfg(unix)]
fn open_project_env(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|source| project_env_open_error(path, source))
}

#[cfg(not(unix))]
fn open_project_env(path: &Path) -> Result<std::fs::File> {
    std::fs::File::open(path).map_err(|source| project_env_open_error(path, source))
}

/// Maps an env-file open failure to a safe, typed [`DomainError`].
///
/// The message never renders the path; the underlying [`std::io::Error`] is
/// retained only as the error source. A refused symlink (`ELOOP`, or a plain
/// symlink on a platform without `O_NOFOLLOW`) is reported as the symlink
/// violation, and a directory that the kernel refused to open is reported as a
/// non-regular file, both matching the reference reader's fail-closed branches.
fn project_env_open_error(path: &Path, source: std::io::Error) -> DomainError {
    match source.kind() {
        std::io::ErrorKind::NotFound => {
            DomainError::not_found("project env file not found").with_source(source)
        }
        std::io::ErrorKind::PermissionDenied => {
            DomainError::permission_denied("project env file could not be read").with_source(source)
        }
        _ => match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                DomainError::invalid_input("project env file must not be a symlink")
                    .with_source(source)
            }
            Ok(metadata) if metadata.is_dir() => {
                DomainError::invalid_input("project env path is not a regular file")
                    .with_source(source)
            }
            _ => DomainError::internal("project env file could not be read").with_source(source),
        },
    }
}

/// Validates the metadata of the already-opened env descriptor.
///
/// The checks mirror `project_env._validate_fd` on the inode that was actually
/// opened: a regular file owned by the current user with mode exactly `0600`
/// (special permission bits included). The message never renders the path.
#[cfg(unix)]
fn validate_project_env_metadata(metadata: &std::fs::Metadata) -> Result<()> {
    validate_project_env_metadata_owned_by(metadata, current_effective_uid()?)
}

/// The owner-parameterized core of [`validate_project_env_metadata`].
///
/// Splitting the expected owner out keeps the ownership branch directly
/// testable without needing a second local user.
#[cfg(unix)]
fn validate_project_env_metadata_owned_by(
    metadata: &std::fs::Metadata,
    expected_uid: u32,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.is_file() {
        return Err(DomainError::invalid_input(
            "project env path is not a regular file",
        ));
    }
    if metadata.uid() != expected_uid {
        return Err(DomainError::invalid_input(
            "project env file must be owned by the current user",
        ));
    }
    if (metadata.mode() & 0o7777) != 0o600 {
        return Err(DomainError::invalid_input(
            "project env file permissions must be exactly 600",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_project_env_metadata(metadata: &std::fs::Metadata) -> Result<()> {
    if !metadata.is_file() {
        return Err(DomainError::invalid_input(
            "project env path is not a regular file",
        ));
    }
    Ok(())
}

/// Reads and validates an env file, returning its redacting mapping.
///
/// The file is opened once without following a symlink, the descriptor is
/// validated, and the same descriptor is read, so a path swap cannot make the
/// validation describe a different inode than the one read. The bytes must be
/// valid UTF-8; the content is parsed by [`parse_project_env`]. Errors never
/// render the path, a variable name or a variable value.
fn read_project_env(path: &Path) -> Result<ProjectEnv> {
    let mut file = open_project_env(path)?;
    let metadata = file.metadata().map_err(project_env_read_error)?;
    validate_project_env_metadata(&metadata)?;
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut data).map_err(project_env_read_error)?;
    let text = std::str::from_utf8(&data).map_err(|source| {
        DomainError::invalid_input("project env file is not valid UTF-8").with_source(source)
    })?;
    parse_project_env(text)
}

/// Maps an env-file read failure to a safe, typed [`DomainError`].
fn project_env_read_error(source: std::io::Error) -> DomainError {
    DomainError::internal("project env file could not be read").with_source(source)
}

/// Returns `true` when `name` is a valid environment variable name.
///
/// Mirrors the reference `^[A-Za-z_][A-Za-z0-9_]*$` pattern: an ASCII letter or
/// underscore followed by ASCII letters, digits or underscores.
fn is_valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(b'A'..=b'Z' | b'a'..=b'z' | b'_') => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Splits `text` into lines with the reference reader's semantics.
///
/// Python's `str.splitlines()` breaks on `\n`, `\r`, `\r\n`, `\v`, `\f`,
/// `\x1c`–`\x1e`, `\x85`, `\u2028` and `\u2029`, and never yields a trailing
/// empty element after a final line break. Reproducing it exactly keeps the
/// Rust parser byte-for-byte compatible with the reference on unusual input.
fn split_env_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        let boundary = match ch {
            '\n' | '\u{000b}' | '\u{000c}' | '\u{001c}' | '\u{001d}' | '\u{001e}' | '\u{0085}'
            | '\u{2028}' | '\u{2029}' => true,
            '\r' => {
                if let Some((_, '\n')) = chars.peek() {
                    chars.next();
                }
                true
            }
            _ => false,
        };
        if boundary {
            lines.push(&text[start..index]);
            start = chars.peek().map_or(text.len(), |(next, _)| *next);
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Parses env text into a validated, redacting [`ProjectEnv`].
///
/// The parser mirrors `project_env._parse`: blank lines and `#` comments are
/// skipped; each remaining line must contain `=`; the split is on the first `=`
/// only and the value is kept verbatim (no quoting, expansion or shell
/// interpretation); the name must be a valid environment variable name; NUL
/// bytes, reserved bridge service names and duplicate names are rejected
/// fail-closed. Every error message is static and never contains a variable
/// name or value.
fn parse_project_env(text: &str) -> Result<ProjectEnv> {
    let mut variables: BTreeMap<String, String> = BTreeMap::new();
    for line in split_env_lines(text) {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        if line.contains('\0') {
            return Err(DomainError::invalid_input(
                "project env file line contains a NUL byte",
            ));
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(DomainError::invalid_input(
                "project env file line must be NAME=value",
            ));
        };
        if !is_valid_env_name(name) {
            return Err(DomainError::invalid_input(
                "project env file line has an invalid variable name",
            ));
        }
        if PROTECTED_ENV_NAMES.contains(&name) {
            return Err(DomainError::invalid_input(
                "project env file must not define a reserved agent-bridge service variable",
            ));
        }
        if variables.contains_key(name) {
            return Err(DomainError::invalid_input(
                "project env file defines the same variable name twice",
            ));
        }
        variables.insert(name.to_owned(), value.to_owned());
    }
    Ok(ProjectEnv::new(variables))
}

#[cfg(test)]
mod tests {
    use super::{
        AUTO_APPROVE_EXTERNAL_DIRECTORIES_KEY, AUTO_APPROVE_PERMISSIONS_KEY, Config,
        EXECUTION_MODE_KEY, MAX_ROUNDS_KEY, MCP_URL_KEY, OPENCODE_ENV_FILE_KEY, OPENCODE_MODEL_KEY,
        OPENCODE_URL_KEY, PROJECTS_TABLE, PROTECTED_ENV_NAMES, ProjectEnv, WORKSPACE_KEY,
        load_config, parse_project_env, parse_projects, split_env_lines,
    };
    use bridge_domain::{DomainError, ErrorKind, ExecutionMode, ProjectId};
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Corpus case `valid-minimal-project`: required keys only.
    const MINIMAL: &str = "[projects.proj]\nworkspace = \"ws\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = 3\n";

    /// A temporary directory removed recursively on drop.
    pub(super) struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        pub(super) fn new(tag: &str) -> Self {
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

        pub(super) fn mkdir(&self, name: &str) -> PathBuf {
            let path = self.path.join(name);
            std::fs::create_dir_all(&path).expect("temporary subdirectory must be creatable");
            path
        }

        pub(super) fn write(&self, name: &str, text: &str) -> PathBuf {
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

    /// Writes credential bytes and forces an explicit Unix mode.
    fn write_credential(dir: &TempDir, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("credential must be writable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("credential mode must be settable");
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
        }
        path
    }

    /// Builds a minimal project table with a quoted id key and `workspace`.
    fn project_toml(id: &str, workspace: &str) -> String {
        format!(
            "[projects.\"{id}\"]\nworkspace = \"{workspace}\"\nopencode_url = \"http://127.0.0.1:4101\"\npassword_file = \"secrets/proj.password\"\nmax_rounds = 3\n"
        )
    }

    /// Builds a project table with an explicit `opencode_url` and extra keys.
    pub(super) fn project_toml_with(
        id: &str,
        workspace: &str,
        opencode_url: &str,
        extra: &str,
    ) -> String {
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
        assert_eq!(EXECUTION_MODE_KEY, "execution_mode");
    }

    #[test]
    fn execution_mode_defaults_and_explicit_modes_preserve_raw_values() {
        let dir = TempDir::new("execution-mode-valid");
        let workspace = dir.mkdir("ws");
        for (extra, expected, raw) in [
            ("", ExecutionMode::Direct, None),
            (
                "execution_mode = 'direct'\n",
                ExecutionMode::Direct,
                Some("direct"),
            ),
            (
                "execution_mode = 'worktree'\n",
                ExecutionMode::Worktree,
                Some("worktree"),
            ),
        ] {
            let path = dir.write(
                "projects.toml",
                &project_toml_with(
                    "proj",
                    workspace.to_str().unwrap(),
                    "http://127.0.0.1:4101",
                    extra,
                ),
            );
            let config = load_config(&path).expect("valid execution mode");
            let entry = config.project("proj").unwrap();
            assert_eq!(entry.execution_mode(), expected);
            assert_eq!(entry.max_active_tasks(), 1);
            assert!(!entry.allow_parallel_writers());
            assert_eq!(
                entry.get(EXECUTION_MODE_KEY).and_then(toml::Value::as_str),
                raw
            );
            assert_eq!(entry.contains_key(EXECUTION_MODE_KEY), raw.is_some());
            assert_eq!(entry.clone().execution_mode(), expected);
        }
    }

    #[test]
    fn execution_mode_rejects_non_strings() {
        for raw in [
            "true",
            "false",
            "3",
            "1.5",
            "[]",
            "['worktree']",
            "{}",
            "1979-05-27",
        ] {
            let error = load_endpoint_config(
                "proj",
                "http://127.0.0.1:4101",
                &format!("execution_mode = {raw}\n"),
            );
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert_eq!(error.message(), "project execution_mode must be a string");
        }
    }

    #[test]
    fn execution_mode_rejects_whitespace_case_and_ambiguous_strings() {
        for raw in [
            "'direct '",
            "' direct'",
            "'worktree '",
            "' worktree'",
            "'Direct'",
            "'WORKTREE'",
            "'Worktree'",
            "''",
            "'none'",
            "'default'",
            "'auto'",
            "\"direct\\t\"",
            "\"worktree\\n\"",
            "'direct\\t'",
            "'direct\u{a0}'",
        ] {
            let error = load_endpoint_config(
                "proj",
                "http://127.0.0.1:4101",
                &format!("execution_mode = {raw}\n"),
            );
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert!(error.message().starts_with("project execution_mode"));
        }
    }

    #[test]
    fn parallel_writers_require_worktree_for_every_mode_and_boolean_combination() {
        let dir = TempDir::new("execution-mode-parallel-gate");
        let workspace = dir.mkdir("ws");
        for (mode_toml, mode) in [
            ("", ExecutionMode::Direct),
            ("execution_mode = 'direct'\n", ExecutionMode::Direct),
            ("execution_mode = 'worktree'\n", ExecutionMode::Worktree),
        ] {
            for parallel in [None, Some(false), Some(true)] {
                let parallel_toml = parallel
                    .map(|v| format!("allow_parallel_writers = {v}\n"))
                    .unwrap_or_default();
                let path = dir.write(
                    "projects.toml",
                    &project_toml_with(
                        "proj",
                        workspace.to_str().unwrap(),
                        "http://127.0.0.1:4101",
                        &format!("{mode_toml}{parallel_toml}"),
                    ),
                );
                let result = load_config(&path);
                if parallel == Some(true) && mode == ExecutionMode::Direct {
                    let error = result.expect_err("parallel writers need a private worktree");
                    assert_eq!(error.kind(), ErrorKind::InvalidInput);
                    assert_eq!(
                        error.message(),
                        "project allow_parallel_writers=true requires execution_mode=worktree"
                    );
                } else {
                    let config = result.expect("valid mode/parallel combination");
                    let entry = config.project("proj").unwrap();
                    assert_eq!(entry.execution_mode(), mode);
                    assert_eq!(entry.allow_parallel_writers(), parallel.unwrap_or(false));
                    assert_eq!(
                        entry
                            .get("allow_parallel_writers")
                            .and_then(toml::Value::as_bool),
                        parallel
                    );
                }
            }
        }
    }

    #[test]
    fn parallel_writer_mode_gate_rejects_non_booleans_in_both_modes() {
        for mode in ["direct", "worktree"] {
            for raw in [
                "'false'",
                "'true'",
                "0",
                "1",
                "0.0",
                "[]",
                "{}",
                "1979-05-27",
            ] {
                let error = load_endpoint_config(
                    "proj",
                    "http://127.0.0.1:4101",
                    &format!("execution_mode = '{mode}'\nallow_parallel_writers = {raw}\n"),
                );
                assert_eq!(error.kind(), ErrorKind::InvalidInput);
                assert_eq!(
                    error.message(),
                    "project allow_parallel_writers must be a boolean"
                );
            }
        }
    }

    #[test]
    fn execution_mode_validation_is_per_project_and_preserves_other_settings() {
        let dir = TempDir::new("execution-mode-per-project");
        let a = dir.mkdir("a");
        let b = dir.mkdir("b");
        let text = format!(
            "{}\n{}",
            project_toml_with("a", a.to_str().unwrap(), "http://127.0.0.1:4101", ""),
            project_toml_with(
                "b",
                b.to_str().unwrap(),
                "http://127.0.0.1:4102",
                "execution_mode = 'worktree'\nallow_parallel_writers = true\nmax_active_tasks = 3\n"
            )
        );
        let path = dir.write("projects.toml", &text);
        let config = load_config(&path).expect("valid independent projects");
        assert_eq!(
            config.project("a").unwrap().execution_mode(),
            ExecutionMode::Direct
        );
        let entry = config.project("b").unwrap();
        assert_eq!(entry.execution_mode(), ExecutionMode::Worktree);
        assert_eq!(entry.max_active_tasks(), 3);
        assert!(entry.allow_parallel_writers());
        assert_eq!(config.project("a").unwrap().max_active_tasks(), 1);
        assert!(!config.project("a").unwrap().allow_parallel_writers());
        assert_eq!(entry.max_rounds(), 3);
        assert_eq!(
            entry
                .get("max_active_tasks")
                .and_then(toml::Value::as_integer),
            Some(3)
        );
        let invalid = text.replace("execution_mode = 'worktree'", "execution_mode = 'direct'");
        let path = dir.write("projects.toml", &invalid);
        assert!(
            load_config(&path).is_err(),
            "a bad mode in any project rejects the whole config"
        );
    }

    #[test]
    fn execution_mode_and_parallel_gate_errors_are_redacted() {
        let dir = TempDir::new("execution-mode-private-config");
        let workspace = dir.mkdir("private-workspace");
        let private_input = "secret-execution-token";
        for extra in [
            format!("execution_mode = '{private_input}'\n"),
            format!("allow_parallel_writers = '{private_input}'\n"),
            format!("max_active_tasks = '{private_input}'\n"),
            "allow_parallel_writers = true\n".to_owned(),
        ] {
            let path = dir.write(
                "private-projects.toml",
                &project_toml_with(
                    "private_proj",
                    workspace.to_str().unwrap(),
                    "http://127.0.0.1:4101",
                    &extra,
                ),
            );
            let error = load_config(&path).expect_err("must reject invalid mode/gate");
            let rendered = format!("{error} {error:?}");
            for sensitive in [
                private_input,
                "private_proj",
                workspace.to_str().unwrap(),
                path.to_str().unwrap(),
            ] {
                assert!(
                    !rendered.contains(sensitive),
                    "error leaked input: {rendered}"
                );
            }
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn execution_mode_and_admission_match_frozen_v15_config_corpus() {
        let corpus: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/fixtures/config-cases.json")).unwrap();
        let mut checked = 0;
        for case in corpus["cases"].as_array().unwrap() {
            if !matches!(
                case["rule"].as_str(),
                Some("execution_mode" | "allow_parallel_writers" | "max_active_tasks")
            ) {
                continue;
            }
            checked += 1;
            let dir = TempDir::new("execution-mode-corpus");
            let workspace = dir.mkdir("ws");
            let text = case["toml"]
                .as_str()
                .unwrap()
                .replace("${WORKSPACE}", workspace.to_str().unwrap());
            let path = dir.write("projects.toml", &text);
            let result = load_config(&path);
            if case["expectation"] == "valid" {
                let config = result.unwrap_or_else(|error| panic!("case {}: {error}", case["id"]));
                let entry = config.project("proj").unwrap();
                let expected = case["expect"]["execution_mode"]
                    .as_str()
                    .unwrap_or("direct");
                assert_eq!(
                    entry.execution_mode().as_str(),
                    expected,
                    "case {}",
                    case["id"]
                );
                assert_eq!(
                    entry.max_active_tasks(),
                    case["expect"]["max_active_tasks"].as_u64().unwrap_or(1),
                    "case {}",
                    case["id"]
                );
                assert_eq!(
                    entry.allow_parallel_writers(),
                    case["expect"]["allow_parallel_writers"]
                        .as_bool()
                        .unwrap_or(false),
                    "case {}",
                    case["id"]
                );
            } else {
                let error = result.expect_err("invalid corpus case must fail");
                assert_eq!(error.kind(), ErrorKind::InvalidInput);
                let message = match case["error_category"].as_str().unwrap() {
                    "execution_mode_type" => "project execution_mode must be a string",
                    "execution_mode_whitespace" => {
                        "project execution_mode must not have surrounding whitespace"
                    }
                    "execution_mode_value" => "project execution_mode must be direct or worktree",
                    "allow_parallel_writers_type" => {
                        "project allow_parallel_writers must be a boolean"
                    }
                    "allow_parallel_writers_direct" => {
                        "project allow_parallel_writers=true requires execution_mode=worktree"
                    }
                    "max_active_tasks_invalid" => {
                        "project max_active_tasks must be a positive integer"
                    }
                    category => panic!("unexpected targeted category: {category}"),
                };
                assert_eq!(error.message(), message, "case {}", case["id"]);
            }
        }
        assert_eq!(checked, 23, "all targeted frozen v15 cases must run");
    }

    #[test]
    fn admission_defaults_and_positive_integer_counts_preserve_raw_values() {
        let dir = TempDir::new("admission-valid-counts");
        let workspace = dir.mkdir("ws");
        for (extra, expected, raw) in [
            ("".to_owned(), 1, None),
            ("max_active_tasks = 1\n".to_owned(), 1, Some(1)),
            ("max_active_tasks = 3\n".to_owned(), 3, Some(3)),
            ("max_active_tasks = 0x10\n".to_owned(), 16, Some(16)),
            ("max_active_tasks = 1_000\n".to_owned(), 1000, Some(1000)),
            (
                format!("max_active_tasks = {}\n", i64::MAX),
                i64::MAX as u64,
                Some(i64::MAX),
            ),
        ] {
            let path = dir.write(
                "projects.toml",
                &project_toml_with(
                    "proj",
                    workspace.to_str().unwrap(),
                    "http://127.0.0.1:4101",
                    &extra,
                ),
            );
            let config = load_config(&path).expect("positive TOML integer or absent default");
            let entry = config.project("proj").unwrap();
            assert_eq!(entry.max_active_tasks(), expected);
            assert_eq!(
                entry
                    .get("max_active_tasks")
                    .and_then(toml::Value::as_integer),
                raw
            );
            assert!(!entry.allow_parallel_writers());
            assert!(!entry.contains_key("allow_parallel_writers"));
            assert_eq!(entry.clone().max_active_tasks(), expected);
        }
    }

    #[test]
    fn admission_counts_reject_nonpositive_and_noninteger_values_in_both_modes() {
        for mode in ["direct", "worktree"] {
            for raw in [
                "0",
                "-1",
                "-9223372036854775808",
                "true",
                "false",
                "1.0",
                "1.5",
                "'2'",
                "[]",
                "[1]",
                "{}",
                "1979-05-27",
            ] {
                let error = load_endpoint_config(
                    "proj",
                    "http://127.0.0.1:4101",
                    &format!("execution_mode = '{mode}'\nmax_active_tasks = {raw}\n"),
                );
                assert_eq!(error.kind(), ErrorKind::InvalidInput);
                assert_eq!(
                    error.message(),
                    "project max_active_tasks must be a positive integer"
                );
            }
        }
    }

    #[test]
    fn admission_task_bound_and_parallel_writer_opt_in_are_independent() {
        let dir = TempDir::new("admission-independent-settings");
        let workspace = dir.mkdir("ws");
        for count in [1, 3] {
            for mode in ["direct", "worktree"] {
                for parallel in [false, true] {
                    let extra = format!(
                        "max_active_tasks = {count}\nexecution_mode = '{mode}'\nallow_parallel_writers = {parallel}\n"
                    );
                    let path = dir.write(
                        "projects.toml",
                        &project_toml_with(
                            "proj",
                            workspace.to_str().unwrap(),
                            "http://127.0.0.1:4101",
                            &extra,
                        ),
                    );
                    let result = load_config(&path);
                    if parallel && mode == "direct" {
                        assert!(
                            result.is_err(),
                            "a higher task bound never bypasses the mode gate"
                        );
                    } else {
                        let config = result.expect("independent settings are valid");
                        let entry = config.project("proj").unwrap();
                        assert_eq!(entry.max_active_tasks(), count);
                        assert_eq!(entry.allow_parallel_writers(), parallel);
                    }
                }
            }
        }
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
            entry.opencode_env_file().map(|file| file.as_path()),
            Some(dir.path().join("secrets/proj.env").as_path())
        );
        assert!(
            entry
                .opencode_env_file()
                .expect("path")
                .as_path()
                .is_absolute()
        );
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

        assert_eq!(
            entry.opencode_env_file().map(|file| file.as_path()),
            Some(env_file.as_path())
        );
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

    #[test]
    fn crate_constant_names_the_auto_approve_external_directories_key() {
        assert_eq!(
            AUTO_APPROVE_EXTERNAL_DIRECTORIES_KEY,
            "auto_approve_external_directories"
        );
    }

    /// Builds a multi-project TOML with sequential loopback ports; every entry
    /// is `(id, workspace, extra)` and gets `auto_approve_external_directories`
    /// from `extra`.
    fn projects_toml(entries: &[(&str, &Path, &str)]) -> String {
        let mut text = String::new();
        for (index, (id, workspace, extra)) in entries.iter().enumerate() {
            let port = 4101 + index;
            text.push_str(&format!(
                "[projects.{id}]\nworkspace = \"{ws}\"\nopencode_url = \"http://127.0.0.1:{port}\"\npassword_file = \"secrets/{id}.password\"\nmax_rounds = 3\n{extra}\n",
                ws = workspace.to_str().expect("utf-8 path"),
            ));
        }
        text
    }

    /// Loads a one-project config with the given extra key lines, returning the
    /// validation error and panicking if it unexpectedly loads.
    fn load_external_extra_error(extra: &str) -> DomainError {
        let dir = TempDir::new("external-extra-error");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                extra,
            ),
        );
        load_config(&config_path).expect_err("config must be rejected")
    }

    /// Returns the linked project ids for `id`, in discovery order.
    fn linked_ids<'a>(config: &'a Config, id: &str) -> Vec<&'a str> {
        config
            .linked_projects(id)
            .iter()
            .map(|entry| entry.id().as_str())
            .collect()
    }

    // Corpus case `valid-minimal-project`: the optional key defaults to empty.
    #[test]
    fn auto_approve_external_directories_default_to_empty() {
        let dir = TempDir::new("external-default");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );

        let config = load_config(&config_path).expect("minimal config must load");
        let entry = config.project("proj").expect("project must exist");

        assert!(entry.auto_approve_external_directories().is_empty());
    }

    // Corpus case `valid-external-directory-canonicalized`.
    #[cfg(unix)]
    #[test]
    fn canonicalizes_trusted_external_directory_symlink() {
        let dir = TempDir::new("external-symlink");
        let workspace = dir.mkdir("ws");
        let real = dir.mkdir("real-trusted");
        let link = dir.path().join("trusted-link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink must be creatable");
        let extra = format!(
            "auto_approve_external_directories = [\"{}\"]\n",
            link.to_str().expect("utf-8 path")
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

        let config = load_config(&config_path).expect("symlinked trusted directory must load");
        let entry = config.project("proj").expect("project must exist");

        assert_eq!(
            entry.auto_approve_external_directories().to_vec(),
            vec![canonical(&real)]
        );
    }

    // Corpus case `invalid-external-directory-relative`.
    #[test]
    fn rejects_relative_trusted_external_directory() {
        let error =
            load_external_extra_error("auto_approve_external_directories = [\"relative/dir\"]\n");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project auto_approve_external_directories entries must be absolute paths"
        );
    }

    // Corpus case `invalid-external-directory-filesystem-root`.
    #[test]
    fn rejects_filesystem_root_as_trusted_external_directory() {
        let error = load_external_extra_error("auto_approve_external_directories = [\"/\"]\n");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project filesystem root is not a trusted external directory"
        );
    }

    // Corpus case `invalid-external-directory-missing`.
    #[test]
    fn rejects_missing_trusted_external_directory() {
        let dir = TempDir::new("external-missing");
        let workspace = dir.mkdir("ws");
        let missing = dir.path().join("missing-dir");
        let extra = format!(
            "auto_approve_external_directories = [\"{}\"]\n",
            missing.to_str().expect("utf-8 path")
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

        let error = load_config(&config_path).expect_err("missing trusted directory must fail");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(
            error.to_string(),
            "project external directory does not exist"
        );
    }

    // Corpus case `invalid-external-directory-not-a-directory`.
    #[test]
    fn rejects_file_as_trusted_external_directory() {
        let dir = TempDir::new("external-file");
        let workspace = dir.mkdir("ws");
        let file = dir.write("not-a-dir", "x");
        let extra = format!(
            "auto_approve_external_directories = [\"{}\"]\n",
            file.to_str().expect("utf-8 path")
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

        let error = load_config(&config_path).expect_err("file trusted directory must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project external directory is not a directory"
        );
    }

    // Corpus case `invalid-external-directory-non-list`.
    #[test]
    fn rejects_non_list_auto_approve_external_directories() {
        for literal in ["\"/tmp\"", "3", "true", "3.5", "{ x = 1 }"] {
            let error = load_external_extra_error(&format!(
                "auto_approve_external_directories = {literal}\n"
            ));
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "literal: {literal}");
            assert_eq!(
                error.to_string(),
                "project auto_approve_external_directories must be a list of strings",
                "literal: {literal}"
            );
        }
    }

    #[test]
    fn rejects_non_string_or_empty_external_directory_entries() {
        for literal in ["[1]", "[\"\"]", "[\"   \"]", "[true]", "[[\"/tmp\"]]"] {
            let error = load_external_extra_error(&format!(
                "auto_approve_external_directories = {literal}\n"
            ));
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "literal: {literal}");
            assert_eq!(
                error.to_string(),
                "project auto_approve_external_directories entries must be non-empty strings",
                "literal: {literal}"
            );
        }
    }

    #[test]
    fn accepts_empty_auto_approve_external_directories_array() {
        let dir = TempDir::new("external-empty");
        let workspace = dir.mkdir("ws");
        let extra = "auto_approve_external_directories = []\n";
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                extra,
            ),
        );

        let config = load_config(&config_path).expect("empty array must load");
        assert!(
            config
                .project("proj")
                .expect("project must exist")
                .auto_approve_external_directories()
                .is_empty()
        );
    }

    #[test]
    fn collapses_duplicate_trusted_external_directories_stably() {
        let dir = TempDir::new("external-dedupe");
        let workspace = dir.mkdir("ws");
        let trusted = dir.mkdir("trusted");
        let other = dir.mkdir("other");
        let extra = format!(
            "auto_approve_external_directories = [\"{a}\", \"{b}\", \"{a}\"]\n",
            a = trusted.to_str().expect("utf-8 path"),
            b = other.to_str().expect("utf-8 path"),
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

        let config = load_config(&config_path).expect("duplicate trusted directories must load");
        let entry = config.project("proj").expect("project must exist");

        assert_eq!(
            entry.auto_approve_external_directories().to_vec(),
            vec![canonical(&trusted), canonical(&other)]
        );
    }

    // A symlink alias of an already-trusted directory collapses onto the same
    // canonical entry instead of being trusted twice.
    #[cfg(unix)]
    #[test]
    fn collapses_symlink_alias_of_trusted_external_directory() {
        let dir = TempDir::new("external-dedupe-link");
        let workspace = dir.mkdir("ws");
        let trusted = dir.mkdir("trusted");
        let link = dir.path().join("trusted-link");
        std::os::unix::fs::symlink(&trusted, &link).expect("symlink must be creatable");
        let extra = format!(
            "auto_approve_external_directories = [\"{a}\", \"{b}\"]\n",
            a = trusted.to_str().expect("utf-8 path"),
            b = link.to_str().expect("utf-8 path"),
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

        let config = load_config(&config_path).expect("alias trusted directory must load");
        let entry = config.project("proj").expect("project must exist");

        assert_eq!(
            entry.auto_approve_external_directories().to_vec(),
            vec![canonical(&trusted)]
        );
    }

    #[test]
    fn external_directory_errors_do_not_leak_supplied_values() {
        const SECRET: &str = "secret-external-directory";

        let dir = TempDir::new("external-redact");
        let workspace = dir.mkdir("ws");
        let secret = dir.path().join(SECRET);
        let extra = format!(
            "auto_approve_external_directories = [\"{}\"]\n",
            secret.to_str().expect("utf-8 path")
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

        let error = load_config(&config_path).expect_err("missing trusted directory must fail");
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(SECRET), "leaked directory: {rendered}");
        assert!(
            !rendered.contains(config_path.to_str().expect("utf-8 path")),
            "leaked config path: {rendered}"
        );
    }

    // Corpus case `valid-linked-project-canonical-exact-match`.
    #[cfg(unix)]
    #[test]
    fn links_project_through_canonical_workspace_match() {
        let dir = TempDir::new("linked-canonical");
        let proj_ws = dir.mkdir("ws-proj");
        let beta_ws = dir.mkdir("ws-beta");
        let link = dir.path().join("beta-alias");
        std::os::unix::fs::symlink(&beta_ws, &link).expect("symlink must be creatable");
        let proj_extra = format!(
            "auto_approve_external_directories = [\"{}\"]\n",
            link.to_str().expect("utf-8 path")
        );
        let text = projects_toml(&[("proj", &proj_ws, &proj_extra), ("beta", &beta_ws, "")]);
        let config_path = dir.write("projects.toml", &text);

        let config = load_config(&config_path).expect("linked config must load");
        let linked = config.linked_projects("proj");

        assert_eq!(linked_ids(&config, "proj"), ["beta"]);
        assert_eq!(linked[0].workspace(), canonical(&beta_ws).as_path());
    }

    // Corpus case `valid-linked-containing-parent-dir-not-a-target`.
    #[test]
    fn containing_parent_directory_does_not_link_child_workspace() {
        let dir = TempDir::new("linked-parent");
        let proj_ws = dir.mkdir("ws-proj");
        let beta_ws = dir.mkdir("ws-beta");
        let parent_extra = format!(
            "auto_approve_external_directories = [\"{}\"]\n",
            dir.path().to_str().expect("utf-8 path")
        );
        let text = projects_toml(&[("proj", &proj_ws, &parent_extra), ("beta", &beta_ws, "")]);
        let config_path = dir.write("projects.toml", &text);

        let config = load_config(&config_path).expect("linked config must load");

        assert!(config.linked_projects("proj").is_empty());
    }

    // Corpus case `valid-linked-unregistered-trusted-dir-not-a-target`.
    #[test]
    fn unregistered_trusted_directory_is_not_a_task_target() {
        let dir = TempDir::new("linked-unregistered");
        let proj_ws = dir.mkdir("ws-proj");
        let beta_ws = dir.mkdir("ws-beta");
        let unregistered = dir.mkdir("ws-lib");
        let proj_extra = format!(
            "auto_approve_external_directories = [\"{a}\", \"{b}\"]\n",
            a = unregistered.to_str().expect("utf-8 path"),
            b = beta_ws.to_str().expect("utf-8 path"),
        );
        let text = projects_toml(&[("proj", &proj_ws, &proj_extra), ("beta", &beta_ws, "")]);
        let config_path = dir.write("projects.toml", &text);

        let config = load_config(&config_path).expect("linked config must load");

        assert_eq!(linked_ids(&config, "proj"), ["beta"]);
    }

    // Corpus case `valid-linked-project-sorted-deterministically`.
    #[test]
    fn linked_projects_are_sorted_by_project_id() {
        let dir = TempDir::new("linked-sorted");
        let proj_ws = dir.mkdir("ws-proj");
        let zeta_ws = dir.mkdir("ws-zeta");
        let alpha_ws = dir.mkdir("ws-alpha");
        let proj_extra = format!(
            "auto_approve_external_directories = [\"{z}\", \"{a}\"]\n",
            z = zeta_ws.to_str().expect("utf-8 path"),
            a = alpha_ws.to_str().expect("utf-8 path"),
        );
        let text = projects_toml(&[
            ("proj", &proj_ws, &proj_extra),
            ("zeta", &zeta_ws, ""),
            ("alpha", &alpha_ws, ""),
        ]);
        let config_path = dir.write("projects.toml", &text);

        let config = load_config(&config_path).expect("linked config must load");

        assert_eq!(linked_ids(&config, "proj"), ["alpha", "zeta"]);
    }

    #[test]
    fn trusted_own_workspace_does_not_self_link() {
        let dir = TempDir::new("linked-self");
        let proj_ws = dir.mkdir("ws-proj");
        let beta_ws = dir.mkdir("ws-beta");
        let proj_extra = format!(
            "auto_approve_external_directories = [\"{}\"]\n",
            proj_ws.to_str().expect("utf-8 path")
        );
        let text = projects_toml(&[("proj", &proj_ws, &proj_extra), ("beta", &beta_ws, "")]);
        let config_path = dir.write("projects.toml", &text);

        let config = load_config(&config_path).expect("linked config must load");

        assert!(config.linked_projects("proj").is_empty());
    }

    #[test]
    fn linked_projects_without_trusted_roots_is_empty() {
        let dir = TempDir::new("linked-none");
        let proj_ws = dir.mkdir("ws-proj");
        let beta_ws = dir.mkdir("ws-beta");
        let text = projects_toml(&[("proj", &proj_ws, ""), ("beta", &beta_ws, "")]);
        let config_path = dir.write("projects.toml", &text);

        let config = load_config(&config_path).expect("linked config must load");

        assert!(config.linked_projects("proj").is_empty());
        assert!(config.linked_projects("unknown").is_empty());
    }

    // Task 2.8: the password and the optional MCP token are read through the
    // typed credential API and the relative paths are resolved from the
    // projects.toml directory.
    #[test]
    fn reads_password_and_optional_mcp_token_through_typed_api() {
        let dir = TempDir::new("cred-read");
        let workspace = dir.mkdir("ws");
        dir.mkdir("secrets");
        let password_path = write_credential(&dir, "secrets/proj.password", b"pw-secret\n", 0o600);
        let token_path = write_credential(&dir, "secrets/proj.mcp-token", b"mcp-secret\n", 0o600);
        let config_path = dir.write(
            "projects.toml",
            &project_toml_with(
                "proj",
                workspace.to_str().expect("utf-8 path"),
                "http://127.0.0.1:4101",
                "mcp_url = \"http://127.0.0.1:4201/mcp\"\nmcp_token_file = \"secrets/proj.mcp-token\"\n",
            ),
        );

        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");

        assert_eq!(
            entry.password_file().expect("password path").as_path(),
            password_path
        );
        assert_eq!(
            entry.mcp_token_file().expect("token path").as_path(),
            token_path
        );

        let password = entry.read_password().expect("password must read");
        assert_eq!(password.expose_secret(), "pw-secret");
        assert!(!password.is_empty());

        let token = entry
            .read_mcp_token()
            .expect("token must read")
            .expect("token must be present");
        assert_eq!(token.expose_secret(), "mcp-secret");
    }

    #[test]
    fn absent_password_file_is_not_configured_and_token_is_none() {
        let dir = TempDir::new("cred-absent");
        let workspace = dir.mkdir("ws");
        let text = format!(
            "[projects.proj]\nworkspace = \"{}\"\nopencode_url = \"http://127.0.0.1:4101\"\nmax_rounds = 3\n",
            workspace.to_str().expect("utf-8 path")
        );
        let config_path = dir.write("projects.toml", &text);

        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");

        assert!(entry.password_file().is_none());
        assert!(entry.mcp_token_file().is_none());
        assert!(
            entry
                .read_mcp_token()
                .expect("token read must succeed")
                .is_none()
        );

        let error = entry
            .read_password()
            .expect_err("password must be required");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(error.to_string(), "project password_file is not configured");
    }

    #[test]
    fn missing_credential_file_fails_with_not_found() {
        let dir = TempDir::new("cred-missing");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );
        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");

        let error = entry.read_password().expect_err("missing file must fail");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(error.to_string(), "credential file not found");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_credential_file_is_rejected() {
        let dir = TempDir::new("cred-symlink");
        let workspace = dir.mkdir("ws");
        dir.mkdir("secrets");
        let real = write_credential(&dir, "secrets/real.password", b"pw\n", 0o600);
        let link = dir.path().join("secrets/proj.password");
        std::os::unix::fs::symlink(&real, &link).expect("symlink must be creatable");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );
        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");

        let error = entry.read_password().expect_err("symlink must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "credential file must not be a symlink");
    }

    #[test]
    fn directory_credential_path_is_not_a_regular_file() {
        let dir = TempDir::new("cred-dir");
        let workspace = dir.mkdir("ws");
        dir.mkdir("secrets/proj.password");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );
        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");

        let error = entry
            .read_password()
            .expect_err("directory must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "credential path is not a regular file");
    }

    #[cfg(unix)]
    #[test]
    fn group_or_other_permissions_are_rejected() {
        let dir = TempDir::new("cred-mode");
        let workspace = dir.mkdir("ws");
        dir.mkdir("secrets");
        write_credential(&dir, "secrets/proj.password", b"pw\n", 0o644);
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );
        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");

        let error = entry
            .read_password()
            .expect_err("open mode must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "credential file permissions too open (want 600)"
        );
    }

    #[test]
    fn invalid_utf8_credential_is_rejected() {
        let dir = TempDir::new("cred-utf8");
        let workspace = dir.mkdir("ws");
        dir.mkdir("secrets");
        write_credential(&dir, "secrets/proj.password", &[0xff, 0xfe, b'\n'], 0o600);
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );
        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");

        let error = entry
            .read_password()
            .expect_err("invalid UTF-8 must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "credential file is not valid UTF-8");
    }

    #[test]
    fn empty_or_blank_credentials_are_rejected() {
        for (name, bytes) in [("empty", b"".as_slice()), ("newline", b"\n".as_slice())] {
            let dir = TempDir::new("cred-empty");
            let workspace = dir.mkdir("ws");
            dir.mkdir("secrets");
            write_credential(&dir, "secrets/proj.password", bytes, 0o600);
            let config_path = dir.write(
                "projects.toml",
                &project_toml("proj", workspace.to_str().expect("utf-8 path")),
            );
            let config = load_config(&config_path).expect("config must load");
            let entry = config.project("proj").expect("project must exist");

            let error = entry.read_password().expect_err("empty must be rejected");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "case: {name}");
            assert_eq!(
                error.to_string(),
                "credential file is empty",
                "case: {name}"
            );
        }
    }

    #[test]
    fn multiline_credentials_are_rejected() {
        for (name, bytes) in [
            ("embedded newline", b"a\nb".as_slice()),
            ("crlf", b"a\r\n".as_slice()),
            ("extra newline", b"a\n\n".as_slice()),
        ] {
            let dir = TempDir::new("cred-multiline");
            let workspace = dir.mkdir("ws");
            dir.mkdir("secrets");
            write_credential(&dir, "secrets/proj.password", bytes, 0o600);
            let config_path = dir.write(
                "projects.toml",
                &project_toml("proj", workspace.to_str().expect("utf-8 path")),
            );
            let config = load_config(&config_path).expect("config must load");
            let entry = config.project("proj").expect("project must exist");

            let error = entry
                .read_password()
                .expect_err("multiline must be rejected");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "case: {name}");
            assert_eq!(
                error.to_string(),
                "credential file must contain a single line",
                "case: {name}"
            );
        }
    }

    #[test]
    fn credential_values_paths_and_errors_are_redacted() {
        const SECRET: &str = "super-secret-credential-value";

        let dir = TempDir::new("cred-redact");
        let workspace = dir.mkdir("ws");
        dir.mkdir("secrets");
        let secret_path = write_credential(
            &dir,
            "secrets/proj.password",
            format!("{SECRET}\n").as_bytes(),
            0o600,
        );
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );
        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");

        let secret = entry.read_password().expect("password must read");
        let secret_debug = format!("{secret:?}");
        let secret_display = secret.to_string();
        assert!(
            !secret_debug.contains(SECRET),
            "Debug leaked secret: {secret_debug}"
        );
        assert!(
            !secret_display.contains(SECRET),
            "Display leaked secret: {secret_display}"
        );

        let credential_path = entry.password_file().expect("password path");
        let path_debug = format!("{credential_path:?}");
        let path_text = secret_path.to_str().expect("utf-8 path");
        assert!(
            !path_debug.contains(path_text),
            "CredentialPath Debug leaked path: {path_debug}"
        );

        write_credential(
            &dir,
            "secrets/proj.password",
            format!("{SECRET}\nsecond-line\n").as_bytes(),
            0o600,
        );
        let error = entry
            .read_password()
            .expect_err("multiline must be rejected");
        let rendered = format!("{error} {error:?}");
        assert!(
            !rendered.contains(SECRET),
            "error leaked secret: {rendered}"
        );
        assert!(
            !rendered.contains(path_text),
            "error leaked path: {rendered}"
        );
    }

    // Task 2.9: the optional `opencode_env_file` is read through the typed
    // `ProjectEnvFile`/`ProjectEnv` API, mirroring
    // `src/agent_bridge/project_env.py`.

    /// Loads a one-project config whose absolute `opencode_env_file` is
    /// `env_path`.
    fn load_env_config(dir: &TempDir, env_path: &Path) -> Config {
        let workspace = dir.mkdir("ws");
        let extra = format!(
            "opencode_env_file = \"{}\"\n",
            env_path.to_str().expect("utf-8 path")
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
        load_config(&config_path).expect("config must load")
    }

    /// Writes an env file with an explicit Unix mode and reads it through the
    /// typed API.
    fn read_env(content: &[u8], mode: u32) -> bridge_domain::Result<ProjectEnv> {
        let dir = TempDir::new("env-read");
        let env_path = write_credential(&dir, "proj.env", content, mode);
        let config = load_env_config(&dir, &env_path);
        let entry = config.project("proj").expect("project must exist");
        let env = entry.read_opencode_env()?;
        env.ok_or_else(|| DomainError::not_found("test env must be present"))
    }

    // Corpus case `valid-env-file-parse`.
    #[test]
    fn parses_valid_opencode_env_file() {
        let content = b"# provider keys\n\nANTHROPIC_API_KEY=placeholder-key\n   \nOPENAI_API_KEY=placeholder=with=equals\nEMPTY=\nQUOTED=\"literal\"\n";
        let env = read_env(content, 0o600).expect("valid env must parse");
        assert_eq!(env.len(), 4);
        assert!(!env.is_empty());
        assert_eq!(env.get("ANTHROPIC_API_KEY"), Some("placeholder-key"));
        assert_eq!(env.get("OPENAI_API_KEY"), Some("placeholder=with=equals"));
        assert_eq!(env.get("EMPTY"), Some(""));
        assert_eq!(env.get("QUOTED"), Some("\"literal\""));
        assert!(env.contains_key("EMPTY"));
        assert!(!env.contains_key("MISSING"));
    }

    #[test]
    fn opencode_env_splits_on_first_equals_and_keeps_value_verbatim() {
        let env = read_env(b"FOO=  spaced  value  \n", 0o600).expect("must parse");
        assert_eq!(env.get("FOO"), Some("  spaced  value  "));
    }

    #[test]
    fn opencode_env_values_are_literal_without_expansion() {
        let env = read_env(
            b"A=$HOME\nB=$(id)\nC=`id`\nD=\"quoted\"\nE='single'\n",
            0o600,
        )
        .expect("must parse");
        assert_eq!(env.get("A"), Some("$HOME"));
        assert_eq!(env.get("B"), Some("$(id)"));
        assert_eq!(env.get("C"), Some("`id`"));
        assert_eq!(env.get("D"), Some("\"quoted\""));
        assert_eq!(env.get("E"), Some("'single'"));
    }

    #[test]
    fn opencode_env_handles_crlf_lines() {
        let env = read_env(b"A=1\r\nB=2\r\n", 0o600).expect("must parse");
        assert_eq!(env.len(), 2);
        assert_eq!(env.get("A"), Some("1"));
        assert_eq!(env.get("B"), Some("2"));
    }

    #[test]
    fn opencode_env_comment_lines_may_contain_arbitrary_bytes() {
        let env = parse_project_env("# comment\0 ignored\nA=1\n")
            .expect("a comment line must be skipped before the NUL check");
        assert_eq!(env.get("A"), Some("1"));
    }

    #[test]
    fn split_env_lines_matches_python_boundaries() {
        assert_eq!(split_env_lines("a\nb"), ["a", "b"]);
        assert_eq!(split_env_lines("a\nb\n"), ["a", "b"]);
        assert_eq!(split_env_lines("a\r\nb"), ["a", "b"]);
        assert_eq!(split_env_lines("\n"), [""]);
        assert!(split_env_lines("").is_empty());
        assert_eq!(split_env_lines("a\u{2028}b"), ["a", "b"]);
        assert_eq!(split_env_lines("a\u{000b}b"), ["a", "b"]);
    }

    // Corpus case `invalid-env-duplicate-variable-name`.
    #[test]
    fn rejects_duplicate_opencode_env_names() {
        let error = read_env(b"A=one\nA=two\n", 0o600).expect_err("duplicate must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project env file defines the same variable name twice"
        );
    }

    // Corpus case `invalid-env-invalid-variable-name`.
    #[test]
    fn rejects_invalid_opencode_env_names() {
        for line in [
            "BAD-NAME=value\n",
            "1BAD=x\n",
            "A-B=x\n",
            "A B=x\n",
            "A.B=x\n",
            "export FOO=x\n",
            " =x\n",
            "=x\n",
        ] {
            let error = read_env(line.as_bytes(), 0o600).expect_err("invalid name must fail");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "line: {line:?}");
            assert_eq!(
                error.to_string(),
                "project env file line has an invalid variable name",
                "line: {line:?}"
            );
        }
    }

    // Corpus case `invalid-env-line-missing-equals`.
    #[test]
    fn rejects_opencode_env_line_without_equals() {
        let error = read_env(b"JUST_A_NAME\n", 0o600).expect_err("must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project env file line must be NAME=value"
        );
    }

    // Corpus case `invalid-env-nul-byte`.
    #[test]
    fn rejects_opencode_env_nul_byte() {
        for content in [
            b"NAME\0=value\n".as_slice(),
            b"SECRET_NAME=secret\0tail\n",
            b"SECRET\0NAME=secret\n",
            b"NAMED\0=\0value\n",
        ] {
            let error = read_env(content, 0o600).expect_err("NUL must be rejected");
            assert_eq!(
                error.kind(),
                ErrorKind::InvalidInput,
                "content: {content:?}"
            );
            assert_eq!(
                error.to_string(),
                "project env file line contains a NUL byte",
                "content: {content:?}"
            );
        }
    }

    // Corpus case `invalid-env-protected-service-name`.
    #[test]
    fn rejects_protected_opencode_env_names() {
        for name in PROTECTED_ENV_NAMES {
            let content = format!("{name}=value\n");
            let error = read_env(content.as_bytes(), 0o600).expect_err("protected name must fail");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "name: {name}");
            assert_eq!(
                error.to_string(),
                "project env file must not define a reserved agent-bridge service variable",
                "name: {name}"
            );
        }
    }

    // Corpus case `invalid-env-file-missing`.
    #[test]
    fn missing_opencode_env_file_is_not_found() {
        let dir = TempDir::new("env-missing");
        let env_path = dir.path().join("missing.env");
        let config = load_env_config(&dir, &env_path);
        let entry = config.project("proj").expect("project must exist");

        let error = entry
            .read_opencode_env()
            .expect_err("missing file must be rejected");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(error.to_string(), "project env file not found");
    }

    // Corpus case `invalid-env-file-symlink`.
    #[cfg(unix)]
    #[test]
    fn symlink_opencode_env_file_is_rejected() {
        let dir = TempDir::new("env-symlink");
        let real = write_credential(&dir, "real.env", b"A=1\n", 0o600);
        let link = dir.path().join("proj.env");
        std::os::unix::fs::symlink(&real, &link).expect("symlink must be creatable");
        let config = load_env_config(&dir, &link);
        let entry = config.project("proj").expect("project must exist");

        let error = entry
            .read_opencode_env()
            .expect_err("symlink must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project env file must not be a symlink");
    }

    // Corpus case `invalid-env-file-not-regular`.
    #[test]
    fn directory_opencode_env_path_is_not_regular() {
        let dir = TempDir::new("env-dir");
        let env_path = dir.mkdir("proj.env");
        let config = load_env_config(&dir, &env_path);
        let entry = config.project("proj").expect("project must exist");

        let error = entry
            .read_opencode_env()
            .expect_err("directory must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project env path is not a regular file");
    }

    // Corpus case `invalid-env-file-mode-not-0600`.
    #[cfg(unix)]
    #[test]
    fn opencode_env_mode_must_be_exactly_0600() {
        for mode in [0o644, 0o666, 0o640, 0o400, 0o777, 0o604] {
            let error = read_env(b"A=1\n", mode).expect_err("wrong mode must be rejected");
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "mode: {mode:o}");
            assert_eq!(
                error.to_string(),
                "project env file permissions must be exactly 600",
                "mode: {mode:o}"
            );
        }
    }

    // Corpus case `invalid-env-file-foreign-owner`: the ownership branch is
    // exercised against a deliberately wrong expected uid, because a normal test
    // process cannot create a file owned by another user.
    #[cfg(unix)]
    #[test]
    fn foreign_owner_opencode_env_is_rejected() {
        use std::os::unix::fs::MetadataExt;

        let dir = TempDir::new("env-owner");
        let env_path = write_credential(&dir, "proj.env", b"A=1\n", 0o600);
        let metadata = std::fs::metadata(&env_path).expect("metadata must read");
        let error = super::validate_project_env_metadata_owned_by(
            &metadata,
            metadata.uid().wrapping_add(1),
        )
        .expect_err("foreign owner must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "project env file must be owned by the current user"
        );
    }

    // Corpus case `invalid-env-file-not-utf8`.
    #[test]
    fn invalid_utf8_opencode_env_is_rejected() {
        let error = read_env(&[0xff, 0xfe, b'=', b'1', b'\n'], 0o600)
            .expect_err("invalid UTF-8 must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "project env file is not valid UTF-8");
    }

    #[test]
    fn absent_opencode_env_file_reads_as_none() {
        let dir = TempDir::new("env-absent");
        let workspace = dir.mkdir("ws");
        let config_path = dir.write(
            "projects.toml",
            &project_toml("proj", workspace.to_str().expect("utf-8 path")),
        );
        let config = load_config(&config_path).expect("config must load");
        let entry = config.project("proj").expect("project must exist");

        assert!(entry.opencode_env_file().is_none());
        assert!(
            entry
                .read_opencode_env()
                .expect("absent env must read as none")
                .is_none()
        );
    }

    #[test]
    fn opencode_env_values_paths_and_errors_are_redacted() {
        const SECRET: &str = "super-secret-env-value";

        let dir = TempDir::new("env-redact");
        let env_path = write_credential(
            &dir,
            "proj.env",
            format!("ANTHROPIC_API_KEY={SECRET}\n").as_bytes(),
            0o600,
        );
        let config = load_env_config(&dir, &env_path);
        let entry = config.project("proj").expect("project must exist");

        let env = entry
            .read_opencode_env()
            .expect("read must succeed")
            .expect("env must be present");
        let env_debug = format!("{env:?}");
        let env_display = env.to_string();
        assert!(
            !env_debug.contains(SECRET),
            "ProjectEnv Debug leaked secret: {env_debug}"
        );
        assert!(
            !env_display.contains(SECRET),
            "ProjectEnv Display leaked secret: {env_display}"
        );
        assert!(
            !env_debug.contains("ANTHROPIC_API_KEY"),
            "ProjectEnv Debug leaked a variable name: {env_debug}"
        );

        let env_file = entry.opencode_env_file().expect("env file path");
        let path_text = env_path.to_str().expect("utf-8 path");
        let path_debug = format!("{env_file:?}");
        assert!(
            !path_debug.contains(path_text),
            "ProjectEnvFile Debug leaked path: {path_debug}"
        );

        write_credential(
            &dir,
            "proj.env",
            format!("ANTHROPIC_API_KEY={SECRET}\nANTHROPIC_API_KEY=dup\n").as_bytes(),
            0o600,
        );
        let error = entry
            .read_opencode_env()
            .expect_err("duplicate must be rejected");
        let rendered = format!("{error} {error:?}");
        assert!(
            !rendered.contains(SECRET),
            "error leaked secret: {rendered}"
        );
        assert!(
            !rendered.contains(path_text),
            "error leaked path: {rendered}"
        );
    }
}
