//! Current-round permission blockers and optional once-only approval (7.6/7.8a).
//!
//! The historical `handle_permission_blocker` contract below remains no-reply.
//! `handle_permission_blocker_with_auto_approval` adds configured decisions,
//! replies only `once`, keeps failed replies as blockers, and binds its success
//! cache to one round/session. It revalidates state around HTTP and preserves
//! needs_user idempotence. Question blockers and observer/FSM wiring are separate.
//!
//! This module is the narrow, reusable production step that turns the pending
//! OpenCode permissions of one round into a persisted `needs_user` blocker. It
//! builds on the existing [`bridge_opencode::OpenCodeClient::list_permissions`]
//! operation, the atomic [`bridge_storage::StorageConnection::finish_round`]
//! lifecycle write and the pure [`crate::round_session_title`] helper. It
//! deliberately stops before question blockers (7.7), auto-approval (7.8),
//! failed/delivery-unknown handling (7.9), continuation recovery (7.10),
//! verification integration (7.11), cooperative close (7.12), the observation
//! loop, the worker state machine, MCP/runtime wiring and the production CLI
//! `worker` subcommand.
//!
//! # Detection contract
//!
//! [`handle_permission_blocker`] performs, in order:
//!
//! 1. **Pre-flight validation, before any HTTP request or write.** The explicit
//!    [`RustStateLayout`] is opened through the production ownership guard
//!    [`RustStateLayout::open`] (sidecar marker, Rust implementation and format
//!    version, project namespace, normalized state root, `meta.runtime_owner`
//!    and schema v6). The layout project must equal `round.project_id`; the task
//!    must exist and belong to the same project; the round must exist, agree on
//!    task/project, and be the current (highest-numbered) round of its task; the
//!    task workspace must resolve to the client workspace. A pending cooperative
//!    close (`close_requested_at`) fails closed with
//!    [`PermissionBlockerErrorKind::CloseRequested`] before any HTTP or write.
//! 2. **Observability gate.** The round must be `observing` (the normal write
//!    path) or already `needs_user` (the idempotent replay path). The task must
//!    be `implementing`/`revising` on the write path or `needs_user` on the
//!    replay path. Any other round/task state fails closed with
//!    [`PermissionBlockerErrorKind::RoundNotObservable`] /
//!    [`PermissionBlockerErrorKind::TaskNotObservable`] before any HTTP request.
//! 3. **Current session.** The current round's persisted `rounds.session_id`
//!    identifies the one session this round owns. A missing or empty session
//!    fails closed with [`PermissionBlockerErrorKind::SessionUnknown`]; the
//!    previous round's session and `tasks.session_id` are never consulted.
//! 4. **Filtering.** [`bridge_opencode::OpenCodeClient::list_permissions`] is
//!    called exactly once and every returned permission is filtered by the
//!    exact, byte-for-byte current session id
//!    ([`bridge_opencode::Permission::belongs_to_session`]). Permissions of any
//!    other session never block. An empty or foreign-only result is a typed
//!    [`PermissionBlockerOutcome::NoBlocker`] **no-op**: nothing is written and
//!    the round/task stay untouched.
//! 5. **Post-HTTP revalidation.** A cooperative close writer
//!    ([`bridge_storage::StorageConnection::request_task_close`]) does not take
//!    the project [`crate::WorkerLock`], so the persisted task/round/session is
//!    read again *after* the permission GET and *before any outcome is derived*.
//!    A `close_requested_at` that appeared during the request, a task/round that
//!    is no longer current, a session change or a changed observability pair is
//!    classified from this fresh state: a pending close fails closed with
//!    [`PermissionBlockerErrorKind::CloseRequested`], and the write/replay
//!    decision and the returned round/task are never taken from the stale
//!    pre-HTTP snapshot.
//! 6. **Persist `needs_user` exactly once.** When at least one permission
//!    belongs to the current session and the round is still `observing`, the
//!    round and task are moved to `needs_user` through the single atomic
//!    [`finish_round`](bridge_storage::StorageConnection::finish_round) call
//!    with `error_code = "needs_user"` and a `result_json` carrying the typed
//!    `blockers` list, exactly like the reference `worker.py` observation path
//!    (minus auto-approval and questions, which are out of scope). The committed
//!    task returned by that call is authoritative: if a cooperative close landed
//!    in the last window before the atomic write and the committed task is
//!    `closed` (or carries `close_requested_at`), the call returns
//!    [`PermissionBlockerErrorKind::CloseRequested`] instead of a false
//!    `Blocked`/`UserAction`. No permission is ever answered:
//!    [`bridge_opencode::OpenCodeClient::reply_permission`] is never called, so
//!    there is no auto-approval and no automatic external access.
//! 7. **Idempotent replay.** If the revalidated round is already `needs_user`,
//!    the same filtering runs but **no lifecycle write happens**, so a repeated
//!    call never appends a second `needs_user` event or moves `updated_at`
//!    again. The replay returns the revalidated round/task together with the
//!    freshly listed current-session permissions and the same typed user action.
//!    A replay that finds no current-session permission returns
//!    [`PermissionBlockerOutcome::NoBlocker`] without changing the persisted
//!    `needs_user` state: resolving a cleared blocker is the continuation
//!    recovery step (7.10), which this foundation deliberately does not
//!    implement.
//!
//! # User action
//!
//! A blocking outcome carries a typed [`UserAction`] reproducing the reference
//! `mcp_server.py::_user_action` contract: the `open_project_console` type, the
//! human-readable message and instructions, the current session id and the
//! deterministic session title (`agent-bridge <task_id> round <n>`). The
//! `command` and `fallback_command` fields are part of the typed shape but are
//! left `None` here: the ready-to-run `console`/`attach-opencode` command
//! builders belong to the runtime CLI (stream 9) and are intentionally not part
//! of this narrow worker foundation. The future MCP/runtime layer populates
//! them; no command, path or credential is fabricated in this step.
//!
//! # Errors and redaction
//!
//! A transport, malformed-response or storage failure is a typed
//! [`PermissionBlockerError`] and is never a false success. Unlike the
//! reference `worker.py::_pending_permissions`, which swallows an OpenCode error
//! and keeps observing, this foundation surfaces the failure fail closed as
//! [`PermissionBlockerErrorKind::Permissions`], so a transient permission-list
//! failure cannot be mistaken for "no blocker".
//!
//! [`fmt::Display`] and [`fmt::Debug`] render only fixed, developer-authored
//! labels and never the project/task/session id, session title, permission name,
//! patterns, HTTP body, credentials, SQL or paths. The internal diagnostic cause
//! is reachable only through [`std::error::Error::source`]. The typed outcome
//! types redact the session id, session title, permission details and user
//! action in their renderings; exact values are available only through the
//! explicit accessors.
//!
//! # Concurrency precondition
//!
//! The caller **must hold the project [`crate::WorkerLock`] for the whole call**,
//! acquired from the same [`RustStateLayout`] passed here, so the lock and the
//! checked state refer to the same verified project namespace. The lock
//! serializes workers of one project, so two calls cannot both observe an
//! `observing` round and each persist a `needs_user` transition. This module
//! does not acquire the lock itself.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::path::Path;

use bridge_domain::{RoundStatus, TaskId, TaskStatus};
use bridge_opencode::{OpenCodeClient, Permission, PermissionReply};
use bridge_storage::{
    FinishRoundInput, RoundRef, RoundRow, RustStateLayout, StorageConnection, Task,
};

use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::session::{directory_matches_workspace, round_session_title};

/// The stable blocker reason recorded while automatic approval is disabled.
///
/// Automatic approval belongs to task 7.8; this foundation records every
/// current-session permission as a blocker and never answers it. The reason is
/// a non-contract diagnostic, mirroring the reference
/// `worker.py::_permission_blocker` `reason` field.
pub const PERMISSION_BLOCKER_REASON: &str = "auto_approval_disabled";

/// The machine-readable `error_code` persisted for a `needs_user` blocker.
///
/// This is the exact reference `worker.py` spelling.
pub const NEEDS_USER_ERROR_CODE: &str = "needs_user";

/// The `open_project_console` user-action type.
const USER_ACTION_CONSOLE: &str = "open_project_console";

/// The reference `mcp_server.py::USER_ACTION_INSTRUCTIONS` text (session known).
const USER_ACTION_INSTRUCTIONS: &str = "Откройте проектную OpenCode TUI командой из command \
     только если она ещё не открыта. В уже открытой TUI переключитесь на session_id/session_title, \
     осознанно ответьте на запрос разрешения (permission) или вопрос (question) и не разрешайте \
     внешний доступ автоматически. fallback_command открывает нужную сессию напрямую. Затем \
     повторите task_status(wait_seconds=300).";

/// The reference `mcp_server.py::_user_action` message when a session exists.
const USER_ACTION_MESSAGE: &str = "OpenCode приостановил задачу и ждёт ответа пользователя: нужно \
     обработать запрос разрешения (permission) или вопрос (question) в OpenCode TUI.";

/// Broad, typed category of a [`PermissionBlockerError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PermissionBlockerErrorKind {
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
    /// The task has a pending cooperative close request.
    CloseRequested,
    /// The round is not `observing` and not an already-persisted `needs_user`.
    RoundNotObservable,
    /// The task is not in the status the round state requires.
    TaskNotObservable,
    /// The current round has no persisted OpenCode session.
    SessionUnknown,
    /// The permission list could not be fetched or parsed.
    Permissions,
    /// The `needs_user` lifecycle write failed.
    Storage,
}

impl PermissionBlockerErrorKind {
    /// Returns a short, non-sensitive label for this category.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "permission blocker input is invalid",
            Self::UnknownTask => "permission blocker task does not exist",
            Self::TaskMismatch => "permission blocker task does not match the round",
            Self::WorkspaceMismatch => "permission blocker workspace does not match the task",
            Self::StaleRound => "permission blocker round is missing or not current",
            Self::StateOwnership => "rust state ownership could not be verified",
            Self::CloseRequested => "task has a pending close request",
            Self::RoundNotObservable => "round is not observable for permission blockers",
            Self::TaskNotObservable => "task is not observable for permission blockers",
            Self::SessionUnknown => "current round has no OpenCode session",
            Self::Permissions => "opencode permissions could not be listed",
            Self::Storage => "permission blocker could not be persisted",
        }
    }
}

impl fmt::Display for PermissionBlockerErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed, safe error raised while handling a permission blocker.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message and [`Debug`](fmt::Debug) shows only the category. Neither ever
/// contains a project/task/session id, title, permission name, patterns, HTTP
/// body, credentials, SQL or paths. The internal diagnostic cause, when present,
/// is reachable only through [`Error::source`].
pub struct PermissionBlockerError {
    kind: PermissionBlockerErrorKind,
    source: Option<Box<dyn Error + Send + Sync + 'static>>,
}

impl PermissionBlockerError {
    /// Creates an error of the given `kind` without a source.
    pub(crate) fn new(kind: PermissionBlockerErrorKind) -> Self {
        Self { kind, source: None }
    }

    /// Creates an error of the given `kind` with an internal diagnostic source.
    fn with_source(
        kind: PermissionBlockerErrorKind,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            source: Some(Box::new(source)),
        }
    }

    /// Returns the category of this error.
    #[must_use]
    pub const fn kind(&self) -> PermissionBlockerErrorKind {
        self.kind
    }
}

impl fmt::Display for PermissionBlockerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.as_str())
    }
}

impl fmt::Debug for PermissionBlockerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermissionBlockerError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Error for PermissionBlockerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.source {
            Some(source) => Some(source.as_ref()),
            None => None,
        }
    }
}

/// The `open_project_console` user-action type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UserActionKind {
    /// Open the reusable project console and answer the blocker in the TUI.
    OpenProjectConsole,
}

impl UserActionKind {
    /// Returns the exact reference wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenProjectConsole => USER_ACTION_CONSOLE,
        }
    }
}

impl fmt::Display for UserActionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The typed next step for a `needs_user` task (reference
/// `mcp_server.py::_user_action`).
///
/// The [`fmt::Debug`]/[`fmt::Display`] representations redact the session id,
/// session title and the optional commands; exact values are available only
/// through the explicit accessors. `command`/`fallback_command` are `None` in
/// this narrow foundation because the ready-to-run CLI command builders belong
/// to the runtime CLI (stream 9); see the module documentation.
#[derive(Clone, PartialEq, Eq)]
pub struct UserAction {
    kind: UserActionKind,
    message: &'static str,
    command: Option<String>,
    session_id: Option<String>,
    session_title: Option<String>,
    fallback_command: Option<String>,
    instructions: &'static str,
}

impl UserAction {
    /// Builds the session-aware user action for `session_id`/`session_title`.
    pub(crate) fn for_session(session_id: String, session_title: String) -> Self {
        Self {
            kind: UserActionKind::OpenProjectConsole,
            message: USER_ACTION_MESSAGE,
            command: None,
            session_id: Some(session_id),
            session_title: Some(session_title),
            fallback_command: None,
            instructions: USER_ACTION_INSTRUCTIONS,
        }
    }

    /// Returns the user-action type.
    #[must_use]
    pub const fn kind(&self) -> UserActionKind {
        self.kind
    }

    /// Returns the human-readable message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }

    /// Returns the project-console command, when the runtime layer supplies it.
    #[must_use]
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// Returns the OpenCode session id to select, when known.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Returns the deterministic session title, when known.
    #[must_use]
    pub fn session_title(&self) -> Option<&str> {
        self.session_title.as_deref()
    }

    /// Returns the direct-session fallback command, when the runtime layer
    /// supplies it.
    #[must_use]
    pub fn fallback_command(&self) -> Option<&str> {
        self.fallback_command.as_deref()
    }

    /// Returns the human-readable instructions.
    #[must_use]
    pub const fn instructions(&self) -> &'static str {
        self.instructions
    }
}

impl fmt::Debug for UserAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserAction")
            .field("kind", &self.kind)
            .field("message", &"[redacted]")
            .field("command", &self.command.as_ref().map(|_| "[redacted]"))
            .field(
                "session_id",
                &self.session_id.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "session_title",
                &self.session_title.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "fallback_command",
                &self.fallback_command.as_ref().map(|_| "[redacted]"),
            )
            .field("instructions", &"[redacted]")
            .finish()
    }
}

impl fmt::Display for UserAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OpenCode user action ({})", self.kind)
    }
}

/// One current-session permission blocker entry.
///
/// The [`fmt::Debug`]/[`fmt::Display`] representations redact the permission
/// name and patterns; exact values are available only through the accessors.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingPermission {
    permission: Option<String>,
    patterns: Vec<String>,
    reason: &'static str,
    detail: Option<String>,
}

impl PendingPermission {
    /// Projects one typed OpenCode permission onto the blocker entry.
    fn from_permission(permission: &Permission) -> Self {
        Self {
            permission: permission.permission().map(str::to_owned),
            patterns: permission.patterns().to_vec(),
            reason: PERMISSION_BLOCKER_REASON,
            detail: None,
        }
    }

    /// Returns the permission name, when the server reported one.
    #[must_use]
    pub fn permission(&self) -> Option<&str> {
        self.permission.as_deref()
    }

    /// Returns the requested patterns.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    #[must_use]
    pub const fn reason(&self) -> &'static str {
        self.reason
    }
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// Renders the reference `worker.py::_permission_blocker` JSON entry.
    fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "type": "permission", "permission": &self.permission,
            "patterns": &self.patterns, "reason": self.reason,
        });
        if let Some(detail) = &self.detail {
            value["detail"] = serde_json::Value::String(detail.clone());
        }
        value
    }
}

impl fmt::Debug for PendingPermission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingPermission")
            .field(
                "permission",
                &self.permission.as_ref().map(|_| "[redacted]"),
            )
            .field("patterns", &self.patterns.len())
            .finish()
    }
}

impl fmt::Display for PendingPermission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode permission blocker (permission present: {}, patterns: {})",
            self.permission.is_some(),
            self.patterns.len()
        )
    }
}

/// The persisted result of one detected current-session permission blocker.
///
/// The [`fmt::Debug`] representation redacts the session id, title, permission
/// details and user action; exact values are available only through the
/// accessors.
pub struct PermissionBlocker {
    session_id: String,
    session_title: String,
    permissions: Vec<PendingPermission>,
    user_action: UserAction,
    round: RoundRow,
    task: Task,
}

impl PermissionBlocker {
    /// Returns the current round's session id.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the deterministic session title.
    #[must_use]
    pub fn session_title(&self) -> &str {
        &self.session_title
    }

    /// Returns the current-session permission blockers.
    #[must_use]
    pub fn permissions(&self) -> &[PendingPermission] {
        &self.permissions
    }

    /// Returns the typed user action.
    #[must_use]
    pub const fn user_action(&self) -> &UserAction {
        &self.user_action
    }

    /// Returns the persisted round after the blocker (or the replay read).
    #[must_use]
    pub fn round(&self) -> &RoundRow {
        &self.round
    }

    /// Returns the persisted task after the blocker (or the replay read).
    #[must_use]
    pub fn task(&self) -> &Task {
        &self.task
    }
}

impl fmt::Debug for PermissionBlocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermissionBlocker")
            .field("session_id", &"[redacted]")
            .field("session_title", &"[redacted]")
            .field("permissions", &self.permissions.len())
            .field("user_action", &self.user_action)
            .field("round_number", &self.round.round_number)
            .field("round_status", &self.round.status)
            .field("task_status", &self.task.status)
            .finish()
    }
}

impl fmt::Display for PermissionBlocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenCode permission blocker (round {}, permissions: {})",
            self.round.round_number,
            self.permissions.len()
        )
    }
}

/// The typed result of one permission-blocker check.
#[derive(Debug)]
#[non_exhaustive]
pub enum PermissionBlockerOutcome {
    /// At least one current-session permission blocks; `needs_user` is
    /// persisted (or was already persisted on an idempotent replay).
    Blocked(Box<PermissionBlocker>),
    /// No current-session permission blocks; nothing was written.
    NoBlocker,
}

impl PermissionBlockerOutcome {
    /// Returns the blocker when the outcome is [`PermissionBlockerOutcome::Blocked`].
    #[must_use]
    pub fn blocked(&self) -> Option<&PermissionBlocker> {
        match self {
            Self::Blocked(blocker) => Some(blocker.as_ref()),
            Self::NoBlocker => None,
        }
    }

    /// Returns `true` when the outcome is [`PermissionBlockerOutcome::Blocked`].
    #[must_use]
    pub const fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked(_))
    }
}

impl fmt::Display for PermissionBlockerOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blocked(blocker) => write!(f, "{blocker}"),
            Self::NoBlocker => f.write_str("no OpenCode permission blocker"),
        }
    }
}

/// Detects and persists a current-session permission blocker for `round`.
///
/// The Rust-owned state is identified by the explicit [`RustStateLayout`] and
/// opened through the production [`RustStateLayout::open`] ownership guard
/// before any read, HTTP request or write; the layout project must equal
/// `round.project_id`. See the module documentation for the full contract, the
/// user-action shape, the concurrency precondition (the caller holds the project
/// [`crate::WorkerLock`] for the whole call) and the deliberate fail-closed
/// deviations from the reference.
///
/// # Errors
///
/// Returns a typed [`PermissionBlockerError`] with
/// [`PermissionBlockerErrorKind`] `InvalidInput` before any read,
/// `StateOwnership` when the layout guard rejects the state, `TaskMismatch`/
/// `UnknownTask`/`WorkspaceMismatch`/`StaleRound` when the
/// task/round/project/workspace/current-round validation fails, `CloseRequested`
/// for a pending cooperative close (before the HTTP request or discovered by the
/// post-HTTP revalidation/committed `finish_round` outcome), `RoundNotObservable`/
/// `TaskNotObservable` when the round/task is not in an observable state,
/// `SessionUnknown` when the current round has no persisted session,
/// `Permissions` when the permission list cannot be fetched or parsed, or
/// `Storage` when the `needs_user` write fails. No error message contains ids,
/// title, permission content, HTTP body, SQL or paths.
pub fn handle_permission_blocker(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    round: RoundRef,
) -> Result<PermissionBlockerOutcome, PermissionBlockerError> {
    handle_permission_blocker_inner(client, layout, round, None)
}

/// Successful replies are bound to exactly one round/session. Keep this buffer
/// for the observer lifetime; a new worker may get newly issued requests.
#[derive(Default)]
pub struct PermissionReplies {
    bound: Option<(RoundRef, String)>,
    replied: HashSet<String>,
    approvals: Vec<serde_json::Value>,
}
impl PermissionReplies {
    #[must_use]
    pub fn approvals(&self) -> &[serde_json::Value] {
        &self.approvals
    }
    fn bind(&mut self, round: &RoundRef, session: &str) -> bool {
        match &self.bound {
            Some((saved, id)) => saved == round && id == session,
            None => {
                self.bound = Some((round.clone(), session.to_owned()));
                true
            }
        }
    }
}
impl fmt::Debug for PermissionReplies {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermissionReplies")
            .field("approved", &self.replied.len())
            .finish_non_exhaustive()
    }
}

/// Replies once to proven configured requests of the current session and
/// persists all remaining blockers. Caller holds the same lifecycle fence as
/// for `handle_permission_blocker`. The saved direct-round guard is retained;
/// worktree observer/FSM wiring remains a separate consumer.
/// # Errors
/// Ownership, project/workspace/round/session changes, close requests, malformed
/// GET and storage failures fail closed. Failed POST remains a reply_failed blocker.
pub fn handle_permission_blocker_with_auto_approval(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    round: RoundRef,
    project: &bridge_config::ProjectEntry,
    replies: &mut PermissionReplies,
) -> Result<PermissionBlockerOutcome, PermissionBlockerError> {
    if project.id() != &round.project_id {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::TaskMismatch,
        ));
    }
    if std::fs::canonicalize(project.workspace()).ok().as_deref() != Some(client.workspace()) {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::WorkspaceMismatch,
        ));
    }
    handle_permission_blocker_inner(client, layout, round, Some((project, replies)))
}

fn handle_permission_blocker_inner(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    round: RoundRef,
    mut auto: Option<(&bridge_config::ProjectEntry, &mut PermissionReplies)>,
) -> Result<PermissionBlockerOutcome, PermissionBlockerError> {
    if round.round_number == 0 {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::InvalidInput,
        ));
    }
    if layout.project_id() != &round.project_id {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::TaskMismatch,
        ));
    }

    let mut storage = open_state(layout)?;

    // Pre-flight validation, before any HTTP request or write. A pending
    // cooperative close fails closed here without touching the network.
    let before = inspect_round_state(&storage, &round, client.workspace())?;
    let approvals_start = if let Some((_, replies)) = auto.as_mut() {
        if !replies.bind(&round, &before.session_id) {
            return Err(PermissionBlockerError::new(
                PermissionBlockerErrorKind::InvalidInput,
            ));
        }
        replies.approvals.len()
    } else {
        0
    };

    let listed = client.list_permissions().map_err(|error| {
        PermissionBlockerError::with_source(PermissionBlockerErrorKind::Permissions, error)
    })?;

    // Revalidate the persisted task/round/session after the HTTP round trip
    // before deriving any outcome. A cooperative close writer does not take the
    // `WorkerLock`, so `request_task_close` may have landed while the permission
    // GET was in flight. Every outcome below is built from this fresh state, so
    // a task that is now closed or has a pending close is never reported as a
    // `Blocked`/`UserAction` and the replay path never returns a stale snapshot.
    let state = inspect_round_state(&storage, &round, client.workspace())?;
    let session_id = state.session_id;
    let session_title = round_session_title(round.task_id, round.round_number);

    if auto.is_some() && before.session_id != session_id {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::StaleRound,
        ));
    }
    let mut pending = Vec::new();
    for permission in listed
        .iter()
        .filter(|permission| permission.belongs_to_session(&session_id))
    {
        let mut blocker = PendingPermission::from_permission(permission);
        if let Some((project, replies)) = auto.as_mut() {
            let current = inspect_round_state(&storage, &round, client.workspace())?;
            if current.session_id != session_id {
                return Err(PermissionBlockerError::new(
                    PermissionBlockerErrorKind::StaleRound,
                ));
            }
            let id = permission.id().unwrap_or("");
            if replies.replied.contains(id) {
                continue;
            }
            let raw = serde_json::json!({"id":permission.id(),"permission":permission.permission(),"patterns":permission.patterns(),"metadata":permission.metadata()});
            let decision =
                crate::auto_approval::permission_decision(project, layout.state_root(), &raw);
            blocker.reason = decision.reason();
            blocker.detail = decision.detail().map(str::to_owned);
            if decision.approved() {
                let current = inspect_round_state(&storage, &round, client.workspace())?;
                if current.session_id != session_id {
                    return Err(PermissionBlockerError::new(
                        PermissionBlockerErrorKind::StaleRound,
                    ));
                }
                if client
                    .reply_permission(id, PermissionReply::Once, None)
                    .is_ok()
                {
                    replies.replied.insert(id.to_owned());
                    replies
                        .approvals
                        .push(serde_json::json!({"id":id,"permission":permission.permission()}));
                    continue;
                }
                blocker.reason = "reply_failed";
                blocker.detail = None;
            }
        }
        pending.push(blocker);
    }
    // POST may synchronously change the task or session. Revalidate every outcome.
    let state = inspect_round_state(&storage, &round, client.workspace())?;
    if auto.is_some() && state.session_id != session_id {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::StaleRound,
        ));
    }

    if pending.is_empty() {
        return Ok(PermissionBlockerOutcome::NoBlocker);
    }

    let user_action = UserAction::for_session(session_id.clone(), session_title.clone());

    if state.already_blocked {
        return Ok(PermissionBlockerOutcome::Blocked(Box::new(
            PermissionBlocker {
                session_id,
                session_title,
                permissions: pending,
                user_action,
                round: state.row,
                task: state.task,
            },
        )));
    }

    let blockers: Vec<serde_json::Value> = pending.iter().map(PendingPermission::to_json).collect();
    let mut result = serde_json::json!({"blockers":blockers});
    if let Some((_, replies)) = auto.as_ref()
        && replies.approvals.len() > approvals_start
    {
        result["auto_approved"] = serde_json::json!(&replies.approvals[approvals_start..]);
    }
    let outcome = storage
        .finish_round(FinishRoundInput {
            round: round.clone(),
            round_status: RoundStatus::NeedsUser,
            task_status: TaskStatus::NeedsUser,
            response_message_id: None,
            response: None,
            error_code: Some(NEEDS_USER_ERROR_CODE.to_owned()),
            result_json: Some(result),
        })
        .map_err(|error| {
            PermissionBlockerError::with_source(PermissionBlockerErrorKind::Storage, error)
        })?;

    // `finish_round` is the last point where a cooperative close can land: it
    // honours a `close_requested_at` that appeared after the revalidation and
    // returns the actually committed task. Never report `Blocked`/`UserAction`
    // when the task was really closed; the committed close is the authority.
    if outcome.task.status == TaskStatus::Closed || outcome.task.close_requested_at.is_some() {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::CloseRequested,
        ));
    }

    Ok(PermissionBlockerOutcome::Blocked(Box::new(
        PermissionBlocker {
            session_id,
            session_title,
            permissions: pending,
            user_action,
            round: outcome.round,
            task: outcome.task,
        },
    )))
}

/// The persisted task/round/session state validated around the HTTP round trip.
pub(crate) struct InspectedRound {
    pub(crate) task: Task,
    pub(crate) row: RoundRow,
    pub(crate) already_blocked: bool,
    pub(crate) session_id: String,
}

/// Validates and reads the current task/round state fail closed.
///
/// The layout project is already checked by the caller. This function enforces
/// task existence, project/task agreement, current-round freshness, resolved
/// workspace equality, the absence of a pending cooperative close, the
/// round/task observability pair and the current session, returning the mapped
/// rows plus the write/replay decision. It is called both before the permission
/// HTTP request (so ownership/state errors and a pending close fail without any
/// network access) and again after it (so any outcome reflects the state that
/// survived the round trip).
pub(crate) fn inspect_round_state(
    storage: &StorageConnection,
    round: &RoundRef,
    workspace: &Path,
) -> Result<InspectedRound, PermissionBlockerError> {
    let task = load_task(storage, round.task_id)?;
    if task.project_id != round.project_id {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::TaskMismatch,
        ));
    }

    let row = read_round(storage, round)?;
    if row.project_id != round.project_id || row.task_id != round.task_id {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::TaskMismatch,
        ));
    }
    if current_round_number(storage, round.task_id)? != round.round_number {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::StaleRound,
        ));
    }
    if !directory_matches_workspace(&task.workspace, workspace) {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::WorkspaceMismatch,
        ));
    }
    if task.close_requested_at.is_some() {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::CloseRequested,
        ));
    }

    let already_blocked = match row.status {
        RoundStatus::Observing => false,
        RoundStatus::NeedsUser => true,
        _ => {
            return Err(PermissionBlockerError::new(
                PermissionBlockerErrorKind::RoundNotObservable,
            ));
        }
    };
    if already_blocked {
        if task.status != TaskStatus::NeedsUser {
            return Err(PermissionBlockerError::new(
                PermissionBlockerErrorKind::TaskNotObservable,
            ));
        }
    } else if !matches!(task.status, TaskStatus::Implementing | TaskStatus::Revising) {
        return Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::TaskNotObservable,
        ));
    }

    let session_id = row
        .session_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| PermissionBlockerError::new(PermissionBlockerErrorKind::SessionUnknown))?
        .to_owned();

    Ok(InspectedRound {
        task,
        row,
        already_blocked,
        session_id,
    })
}

/// Opens the layout through the production ownership guard.
pub(crate) fn open_state(
    layout: &RustStateLayout,
) -> Result<StorageConnection, PermissionBlockerError> {
    layout.open().map_err(|error| {
        PermissionBlockerError::with_source(PermissionBlockerErrorKind::StateOwnership, error)
    })
}

/// Loads a task by id, failing closed when it is absent.
fn load_task(storage: &StorageConnection, task_id: TaskId) -> Result<Task, PermissionBlockerError> {
    storage
        .get_task(task_id)
        .map_err(|error| {
            PermissionBlockerError::with_source(PermissionBlockerErrorKind::Storage, error)
        })?
        .ok_or_else(|| PermissionBlockerError::new(PermissionBlockerErrorKind::UnknownTask))
}

/// Reads the persisted current round through the production row mapping.
fn read_round(
    storage: &StorageConnection,
    round: &RoundRef,
) -> Result<RoundRow, PermissionBlockerError> {
    let found = storage
        .connection()
        .query_row(
            "SELECT * FROM rounds WHERE task_id = ?1 AND round_number = ?2",
            params![round.task_id.to_string(), i64::from(round.round_number)],
            |row| Ok(RoundRow::from_row(row)),
        )
        .optional()
        .map_err(|error| {
            PermissionBlockerError::with_source(PermissionBlockerErrorKind::Storage, error)
        })?;
    match found {
        Some(Ok(row)) => Ok(row),
        Some(Err(error)) => Err(PermissionBlockerError::with_source(
            PermissionBlockerErrorKind::Storage,
            error,
        )),
        None => Err(PermissionBlockerError::new(
            PermissionBlockerErrorKind::StaleRound,
        )),
    }
}

/// Returns the highest round number of `task_id`.
fn current_round_number(
    storage: &StorageConnection,
    task_id: TaskId,
) -> Result<u32, PermissionBlockerError> {
    let max: Option<i64> = storage
        .connection()
        .query_row(
            "SELECT MAX(round_number) FROM rounds WHERE task_id = ?1",
            params![task_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| {
            PermissionBlockerError::with_source(PermissionBlockerErrorKind::Storage, error)
        })?;
    max.and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value >= 1)
        .ok_or_else(|| PermissionBlockerError::new(PermissionBlockerErrorKind::StaleRound))
}

#[cfg(test)]
mod tests {
    use super::{
        NEEDS_USER_ERROR_CODE, PERMISSION_BLOCKER_REASON, PendingPermission,
        PermissionBlockerError, PermissionBlockerErrorKind, UserAction, UserActionKind,
    };

    #[test]
    fn error_rendering_is_redacted() {
        let error = PermissionBlockerError::new(PermissionBlockerErrorKind::SessionUnknown);
        let rendered = format!("{error} {error:?}");
        assert_eq!(
            rendered,
            "current round has no OpenCode session \
             PermissionBlockerError { kind: SessionUnknown, .. }"
        );
    }

    #[test]
    fn blocker_constants_match_the_reference() {
        assert_eq!(NEEDS_USER_ERROR_CODE, "needs_user");
        assert_eq!(PERMISSION_BLOCKER_REASON, "auto_approval_disabled");
        assert_eq!(
            UserActionKind::OpenProjectConsole.as_str(),
            "open_project_console"
        );
    }

    #[test]
    fn pending_permission_rendering_is_redacted() {
        let permission = PendingPermission {
            permission: Some("bash".to_owned()),
            patterns: vec!["rm -rf /".to_owned()],
            reason: PERMISSION_BLOCKER_REASON,
            detail: None,
        };
        let rendered = format!("{permission} {permission:?}");
        assert!(!rendered.contains("bash"));
        assert!(!rendered.contains("rm -rf"));
        assert_eq!(permission.permission(), Some("bash"));
        assert_eq!(permission.patterns(), ["rm -rf /"]);
    }

    #[test]
    fn user_action_rendering_is_redacted() {
        let action = UserAction::for_session("ses-secret".to_owned(), "title-secret".to_owned());
        let rendered = format!("{action} {action:?}");
        assert!(!rendered.contains("ses-secret"));
        assert!(!rendered.contains("title-secret"));
        assert_eq!(action.kind(), UserActionKind::OpenProjectConsole);
        assert_eq!(action.session_id(), Some("ses-secret"));
        assert_eq!(action.session_title(), Some("title-secret"));
        assert_eq!(action.command(), None);
        assert_eq!(action.fallback_command(), None);
        assert!(!action.instructions().is_empty());
    }
}
