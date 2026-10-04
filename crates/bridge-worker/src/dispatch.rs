//! Initial and revision prompt delivery, with strict revision findings (7.13).
//!
//! This module is the narrow, reusable production step that turns a validated
//! initial (`implement`) or revision (`revise`) round into exactly one delivered
//! OpenCode prompt. It builds on the task-7.3 session resolver, the pure
//! [`crate::initial_prompt`]/[`crate::revision_prompt`] builders and the
//! existing atomic storage lifecycle APIs
//! ([`bridge_storage::StorageConnection::prepare_round`],
//! [`bridge_storage::StorageConnection::mark_round_sent`] and
//! [`bridge_storage::StorageConnection::mark_round_observing`]).
//!
//! The initial and revision paths share one internal pipeline; only the expected
//! round kind, the expected task status and the rendered prompt differ.
//! [`dispatch_initial_round`] requires an `implement` round of an `implementing`
//! task and [`dispatch_revision_round`] requires a `revise` round of a
//! `revising` task. Each path rejects the other's kind before any side effect.
//!
//! It deliberately stops before completion observation, permission/question
//! blockers (7.6/7.7), auto-approval (7.8), failed/delivery-unknown handling
//! (7.9), continuation recovery (7.10), verification integration (7.11),
//! cooperative close (7.12) and every CLI/MCP/runtime wiring. There is no
//! observation loop and no automatic retry.
//!
//! # Dispatch contract
//!
//! [`dispatch_initial_round`]/[`dispatch_revision_round`] perform, in order:
//!
//! 1. **Pre-flight validation, before any HTTP request or write.** The explicit
//!    [`RustStateLayout`] is opened through the production ownership guard
//!    [`RustStateLayout::open`] (sidecar marker, Rust implementation and format
//!    version, project namespace, normalized state root, `meta.runtime_owner`
//!    and schema v6). The layout project must equal `round.project_id`; the task
//!    must exist and belong to the same project; the round must exist, agree on
//!    task/project, and be the current (highest-numbered) round of its task; the
//!    task workspace must resolve to the client workspace. The task must be in
//!    the expected status (`implementing` for an initial round, `revising` for a
//!    revision round) with no pending close request
//!    ([`DispatchErrorKind::TaskNotDispatchable`] otherwise), the round must be
//!    of the expected kind (`implement`/`revise`;
//!    [`DispatchErrorKind::RevisionNotSupported`] /
//!    [`DispatchErrorKind::ImplementNotSupported`] otherwise) and be `pending`
//!    and not yet attempted ([`DispatchErrorKind::RoundNotDispatchable`]
//!    otherwise). A stale, mismatched, foreign, cross-kind, already-dispatched
//!    or closed/close-requested input therefore fails closed before any side
//!    effect.
//! 2. **Session resolution.** The task-7.3 [`resolve_round_session`] resolves
//!    and atomically binds exactly one dedicated session for the round. Its
//!    `list`/`create` requests are the only HTTP before the prompt. The session
//!    is keyed to the current round only: the previous round's session and
//!    `tasks.session_id` are never reused, so every round gets an independent
//!    session.
//! 3. **Re-validation.** Because step 2 performs HTTP and a binding write, the
//!    task and current round are validated again immediately before any
//!    lifecycle write or prompt. A close request or task/round mutation that
//!    lands during session resolution is observed here and fails closed before
//!    `prepare`/`mark_sent`/`send`.
//! 4. **Prompt and outbound id.** The persisted task is re-read and the exact
//!    prompt is rendered by [`crate::initial_prompt`] or
//!    [`crate::revision_prompt`] (the revision findings come only from the
//!    persisted current round's `findings` column; `None` becomes the empty
//!    string, exactly like the reference `round_obj.findings or ""`). The
//!    outbound message id is reused when a previous prepare already persisted
//!    one, or allocated with [`new_message_id`] (`msg_` plus 32 lowercase hex,
//!    the reference `"msg_" + uuid.uuid4().hex`) and persisted through
//!    [`prepare_round`](bridge_storage::StorageConnection::prepare_round).
//! 5. **Persist-before-send.** [`mark_round_sent`](bridge_storage::StorageConnection::mark_round_sent)
//!    records the delivery attempt (round `sent`, `attempted = 1`) *before* the
//!    request, exactly like the reference `_send_or_resume`: a crash after this
//!    point can never turn an already-delivered prompt into a second delivery.
//! 6. **Single delivery.** [`OpenCodeClient::send_prompt_async`] is called
//!    exactly once with the resolved session id, the persisted outbound id and
//!    the exact prompt text. There is no retry.
//! 7. **Successful lifecycle.** On a successful `2xx` the round is moved to
//!    `observing` through
//!    [`mark_round_observing`](bridge_storage::StorageConnection::mark_round_observing)
//!    and the persisted round/task are returned. The task status is unchanged
//!    (`implementing` for an initial round, `revising` for a revision round); no
//!    observation is started.
//!
//! The persisted `attempted` flag (not the round status) is what makes a
//! repeated dispatch safe: once it is `1` the pre-flight validation rejects the
//! call with [`DispatchErrorKind::RoundNotDispatchable`] before any HTTP or
//! write, so a second prompt can never be sent.
//!
//! # Errors and redaction
//!
//! A storage or HTTP failure is a typed [`DispatchError`] and is never a false
//! success. A delivery failure leaves the round `sent`/`attempted` with the
//! persisted outbound id (the reference "delivery outcome undefined" state) and
//! is not retried automatically; a subsequent dispatch is rejected before the
//! request. [`fmt::Display`] and [`fmt::Debug`] render only fixed,
//! developer-authored labels and never the task text, prompt, project/task/
//! session/message ids, workspace, HTTP body, credentials, SQL or paths. The
//! internal diagnostic cause is reachable only through [`std::error::Error::source`].
//!
//! # Concurrency precondition and remaining race
//!
//! The caller **must hold the project [`crate::WorkerLock`] for the whole
//! dispatch**, acquired from the same [`RustStateLayout`] passed here, so the
//! lock and the dispatched state refer to the same verified project namespace.
//! The lock serializes workers of one project, so two dispatches of the same
//! project cannot run concurrently and the atomic storage lifecycle writes
//! cannot interleave with a second dispatch.
//!
//! The lock is deliberately **not** held by every lifecycle mutator: the
//! cooperative-close writers
//! [`bridge_storage::StorageConnection::request_task_close`] and
//! [`bridge_storage::StorageConnection::complete_requested_close`] (the future
//! MCP close-request path) do not acquire it, and neither does any other
//! external writer of the same SQLite state. The task status and
//! `close_requested_at` are therefore checked twice — once before any HTTP and
//! again after session resolution, immediately before `prepare`/`mark_sent` —
//! so a close request that lands while a session is listed, created or bound is
//! observed and fails closed with [`DispatchErrorKind::TaskNotDispatchable`]
//! before the prompt request. A close request that lands in the remaining window
//! between that second check and the `mark_round_sent` write is **not** detected
//! here; refusing to send it is the cooperative-close lifecycle (7.12), which
//! this step deliberately does not implement. This module does not acquire the
//! lock itself.

use std::error::Error;
use std::fmt;
use std::path::Path;

use bridge_domain::{ProfileSnapshot, RoundKind, RoundStatus, TaskId, TaskStatus};
use bridge_opencode::OpenCodeClient;
use bridge_storage::{
    RoundRef, RoundRow, RoundUpdateError, RustStateLayout, StorageConnection, Task,
};

use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::findings::validate_revision_findings;
use crate::prompt::{initial_prompt_with_profile, revision_prompt_with_profile};
use crate::session::{ResolvedSession, SessionResolutionSource, resolve_round_session};
use bridge_storage::profiles::ProfileReadError;

/// Generates the reference outbound message id `"msg_" + uuid.uuid4().hex`.
///
/// The result is exactly the `msg_` prefix followed by 32 lowercase hex digits
/// (a v4 UUID rendered without dashes), matching Python
/// `worker.new_message_id`.
#[must_use]
pub fn new_message_id() -> String {
    format!("msg_{}", uuid::Uuid::new_v4().simple())
}

/// Broad, typed category of a [`DispatchError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DispatchErrorKind {
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
    /// An initial dispatch was given a round that is not an `implement` round.
    RevisionNotSupported,
    /// A revision dispatch was given a round that is not a `revise` round.
    ImplementNotSupported,
    /// The round is not `pending`, or a delivery was already attempted.
    RoundNotDispatchable,
    /// The task is not in the status the round kind requires, or has a pending
    /// close request.
    TaskNotDispatchable,
    /// The dedicated session could not be resolved or bound.
    Session,
    /// A storage lifecycle write failed.
    Storage,
    /// The prompt delivery request failed; the delivery outcome is undefined.
    Delivery,
    /// Persisted revision findings are malformed or outside the task scope.
    StructuredFindingsInvariant,
    /// A present task profile has no persisted snapshot.
    ProfileSnapshotMissing,
    /// The persisted profile identity/hash/model is malformed or inconsistent.
    ProfileSnapshotCorrupt,
}

impl DispatchErrorKind {
    /// Returns a short, non-sensitive label for this category.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "initial dispatch input is invalid",
            Self::UnknownTask => "initial dispatch task does not exist",
            Self::TaskMismatch => "initial dispatch task does not match the round",
            Self::WorkspaceMismatch => "initial dispatch workspace does not match the task",
            Self::StaleRound => "initial dispatch round is missing or not current",
            Self::StateOwnership => "rust state ownership could not be verified",
            Self::RevisionNotSupported => "initial dispatch only supports implement rounds",
            Self::ImplementNotSupported => "revision dispatch only supports revise rounds",
            Self::RoundNotDispatchable => "round is not a dispatchable pending, unattempted round",
            Self::TaskNotDispatchable => {
                "task is not in the dispatchable status without a pending close request"
            }
            Self::Session => "opencode session could not be resolved for the round",
            Self::Storage => "round lifecycle could not be persisted",
            Self::Delivery => "opencode prompt delivery failed",
            Self::StructuredFindingsInvariant => "structured_findings_invariant",
            Self::ProfileSnapshotMissing => "profile_snapshot_missing",
            Self::ProfileSnapshotCorrupt => "profile_snapshot_corrupt",
        }
    }
}

impl fmt::Display for DispatchErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed, safe error raised while dispatching an initial round.
///
/// The [`Display`](fmt::Display) representation is a fixed, developer-authored
/// message and [`Debug`](fmt::Debug) shows only the category. Neither ever
/// contains task text, prompt text, project/task/session/message ids, the
/// workspace, an HTTP body, credentials, SQL or paths. The internal diagnostic
/// cause, when present, is reachable only through [`Error::source`].
pub struct DispatchError {
    kind: DispatchErrorKind,
    source: Option<Box<dyn Error + Send + Sync + 'static>>,
}

impl DispatchError {
    /// Creates an error of the given `kind` without a source.
    fn new(kind: DispatchErrorKind) -> Self {
        Self { kind, source: None }
    }

    /// Creates an error of the given `kind` with an internal diagnostic source.
    fn with_source(kind: DispatchErrorKind, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            source: Some(Box::new(source)),
        }
    }

    /// Returns the category of this error.
    #[must_use]
    pub const fn kind(&self) -> DispatchErrorKind {
        self.kind
    }
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.as_str())
    }
}

impl fmt::Debug for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DispatchError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Error for DispatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.source {
            Some(source) => Some(source.as_ref()),
            None => None,
        }
    }
}

/// Which round kind a dispatch targets.
///
/// This is an internal selector: it fixes the expected [`RoundKind`] and
/// [`TaskStatus`] and chooses the prompt builder, so the initial and revision
/// paths share one pipeline while remaining strictly cross-kind rejecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoundDispatchMode {
    /// An initial `implement` round of an `implementing` task.
    Initial,
    /// A revision `revise` round of a `revising` task.
    Revision,
}

impl RoundDispatchMode {
    /// The only round kind this mode accepts.
    const fn expected_kind(self) -> RoundKind {
        match self {
            Self::Initial => RoundKind::Implement,
            Self::Revision => RoundKind::Revise,
        }
    }

    /// The only task status this mode accepts.
    const fn expected_task_status(self) -> TaskStatus {
        match self {
            Self::Initial => TaskStatus::Implementing,
            Self::Revision => TaskStatus::Revising,
        }
    }

    /// The typed error raised when the round has the other kind.
    const fn kind_mismatch(self) -> DispatchErrorKind {
        match self {
            Self::Initial => DispatchErrorKind::RevisionNotSupported,
            Self::Revision => DispatchErrorKind::ImplementNotSupported,
        }
    }

    /// Renders the exact prompt for this mode.
    ///
    /// The revision findings are read only from the persisted current round
    /// (`row.findings`). Text is mandatory; the optional structured column is
    /// revalidated against the current filesystem and persisted task scope.
    /// Violations atomically fail this unsent round before any prompt is sent.
    fn render_prompt(
        self,
        storage: &mut StorageConnection,
        round: &RoundRef,
        task: &Task,
        row: &RoundRow,
        workspace: &Path,
        trusted_roots: &[&Path],
    ) -> Result<(String, Option<ProfileSnapshot>), DispatchError> {
        let profile = validated_profile(storage, round)?;
        let text = match self {
            Self::Initial => Ok(initial_prompt_with_profile(
                task,
                workspace,
                profile.as_ref(),
            )),
            Self::Revision => {
                let structured = match storage.get_round_structured_findings(round) {
                    Ok(value) => value,
                    Err(RoundUpdateError::InvalidPersistedState) => {
                        return fail_findings(storage, round);
                    }
                    Err(error) => {
                        return Err(DispatchError::with_source(
                            DispatchErrorKind::Storage,
                            error,
                        ));
                    }
                };
                let value = structured
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(|_| {
                        DispatchError::new(DispatchErrorKind::StructuredFindingsInvariant)
                    })?;
                let findings = match validate_revision_findings(
                    row.findings.as_deref().unwrap_or(""),
                    value.as_ref(),
                    workspace,
                    trusted_roots,
                    &task.allowed_paths,
                ) {
                    Ok(value) => value,
                    Err(_) => return fail_findings(storage, round),
                };
                Ok(revision_prompt_with_profile(
                    task,
                    workspace,
                    &findings.prompt_findings(),
                    row.round_number,
                    profile.as_ref(),
                ))
            }
        }?;
        Ok((text, profile))
    }

    /// The human label used by [`DispatchedRound`].
    const fn label(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Revision => "revision",
        }
    }
}

/// The persisted result of one successful initial or revision dispatch.
///
/// It carries the resolved session, the persisted outbound message id and the
/// committed round/task rows so the future observation step can continue from
/// the exact state. The [`fmt::Debug`]/[`fmt::Display`] representations redact
/// the outbound message id and the session id.
pub struct DispatchedRound {
    mode: RoundDispatchMode,
    session: ResolvedSession,
    outbound_message_id: String,
    round: RoundRow,
    task: Task,
}

impl DispatchedRound {
    /// Returns the resolved dedicated session.
    #[must_use]
    pub fn session(&self) -> &ResolvedSession {
        &self.session
    }

    /// Returns how the session was obtained.
    #[must_use]
    pub const fn session_source(&self) -> SessionResolutionSource {
        self.session.source()
    }

    /// Returns the persisted outbound message id.
    #[must_use]
    pub fn outbound_message_id(&self) -> &str {
        &self.outbound_message_id
    }

    /// Returns the persisted round after the `observing` transition.
    #[must_use]
    pub fn round(&self) -> &RoundRow {
        &self.round
    }

    /// Returns the persisted task after the dispatch.
    #[must_use]
    pub fn task(&self) -> &Task {
        &self.task
    }
}

impl fmt::Debug for DispatchedRound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DispatchedRound")
            .field("session", &self.session)
            .field("outbound_message_id", &"[redacted]")
            .field("round_number", &self.round.round_number)
            .field("round_status", &self.round.status)
            .field("task_status", &self.task.status)
            .finish()
    }
}

impl fmt::Display for DispatchedRound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} round dispatched (round {})",
            self.mode.label(),
            self.round.round_number
        )
    }
}

/// Dispatches exactly one initial prompt for the `implement` round `round`.
///
/// The Rust-owned state is identified by the explicit [`RustStateLayout`] and
/// opened through the production [`RustStateLayout::open`] ownership guard
/// before any read, HTTP request or write; the layout project must equal
/// `round.project_id`. See the module documentation for the full contract, the
/// concurrency precondition (the caller holds the project [`crate::WorkerLock`]
/// for the whole call) and the persist-before-send ordering.
///
/// # Errors
///
/// Returns a typed [`DispatchError`] with [`DispatchErrorKind`] `InvalidInput`
/// before any read, `StateOwnership` when the layout guard rejects the state,
/// `TaskMismatch`/`UnknownTask`/`WorkspaceMismatch`/`StaleRound` when the
/// task/round/project/workspace/current-round validation fails,
/// `RevisionNotSupported` for a non-`implement` round, `TaskNotDispatchable`
/// for a task that is not `implementing` or has a pending close request,
/// `RoundNotDispatchable` for a round that is not a pending, unattempted round,
/// `Session` when session resolution or binding fails, `Storage` when a
/// lifecycle write fails, or `Delivery` when the single prompt request fails.
/// The task and current round are validated both before any HTTP and again after
/// session resolution, so a close request that lands during resolution fails
/// closed before the prompt. No error message contains task text, prompt text,
/// ids, the workspace, an HTTP body, SQL or a path.
pub fn dispatch_initial_round(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    round: RoundRef,
) -> Result<DispatchedRound, DispatchError> {
    dispatch_round(client, layout, round, RoundDispatchMode::Initial)
}

/// Dispatches exactly one revision prompt for the `revise` round `round`.
///
/// This is the task-7.5 counterpart of [`dispatch_initial_round`] and shares its
/// pipeline, concurrency precondition (the caller holds the project
/// [`crate::WorkerLock`] for the whole call) and persist-before-send ordering.
/// The round must be a `revise` round of a `revising` task; the prompt is the
/// exact reference `prompts.REVISION_TEMPLATE` rendered with the persisted
/// current round's mandatory `findings`, optional validated structured block,
/// original task text and round number, so each session is self-contained.
/// The 7.3 resolver gives the round its own dedicated session: the previous
/// round's session and `tasks.session_id` are never reused, and no `parentID`
/// or fork is sent.
///
/// # Errors
///
/// Returns the same typed [`DispatchError`] categories as
/// [`dispatch_initial_round`], with `ImplementNotSupported` for a round that is
/// not a `revise` round and `TaskNotDispatchable` for a task that is not
/// `revising` or has a pending close request. Invalid persisted findings produce
/// `StructuredFindingsInvariant` and atomically fail the unsent round/task;
/// storage failures produce `Storage`. The default entry point trusts no
/// external roots; use [`dispatch_revision_round_with_trusted_roots`] when
/// qualified external paths are configured.
pub fn dispatch_revision_round(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    round: RoundRef,
) -> Result<DispatchedRound, DispatchError> {
    dispatch_revision_round_with_trusted_roots(client, layout, round, &[])
}

/// Revision dispatch with explicit configured external roots. Qualified paths
/// require both trusted-root authorization and the persisted task's own scope.
/// Caller holds the worker lock for the entire operation.
pub fn dispatch_revision_round_with_trusted_roots(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    round: RoundRef,
    trusted_roots: &[&Path],
) -> Result<DispatchedRound, DispatchError> {
    dispatch_round_with_roots(
        client,
        layout,
        round,
        RoundDispatchMode::Revision,
        trusted_roots,
    )
}

/// The shared initial/revision dispatch pipeline.
///
/// `mode` fixes the expected round kind, the expected task status and the prompt
/// builder. Every other step (ownership guard, project/task/round/current/
/// workspace validation, session resolution, re-validation, prepared-id reuse,
/// persist-before-send and the successful `observing` transition) is identical,
/// so the two public entry points cannot diverge.
fn dispatch_round(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    round: RoundRef,
    mode: RoundDispatchMode,
) -> Result<DispatchedRound, DispatchError> {
    dispatch_round_with_roots(client, layout, round, mode, &[])
}

fn dispatch_round_with_roots(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    round: RoundRef,
    mode: RoundDispatchMode,
    trusted_roots: &[&Path],
) -> Result<DispatchedRound, DispatchError> {
    if round.round_number == 0 {
        return Err(DispatchError::new(DispatchErrorKind::InvalidInput));
    }
    if layout.project_id() != &round.project_id {
        return Err(DispatchError::new(DispatchErrorKind::TaskMismatch));
    }

    let mut preflight = open_state(layout)?;
    let (task, row) = validate_task_and_round(client, layout, &preflight, &round, mode)?;
    mode.render_prompt(
        &mut preflight,
        &round,
        &task,
        &row,
        client.workspace(),
        trusted_roots,
    )?;
    drop(preflight);

    let session = resolve_round_session(client, layout, round.clone())
        .map_err(|error| DispatchError::with_source(DispatchErrorKind::Session, error))?;

    // Re-validate after session resolution: resolving a session performs HTTP
    // and a binding write, so a concurrent close request or round mutation must
    // not slip past the pre-flight check and reach prepare/mark/send.
    let mut storage = open_state(layout)?;
    let (task, row) = validate_task_and_round(client, layout, &storage, &round, mode)?;

    let (text, profile) = mode.render_prompt(
        &mut storage,
        &round,
        &task,
        &row,
        client.workspace(),
        trusted_roots,
    )?;
    let pinned_model = profile
        .as_ref()
        .map(|profile| {
            profile
                .model
                .as_deref()
                .map(bridge_config::OpenCodeModel::parse)
                .transpose()
        })
        .transpose()
        .map_err(|_| DispatchError::new(DispatchErrorKind::ProfileSnapshotCorrupt))?;
    let outbound = match row
        .outbound_message_id
        .as_deref()
        .filter(|id| !id.is_empty())
    {
        Some(id) => id.to_owned(),
        None => {
            let id = new_message_id();
            storage
                .prepare_round(round.clone(), id.clone())
                .map_err(|error| DispatchError::with_source(DispatchErrorKind::Storage, error))?;
            id
        }
    };

    storage
        .mark_round_sent(round.clone())
        .map_err(|error| DispatchError::with_source(DispatchErrorKind::Storage, error))?;

    let delivery = if let Some(model) = pinned_model {
        client.send_prompt_async_with_model(session.id(), &outbound, &text, model.as_ref())
    } else {
        client.send_prompt_async(session.id(), &outbound, &text)
    };
    delivery.map_err(|error| DispatchError::with_source(DispatchErrorKind::Delivery, error))?;

    let outcome = storage
        .mark_round_observing(round.clone())
        .map_err(|error| DispatchError::with_source(DispatchErrorKind::Storage, error))?;

    Ok(DispatchedRound {
        mode,
        session,
        outbound_message_id: outbound,
        round: outcome.round,
        task: outcome.task,
    })
}

fn validated_profile(
    storage: &mut StorageConnection,
    round: &RoundRef,
) -> Result<Option<ProfileSnapshot>, DispatchError> {
    match storage.get_task_profile(round.task_id, &round.project_id) {
        Ok(profile) => Ok(profile),
        Err(error @ (ProfileReadError::MissingSnapshot | ProfileReadError::CorruptSnapshot)) => {
            storage
                .fail_profile_snapshot(round.clone(), error)
                .map_err(|error| DispatchError::with_source(DispatchErrorKind::Storage, error))?;
            Err(DispatchError::new(
                if error == ProfileReadError::MissingSnapshot {
                    DispatchErrorKind::ProfileSnapshotMissing
                } else {
                    DispatchErrorKind::ProfileSnapshotCorrupt
                },
            ))
        }
        Err(error) => Err(DispatchError::with_source(
            DispatchErrorKind::Storage,
            error,
        )),
    }
}

/// Validates the task/round/project/workspace/current-round preconditions
/// before any HTTP request or write.
fn fail_findings<T>(storage: &mut StorageConnection, round: &RoundRef) -> Result<T, DispatchError> {
    storage
        .fail_revision_findings(round.clone())
        .map_err(|error| DispatchError::with_source(DispatchErrorKind::Storage, error))?;
    Err(DispatchError::new(
        DispatchErrorKind::StructuredFindingsInvariant,
    ))
}

/// Validates that the persisted task and current round are suitable for the
/// requested dispatch mode and returns the mapped task and round rows.
///
/// This is run both before any HTTP (on the pre-flight state) and again after
/// session resolution (on the possibly-mutated state) so that a task moved out
/// of the expected status or given a pending close request during resolution
/// cannot reach `prepare`/`mark_sent`/`send`. The task must be in the mode's
/// expected status (`implementing`/`revising`) with no `close_requested_at`,
/// the round must be of the mode's expected kind (`implement`/`revise`) and be
/// `pending` and unattempted, and the project/workspace/current-round checks
/// must all agree. Every failure is a typed [`DispatchError`] raised before any
/// further side effect.
fn validate_task_and_round(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    storage: &StorageConnection,
    round: &RoundRef,
    mode: RoundDispatchMode,
) -> Result<(Task, RoundRow), DispatchError> {
    let task = load_task(storage, round.task_id)?;
    if task.project_id != round.project_id {
        return Err(DispatchError::new(DispatchErrorKind::TaskMismatch));
    }

    let row = read_round(storage, round)?;
    if row.project_id != round.project_id || row.task_id != round.task_id {
        return Err(DispatchError::new(DispatchErrorKind::TaskMismatch));
    }
    if current_round_number(storage, round.task_id)? != round.round_number {
        return Err(DispatchError::new(DispatchErrorKind::StaleRound));
    }
    if crate::execution::execution_root(storage, layout, &task, true)
        .ok()
        .as_deref()
        != Some(client.workspace())
    {
        return Err(DispatchError::new(DispatchErrorKind::WorkspaceMismatch));
    }
    if row.kind != mode.expected_kind() {
        return Err(DispatchError::new(mode.kind_mismatch()));
    }
    if task.status != mode.expected_task_status() || task.close_requested_at.is_some() {
        return Err(DispatchError::new(DispatchErrorKind::TaskNotDispatchable));
    }
    if row.status != RoundStatus::Pending || row.attempted {
        return Err(DispatchError::new(DispatchErrorKind::RoundNotDispatchable));
    }
    Ok((task, row))
}

/// Opens the layout through the production ownership guard.
fn open_state(layout: &RustStateLayout) -> Result<StorageConnection, DispatchError> {
    layout
        .open()
        .map_err(|error| DispatchError::with_source(DispatchErrorKind::StateOwnership, error))
}

/// Loads a task by id, failing closed when it is absent.
fn load_task(storage: &StorageConnection, task_id: TaskId) -> Result<Task, DispatchError> {
    storage
        .get_task(task_id)
        .map_err(|error| DispatchError::with_source(DispatchErrorKind::Storage, error))?
        .ok_or_else(|| DispatchError::new(DispatchErrorKind::UnknownTask))
}

/// Reads the persisted round through the production row mapping.
fn read_round(storage: &StorageConnection, round: &RoundRef) -> Result<RoundRow, DispatchError> {
    let found = storage
        .connection()
        .query_row(
            "SELECT * FROM rounds WHERE task_id = ?1 AND round_number = ?2",
            params![round.task_id.to_string(), i64::from(round.round_number)],
            |row| Ok(RoundRow::from_row(row)),
        )
        .optional()
        .map_err(|error| DispatchError::with_source(DispatchErrorKind::Storage, error))?;
    match found {
        Some(Ok(row)) => Ok(row),
        Some(Err(error)) => Err(DispatchError::with_source(
            DispatchErrorKind::Storage,
            error,
        )),
        None => Err(DispatchError::new(DispatchErrorKind::StaleRound)),
    }
}

/// Returns the highest round number of `task_id`.
fn current_round_number(
    storage: &StorageConnection,
    task_id: TaskId,
) -> Result<u32, DispatchError> {
    let max: Option<i64> = storage
        .connection()
        .query_row(
            "SELECT MAX(round_number) FROM rounds WHERE task_id = ?1",
            params![task_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| DispatchError::with_source(DispatchErrorKind::Storage, error))?;
    max.and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value >= 1)
        .ok_or_else(|| DispatchError::new(DispatchErrorKind::StaleRound))
}

#[cfg(test)]
mod tests {
    use super::{DispatchError, DispatchErrorKind, new_message_id};

    #[test]
    fn new_message_id_matches_the_reference_shape() {
        let id = new_message_id();
        assert!(id.starts_with("msg_"), "prefix: {id}");
        let hex = &id["msg_".len()..];
        assert_eq!(hex.len(), 32, "32 hex digits");
        assert!(
            hex.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "lowercase hex: {hex}"
        );
    }

    #[test]
    fn error_rendering_is_redacted() {
        let error = DispatchError::new(DispatchErrorKind::RevisionNotSupported);
        let rendered = format!("{error} {error:?}");
        assert_eq!(
            rendered,
            "initial dispatch only supports implement rounds \
             DispatchError { kind: RevisionNotSupported, .. }"
        );

        let error = DispatchError::new(DispatchErrorKind::ImplementNotSupported);
        let rendered = format!("{error} {error:?}");
        assert_eq!(
            rendered,
            "revision dispatch only supports revise rounds \
             DispatchError { kind: ImplementNotSupported, .. }"
        );
    }
}
