//! Dedicated OpenCode session resolution for one worker round (task 7.3).
//!
//! This module is the reusable, narrow foundation the future worker round
//! execution builds on. Given the current round of an existing task, it resolves
//! exactly one dedicated OpenCode session for that round on top of the existing
//! [`bridge_opencode::OpenCodeClient`] session API and the atomic
//! [`bridge_storage::StorageConnection::bind_round_session`] lifecycle write. It
//! deliberately stops before initial/revision prompt preparation, prompt
//! delivery, outbound message ids, completion observation, the worker state
//! machine, MCP/runtime wiring and auto-approval.
//!
//! # Resolution contract
//!
//! [`resolve_round_session`] reproduces the reference
//! `worker.py::_resolve_round_session` semantics:
//!
//! 1. The deterministic title is exactly
//!    `agent-bridge {task_id} round {round_number}` ([`round_session_title`]).
//! 2. A non-empty persisted `rounds.session_id` **wins immediately**: it is
//!    returned as [`SessionResolutionSource::Existing`] without any `list`/`create`
//!    HTTP request and without re-binding. `tasks.session_id` and the session of a
//!    previous round are never consulted; only the current round's own persisted
//!    session is used.
//! 3. Otherwise the workspace sessions are listed once. A transport or malformed
//!    failure fails closed as [`SessionResolutionErrorKind::SessionUnknown`] and
//!    no session is created.
//! 4. Sessions whose title is **byte-for-byte** the deterministic title are
//!    collected. More than one match is [`SessionResolutionErrorKind::SessionAmbiguous`]:
//!    the first is never picked and no session is created, even when only one of
//!    the matches has a matching directory.
//! 5. Exactly one match is *adopted*: its id must be non-empty and start with
//!    `ses` (the reference `startswith("ses")`, **not** `ses_`), otherwise
//!    [`SessionResolutionErrorKind::SessionUnknown`]. Adoption requires a
//!    `directory` that resolves to the workspace, otherwise
//!    [`SessionResolutionErrorKind::SessionDirectoryMismatch`]. Only then is the
//!    id bound atomically and returned as [`SessionResolutionSource::Adopted`].
//! 6. With no match, [`bridge_opencode::OpenCodeClient::create_session`] is called
//!    **exactly once** with the deterministic title and no parent/fork/previous
//!    session. An HTTP failure or an unusable id is
//!    [`SessionResolutionErrorKind::SessionUnknown`]. A missing `directory` is
//!    allowed exactly like the reference; a present `directory` must resolve to
//!    the workspace, otherwise [`SessionResolutionErrorKind::SessionDirectoryMismatch`].
//!    The id is bound atomically and returned as
//!    [`SessionResolutionSource::Created`].
//!
//! The atomic [`bind_round_session`](bridge_storage::StorageConnection::bind_round_session)
//! is mandatory before a resolved success is returned, so `rounds.session_id` and
//! `tasks.session_id` always persist together. A storage failure is a typed
//! [`SessionResolutionErrorKind::Storage`] error and is **never** a resolved
//! success. If the server session was created before the binding failed, the next
//! resolution starts from `list_sessions`, finds the created session by its exact
//! title and adopts it instead of creating a duplicate; an HTTP `create` failure
//! whose delivery is unknown is never retried automatically.
//!
//! # Consistency before any HTTP side effect
//!
//! [`resolve_round_session`] takes the explicit [`RustStateLayout`] of the
//! Rust-owned state and opens it through the production ownership guard
//! [`RustStateLayout::open`] *before* any task/round read, HTTP request or
//! write. The guard requires the sidecar marker, its Rust implementation and
//! format version, the project namespace, the normalized state root, the frozen
//! schema v6 contract and `meta.runtime_owner='rust'` to all agree. A foreign,
//! missing, unmarked or copied state — for example a Python-owned schema-v6
//! database without the marker, a foreign marker, or Rust state copied under
//! another root — fails closed as
//! [`SessionResolutionErrorKind::StateOwnership`] before the database is even
//! opened, so no foreign database, marker or row is read or written. The
//! layout's own [`project_id`](RustStateLayout::project_id) must additionally
//! equal `round.project_id`, so a valid Rust state for another project can never
//! resolve or bind a session for this round. There is deliberately no unchecked
//! [`StorageConnection`] injection point: the only connection the resolver uses
//! is the one produced by this guard.
//!
//! Only then are the task and the current round read through the Rust-owned
//! storage and validated: the task must exist, the task and round must agree on
//! the project, the round must be the current (highest-numbered) round of its
//! task, and the task workspace must resolve to the same canonical workspace the
//! client is bound to. A stale or mismatched round reference therefore fails
//! closed as an input category ([`SessionResolutionErrorKind::UnknownTask`],
//! `TaskMismatch`, `StaleRound`, `WorkspaceMismatch`) and never creates a session
//! for a foreign task. No Python state, history or SQL is touched, and the atomic
//! `bind_round_session` remains the only write.
//!
//! # Concurrency precondition
//!
//! The caller **must hold the project [`crate::WorkerLock`] for the whole
//! resolution**, acquired from the same [`RustStateLayout`] that is passed here,
//! so the lock and the resolved state refer to the same verified project
//! namespace. The lock serializes workers of one project, so two workers can
//! never both observe an empty title match and each create a duplicate session,
//! and the atomic binding cannot race a competing resolution. This module does
//! not acquire the lock itself; the future worker wiring owns that ordering.
//!
//! # Directory comparison policy
//!
//! A server-reported `directory` is accepted only when it is an absolute path
//! that resolves (existing symlink components followed) to exactly the canonical
//! workspace the client is bound to. A relative path, an embedded NUL, a missing
//! or otherwise non-resolvable path, and any path that resolves to a different
//! tree are rejected (`SessionDirectoryMismatch`); a different workspace is never
//! accepted by a lexical prefix. This is a deliberate fail-closed hardening over
//! the reference `Path(directory).resolve() == workspace`: the reference resolves
//! relative paths against the process current directory and lexically appends
//! missing components, so its result can depend on the caller's working
//! directory. Here the comparison never depends on the current directory and a
//! missing/non-resolvable directory fails closed.
//!
//! The typed [`bridge_opencode::Session`] parser folds a non-string `id`, `title`
//! or `directory` (and an absent field) to `None`. A `None`/non-string title can
//! therefore never match the deterministic title, and a `None`/non-string id or
//! adoption directory is rejected fail closed, unlike a permissive reference
//! that could coerce or skip such values. For a created session an absent (or
//! JSON `null`) `directory` is accepted compatibly with the reference. A
//! non-string `directory` (for example a JSON number, list or object) is folded
//! to `None` by the existing typed parser and is likewise accepted; this is a
//! deliberate, documented deviation from the reference, whose
//! `directory = session.get("directory")` then `Path(directory)` raises
//! `TypeError` for such a value because it tests only `directory is not None`
//! and never the value's type. A *string* directory is still resolved and must
//! match the workspace. The transport crate is not changed.
//!
//! # Errors and redaction
//!
//! [`SessionResolutionError`] is typed and payload-free: [`fmt::Display`] and
//! [`fmt::Debug`] render only a fixed, developer-authored label and never the
//! project/task/session id, title, directory, HTTP body, credentials, SQL or the
//! underlying transport text. The internal diagnostic cause (a redacted storage
//! or session error) is reachable only through [`std::error::Error::source`].
//! [`ResolvedSession`] carries the resolved id but its [`fmt::Debug`]/[`fmt::Display`]
//! redact it.

use std::error::Error;
use std::fmt;
use std::path::Path;

use bridge_domain::TaskId;
use bridge_opencode::OpenCodeClient;
use bridge_storage::{RoundRef, RoundRow, RustStateLayout, StorageConnection};

use rusqlite::OptionalExtension;
use rusqlite::params;

/// Builds the deterministic session title for one round.
///
/// The title is exactly `agent-bridge {task_id} round {round_number}` and is the
/// byte-for-byte key used to match an existing session for the round.
#[must_use]
pub fn round_session_title(task_id: TaskId, round_number: u32) -> String {
    format!("agent-bridge {task_id} round {round_number}")
}

/// Broad, typed category of a [`SessionResolutionError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionResolutionErrorKind {
    /// The round reference itself is invalid (for example round zero).
    InvalidInput,
    /// The referenced task does not exist.
    UnknownTask,
    /// The task and the round reference disagree on the project or the round.
    TaskMismatch,
    /// The task workspace does not resolve to the client workspace.
    WorkspaceMismatch,
    /// The round is missing or is not the current round of its task.
    StaleRound,
    /// The state layout is not a valid, initialized Rust-owned state.
    StateOwnership,
    /// The session could not be listed/created, or its id is missing or unusable.
    SessionUnknown,
    /// More than one session matched the deterministic title.
    SessionAmbiguous,
    /// A matched or created session directory is absent or not the workspace.
    SessionDirectoryMismatch,
    /// The resolved session could not be bound atomically to the round.
    Storage,
}

impl SessionResolutionErrorKind {
    /// Returns a short, non-sensitive label for this category.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "session resolution input is invalid",
            Self::UnknownTask => "session resolution task does not exist",
            Self::TaskMismatch => "session resolution task does not match the round",
            Self::WorkspaceMismatch => "session resolution workspace does not match the task",
            Self::StaleRound => "session resolution round is missing or not current",
            Self::StateOwnership => "rust state ownership could not be verified",
            Self::SessionUnknown => "opencode session could not be resolved",
            Self::SessionAmbiguous => "opencode session title is ambiguous",
            Self::SessionDirectoryMismatch => {
                "opencode session directory does not match the workspace"
            }
            Self::Storage => "opencode session could not be bound to the round",
        }
    }
}

impl fmt::Display for SessionResolutionErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed, safe error raised while resolving a round session.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message and [`Debug`](fmt::Debug) shows only the category. Neither ever
/// contains a project/task/session id, the title, a directory, an HTTP body,
/// credentials, SQL or transport text. The internal diagnostic cause, when
/// present, is reachable only through [`Error::source`].
pub struct SessionResolutionError {
    kind: SessionResolutionErrorKind,
    source: Option<Box<dyn Error + Send + Sync + 'static>>,
}

impl SessionResolutionError {
    /// Creates an error of the given `kind` without a source.
    fn new(kind: SessionResolutionErrorKind) -> Self {
        Self { kind, source: None }
    }

    /// Creates an error of the given `kind` with an internal diagnostic source.
    fn with_source(
        kind: SessionResolutionErrorKind,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            source: Some(Box::new(source)),
        }
    }

    /// Returns the category of this error.
    #[must_use]
    pub const fn kind(&self) -> SessionResolutionErrorKind {
        self.kind
    }
}

impl fmt::Display for SessionResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.as_str())
    }
}

impl fmt::Debug for SessionResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionResolutionError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Error for SessionResolutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.source {
            Some(source) => Some(source.as_ref()),
            None => None,
        }
    }
}

/// How a [`ResolvedSession`] was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionResolutionSource {
    /// The round already had a persisted session; no HTTP request was made.
    Existing,
    /// An existing server session with the exact title was adopted.
    Adopted,
    /// A new server session was created with the exact title.
    Created,
}

/// The id of the one dedicated session resolved for a round.
///
/// The id is available through [`ResolvedSession::id`], but the
/// [`fmt::Debug`]/[`fmt::Display`] representations redact it.
pub struct ResolvedSession {
    id: String,
    source: SessionResolutionSource,
}

impl ResolvedSession {
    /// Wraps a resolved id and its origin.
    fn new(id: String, source: SessionResolutionSource) -> Self {
        Self { id, source }
    }

    /// Returns the resolved session id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns how the session was obtained.
    #[must_use]
    pub const fn source(&self) -> SessionResolutionSource {
        self.source
    }
}

impl fmt::Debug for ResolvedSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedSession")
            .field("id", &"[redacted]")
            .field("source", &self.source)
            .finish()
    }
}

impl fmt::Display for ResolvedSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OpenCode round session ({:?})", self.source)
    }
}

/// Resolves exactly one dedicated OpenCode session for `round`.
///
/// The Rust-owned state is identified by the explicit [`RustStateLayout`] and
/// opened through the production [`RustStateLayout::open`] ownership guard
/// before any read, HTTP request or write; the layout's project must equal
/// `round.project_id`. There is no unchecked [`StorageConnection`] parameter.
///
/// See the module documentation for the full contract, the concurrency
/// precondition (the caller holds the project [`crate::WorkerLock`] for the whole
/// call) and the directory comparison policy.
///
/// # Errors
///
/// Returns a typed [`SessionResolutionError`] with
/// [`SessionResolutionErrorKind`] `InvalidInput` before any read, `StateOwnership`
/// when the layout guard rejects the state (before any task/round read or HTTP
/// request), `TaskMismatch` when the layout project, task and round disagree,
/// `UnknownTask`, `WorkspaceMismatch` or `StaleRound` before any HTTP request,
/// `SessionUnknown`/`SessionAmbiguous`/`SessionDirectoryMismatch` for the
/// list/create outcomes, or `Storage` when the atomic binding fails. No error
/// message contains an id, title, directory, HTTP body, credential, SQL or path.
pub fn resolve_round_session(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    round: RoundRef,
) -> Result<ResolvedSession, SessionResolutionError> {
    if round.round_number == 0 {
        return Err(SessionResolutionError::new(
            SessionResolutionErrorKind::InvalidInput,
        ));
    }

    if layout.project_id() != &round.project_id {
        return Err(SessionResolutionError::new(
            SessionResolutionErrorKind::TaskMismatch,
        ));
    }

    let mut storage = layout.open().map_err(|error| {
        SessionResolutionError::with_source(SessionResolutionErrorKind::StateOwnership, error)
    })?;

    let task = storage
        .get_task(round.task_id)
        .map_err(|error| {
            SessionResolutionError::with_source(SessionResolutionErrorKind::Storage, error)
        })?
        .ok_or_else(|| SessionResolutionError::new(SessionResolutionErrorKind::UnknownTask))?;
    if task.project_id != round.project_id {
        return Err(SessionResolutionError::new(
            SessionResolutionErrorKind::TaskMismatch,
        ));
    }

    let persisted = read_round(&storage, &round)?;
    if persisted.project_id != round.project_id || persisted.task_id != round.task_id {
        return Err(SessionResolutionError::new(
            SessionResolutionErrorKind::TaskMismatch,
        ));
    }
    if current_round_number(&storage, round.task_id)? != round.round_number {
        return Err(SessionResolutionError::new(
            SessionResolutionErrorKind::StaleRound,
        ));
    }
    if crate::execution::execution_root(&storage, layout, &task, true)
        .ok()
        .as_deref()
        != Some(client.workspace())
    {
        return Err(SessionResolutionError::new(
            SessionResolutionErrorKind::WorkspaceMismatch,
        ));
    }

    if let Some(session_id) = persisted.session_id.as_deref().filter(|id| !id.is_empty()) {
        return Ok(ResolvedSession::new(
            session_id.to_owned(),
            SessionResolutionSource::Existing,
        ));
    }

    let title = round_session_title(round.task_id, round.round_number);
    let sessions = client.list_sessions().map_err(|error| {
        SessionResolutionError::with_source(SessionResolutionErrorKind::SessionUnknown, error)
    })?;
    let mut matches = sessions
        .iter()
        .filter(|session| session.title() == Some(title.as_str()));
    match (matches.next(), matches.next()) {
        (Some(_), Some(_)) => Err(SessionResolutionError::new(
            SessionResolutionErrorKind::SessionAmbiguous,
        )),
        (Some(session), None) => {
            let id = usable_session_id(session.id()).ok_or_else(|| {
                SessionResolutionError::new(SessionResolutionErrorKind::SessionUnknown)
            })?;
            let directory = session.directory().ok_or_else(|| {
                SessionResolutionError::new(SessionResolutionErrorKind::SessionDirectoryMismatch)
            })?;
            if !directory_matches_workspace(directory, client.workspace()) {
                return Err(SessionResolutionError::new(
                    SessionResolutionErrorKind::SessionDirectoryMismatch,
                ));
            }
            let id = id.to_owned();
            bind_round(&mut storage, &round, &id)?;
            Ok(ResolvedSession::new(id, SessionResolutionSource::Adopted))
        }
        (None, _) => {
            let created = client.create_session(&title).map_err(|error| {
                SessionResolutionError::with_source(
                    SessionResolutionErrorKind::SessionUnknown,
                    error,
                )
            })?;
            let id = usable_session_id(created.id()).ok_or_else(|| {
                SessionResolutionError::new(SessionResolutionErrorKind::SessionUnknown)
            })?;
            if let Some(directory) = created.directory()
                && !directory_matches_workspace(directory, client.workspace())
            {
                return Err(SessionResolutionError::new(
                    SessionResolutionErrorKind::SessionDirectoryMismatch,
                ));
            }
            let id = id.to_owned();
            bind_round(&mut storage, &round, &id)?;
            Ok(ResolvedSession::new(id, SessionResolutionSource::Created))
        }
    }
}

/// Returns `id` when it is non-empty and starts with `ses`.
///
/// This mirrors the reference `id and id.startswith("ses")` rule; the separator
/// is deliberately not required to be `ses_`.
fn usable_session_id(id: Option<&str>) -> Option<&str> {
    id.filter(|id| !id.is_empty() && id.starts_with("ses"))
}

/// Reads the persisted current round through the production row mapping.
fn read_round(
    storage: &StorageConnection,
    round: &RoundRef,
) -> Result<RoundRow, SessionResolutionError> {
    let found = storage
        .connection()
        .query_row(
            "SELECT * FROM rounds WHERE task_id = ?1 AND round_number = ?2",
            params![round.task_id.to_string(), i64::from(round.round_number)],
            |row| Ok(RoundRow::from_row(row)),
        )
        .optional()
        .map_err(|error| {
            SessionResolutionError::with_source(SessionResolutionErrorKind::Storage, error)
        })?;
    match found {
        Some(Ok(row)) => Ok(row),
        Some(Err(error)) => Err(SessionResolutionError::with_source(
            SessionResolutionErrorKind::Storage,
            error,
        )),
        None => Err(SessionResolutionError::new(
            SessionResolutionErrorKind::StaleRound,
        )),
    }
}

/// Returns the highest round number of `task_id`.
fn current_round_number(
    storage: &StorageConnection,
    task_id: TaskId,
) -> Result<u32, SessionResolutionError> {
    let max: Option<i64> = storage
        .connection()
        .query_row(
            "SELECT MAX(round_number) FROM rounds WHERE task_id = ?1",
            params![task_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| {
            SessionResolutionError::with_source(SessionResolutionErrorKind::Storage, error)
        })?;
    max.and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value >= 1)
        .ok_or_else(|| SessionResolutionError::new(SessionResolutionErrorKind::StaleRound))
}

/// Binds `session_id` to the round through the atomic production write.
fn bind_round(
    storage: &mut StorageConnection,
    round: &RoundRef,
    session_id: &str,
) -> Result<(), SessionResolutionError> {
    storage
        .bind_round_session(round.clone(), session_id.to_owned())
        .map(drop)
        .map_err(|error| {
            SessionResolutionError::with_source(SessionResolutionErrorKind::Storage, error)
        })
}

/// Reports whether a server-reported directory resolves to the workspace.
///
/// The comparison is fail-closed: a relative path (whose resolution would depend
/// on the process current directory), an embedded NUL, a missing/non-resolvable
/// path and any path that resolves to a different tree all report `false`. A
/// lexical prefix of the workspace is never accepted.
pub(crate) fn directory_matches_workspace(directory: &str, workspace: &Path) -> bool {
    if directory.as_bytes().contains(&0) {
        return false;
    }
    let directory = Path::new(directory);
    if !directory.is_absolute() {
        return false;
    }
    std::fs::canonicalize(directory).is_ok_and(|resolved| resolved == workspace)
}

#[cfg(test)]
mod tests {
    use super::{
        ResolvedSession, SessionResolutionSource, directory_matches_workspace, usable_session_id,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bridge-worker-session-unit-{label}-{nanos}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temporary root");
        path
    }

    #[test]
    fn title_is_exact_and_byte_for_byte() {
        let task_id = bridge_domain::TaskId::from_str("550e8400-e29b-41d4-a716-446655440000")
            .expect("task id");

        assert_eq!(
            super::round_session_title(task_id, 3),
            "agent-bridge 550e8400-e29b-41d4-a716-446655440000 round 3"
        );
    }

    #[test]
    fn usable_id_requires_the_reference_prefix() {
        assert_eq!(usable_session_id(Some("ses_abc")), Some("ses_abc"));
        assert_eq!(usable_session_id(Some("ses-abc")), Some("ses-abc"));
        assert_eq!(usable_session_id(Some("ses")), Some("ses"));
        assert_eq!(usable_session_id(Some("")), None);
        assert_eq!(usable_session_id(Some("other")), None);
        assert_eq!(usable_session_id(None), None);
    }

    #[test]
    fn directory_matching_is_resolved_and_fail_closed() {
        let root = unique_dir("dir");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let workspace = fs::canonicalize(&workspace).expect("canonical workspace");

        assert!(directory_matches_workspace(
            workspace.to_str().expect("utf-8"),
            &workspace
        ));
        assert!(directory_matches_workspace(
            &format!("{}/", workspace.display()),
            &workspace
        ));
        assert!(directory_matches_workspace(
            &format!("{}/.", workspace.display()),
            &workspace
        ));

        let sibling = root.join("workspace-extra");
        fs::create_dir_all(&sibling).expect("sibling");
        assert!(!directory_matches_workspace(
            sibling.to_str().expect("utf-8"),
            &workspace
        ));

        // A missing directory and a relative directory both fail closed.
        assert!(!directory_matches_workspace(
            root.join("missing").to_str().expect("utf-8"),
            &workspace
        ));
        assert!(!directory_matches_workspace("workspace", &workspace));

        // A lexical prefix is never accepted.
        let prefix = workspace.parent().expect("parent");
        assert!(!directory_matches_workspace(
            prefix.to_str().expect("utf-8"),
            &workspace
        ));

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_of_the_workspace_matches() {
        let root = unique_dir("symlink");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let workspace = fs::canonicalize(&workspace).expect("canonical workspace");
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&workspace, &alias).expect("symlink");

        assert!(directory_matches_workspace(
            alias.to_str().expect("utf-8"),
            &workspace
        ));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolved_session_redacts_its_id() {
        let session =
            ResolvedSession::new("ses-secret-id".to_owned(), SessionResolutionSource::Adopted);
        let rendered = format!("{session} {session:?}");
        assert!(!rendered.contains("ses-secret-id"));
        assert!(rendered.contains("Adopted"));
        assert_eq!(session.id(), "ses-secret-id");
    }

    #[test]
    fn error_rendering_is_redacted() {
        let error = super::SessionResolutionError::new(
            super::SessionResolutionErrorKind::SessionDirectoryMismatch,
        );
        let rendered = format!("{error} {error:?}");
        assert_eq!(
            rendered,
            "opencode session directory does not match the workspace \
             SessionResolutionError { kind: SessionDirectoryMismatch, .. }"
        );
    }
}
