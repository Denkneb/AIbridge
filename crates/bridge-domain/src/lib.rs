//! Minimal domain crate for the agent-bridge Rust implementation.
//!
//! This crate provides the shared, typed error model used by future
//! components together with the core domain identifiers ([`ProjectId`],
//! [`TaskId`]), the frozen [`TaskStatus`] vocabulary and the frozen
//! round/verification vocabulary ([`RoundKind`], [`RoundStatus`],
//! [`VerifierState`], [`VerificationStatus`]) with the [`Round`] and
//! [`Verification`] data models. It deliberately keeps a strict separation
//! between the *safe* message that may be shown to a user and the
//! *diagnostic* source that may contain sensitive internal details.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Returns the package name as a trivial smoke-check helper.
#[must_use]
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

/// Broad, stable category of a [`DomainError`].
///
/// The category is intentionally coarse: it lets callers branch on the
/// kind of failure without exposing or depending on internal details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The caller supplied input that is not acceptable.
    InvalidInput,
    /// A requested resource does not exist.
    NotFound,
    /// The operation conflicts with the current state.
    Conflict,
    /// The operation is not permitted.
    PermissionDenied,
    /// An unexpected internal failure occurred.
    Internal,
}

impl ErrorKind {
    /// Returns a short, non-sensitive label for this category.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::PermissionDenied => "permission_denied",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed domain error with a safe user-facing message.
///
/// The [`Display`](fmt::Display) representation exposes only the static,
/// developer-authored message. Any sensitive or internal detail must be
/// attached as a *source* via [`DomainError::with_source`] and is available
/// only through [`Error::source`]; it is never part of the safe output.
pub struct DomainError {
    kind: ErrorKind,
    message: &'static str,
    source: Option<Box<dyn Error + Send + Sync + 'static>>,
}

impl DomainError {
    /// Creates an error of the given `kind` with a safe `message`.
    ///
    /// `message` must be a static, developer-authored string that is safe
    /// to show to a user.
    #[must_use]
    pub const fn new(kind: ErrorKind, message: &'static str) -> Self {
        Self {
            kind,
            message,
            source: None,
        }
    }

    /// Attaches an internal diagnostic `source` to this error.
    ///
    /// The source is never rendered by [`Display`](fmt::Display) or
    /// [`Debug`](fmt::Debug); it is reachable only through [`Error::source`].
    #[must_use]
    pub fn with_source(mut self, source: impl Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// Creates an [`ErrorKind::InvalidInput`] error.
    #[must_use]
    pub const fn invalid_input(message: &'static str) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    /// Creates an [`ErrorKind::NotFound`] error.
    #[must_use]
    pub const fn not_found(message: &'static str) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    /// Creates an [`ErrorKind::Conflict`] error.
    #[must_use]
    pub const fn conflict(message: &'static str) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    /// Creates an [`ErrorKind::PermissionDenied`] error.
    #[must_use]
    pub const fn permission_denied(message: &'static str) -> Self {
        Self::new(ErrorKind::PermissionDenied, message)
    }

    /// Creates an [`ErrorKind::Internal`] error.
    #[must_use]
    pub const fn internal(message: &'static str) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// Returns the category of this error.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the safe, user-facing message.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl fmt::Debug for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DomainError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .finish_non_exhaustive()
    }
}

impl Error for DomainError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.source {
            Some(source) => Some(source.as_ref()),
            None => None,
        }
    }
}

/// Convenience alias for results carrying a [`DomainError`].
pub type Result<T> = std::result::Result<T, DomainError>;

/// Stable identifier of a configured project.
///
/// `ProjectId` is a distinct newtype, so it can never be confused with a
/// [`TaskId`] at the type level.
///
/// # Parsing contract
///
/// Parsing accepts any non-empty string verbatim. It intentionally does
/// **not** enforce the project-id pattern/length rule
/// `^[a-z0-9][a-z0-9_-]{0,63}$` or any other configuration constraint:
/// that validation belongs to the configuration layer (task 2.2) and must
/// not be duplicated here. Only the universally required invariant that an
/// identifier is non-empty is checked.
///
/// The string representation ([`Display`](fmt::Display) and serde) is the
/// identifier itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProjectId(String);

impl ProjectId {
    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ProjectId {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Err(DomainError::invalid_input("project id must not be empty"));
        }
        Ok(Self(s.to_owned()))
    }
}

impl From<ProjectId> for String {
    fn from(id: ProjectId) -> Self {
        id.0
    }
}

impl TryFrom<String> for ProjectId {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

/// Identifier of a task, represented as a UUID.
///
/// `TaskId` is a distinct newtype, so it can never be confused with a
/// [`ProjectId`] at the type level. It parses canonical UUID text and always
/// renders in the lowercase hyphenated form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TaskId(Uuid);

impl TaskId {
    /// Returns the underlying UUID.
    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for TaskId {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self> {
        Uuid::parse_str(s).map(Self).map_err(|source| {
            DomainError::invalid_input("task id is not a valid UUID").with_source(source)
        })
    }
}

impl From<TaskId> for String {
    fn from(id: TaskId) -> Self {
        id.0.to_string()
    }
}

impl TryFrom<String> for TaskId {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

/// Frozen lifecycle status of a task.
///
/// The variant set and string spellings are fixed by the external contract
/// (`domain.task_statuses` in the contract manifest). Every status is either
/// [`active`](TaskStatus::is_active) or
/// [`terminal`](TaskStatus::is_terminal), never both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[non_exhaustive]
pub enum TaskStatus {
    /// The worker is implementing the task.
    Implementing,
    /// The implementation is ready and waiting for review.
    AwaitingReview,
    /// A revision round is in progress.
    Revising,
    /// The task needs a user decision.
    NeedsUser,
    /// The task failed and cannot be recovered automatically.
    Failed,
    /// The prompt delivery outcome is unknown.
    DeliveryUnknown,
    /// The task was accepted.
    Accepted,
    /// The task was closed.
    Closed,
}

/// The single, table-driven source of truth for allowed [`TaskStatus`]
/// transitions.
///
/// Each entry is an allowed `(from, to)` pair. Every decision about whether a
/// transition is permitted is derived from this table, so no caller-side
/// `match` or scattered rule can diverge from the frozen contract
/// (`domain.task_transitions` in the contract manifest). The `any_non_terminal
/// -> closed` rule is expanded here into one explicit pair per non-terminal
/// status.
pub const TASK_TRANSITIONS: [(TaskStatus, TaskStatus); 26] = [
    (TaskStatus::Accepted, TaskStatus::Accepted),
    (TaskStatus::Closed, TaskStatus::Closed),
    (TaskStatus::Implementing, TaskStatus::Closed),
    (TaskStatus::AwaitingReview, TaskStatus::Closed),
    (TaskStatus::Revising, TaskStatus::Closed),
    (TaskStatus::NeedsUser, TaskStatus::Closed),
    (TaskStatus::Failed, TaskStatus::Closed),
    (TaskStatus::DeliveryUnknown, TaskStatus::Closed),
    (TaskStatus::AwaitingReview, TaskStatus::Accepted),
    (TaskStatus::AwaitingReview, TaskStatus::NeedsUser),
    (TaskStatus::AwaitingReview, TaskStatus::Revising),
    (TaskStatus::DeliveryUnknown, TaskStatus::Implementing),
    (TaskStatus::DeliveryUnknown, TaskStatus::Revising),
    (TaskStatus::Failed, TaskStatus::Implementing),
    (TaskStatus::Failed, TaskStatus::Revising),
    (TaskStatus::Implementing, TaskStatus::AwaitingReview),
    (TaskStatus::Implementing, TaskStatus::DeliveryUnknown),
    (TaskStatus::Implementing, TaskStatus::Failed),
    (TaskStatus::Implementing, TaskStatus::NeedsUser),
    (TaskStatus::NeedsUser, TaskStatus::Accepted),
    (TaskStatus::NeedsUser, TaskStatus::Implementing),
    (TaskStatus::NeedsUser, TaskStatus::Revising),
    (TaskStatus::Revising, TaskStatus::AwaitingReview),
    (TaskStatus::Revising, TaskStatus::DeliveryUnknown),
    (TaskStatus::Revising, TaskStatus::Failed),
    (TaskStatus::Revising, TaskStatus::NeedsUser),
];

impl TaskStatus {
    /// All statuses in the frozen vocabulary order.
    pub const ALL: [TaskStatus; 8] = [
        Self::Implementing,
        Self::AwaitingReview,
        Self::Revising,
        Self::NeedsUser,
        Self::Failed,
        Self::DeliveryUnknown,
        Self::Accepted,
        Self::Closed,
    ];

    /// Returns the exact `snake_case` contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Implementing => "implementing",
            Self::AwaitingReview => "awaiting_review",
            Self::Revising => "revising",
            Self::NeedsUser => "needs_user",
            Self::Failed => "failed",
            Self::DeliveryUnknown => "delivery_unknown",
            Self::Accepted => "accepted",
            Self::Closed => "closed",
        }
    }

    /// Returns `true` for statuses that keep a task active.
    ///
    /// Active statuses are `implementing`, `awaiting_review`, `revising`,
    /// `needs_user`, `failed` and `delivery_unknown`.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Implementing
                | Self::AwaitingReview
                | Self::Revising
                | Self::NeedsUser
                | Self::Failed
                | Self::DeliveryUnknown
        )
    }

    /// Returns `true` for statuses that end a task.
    ///
    /// Terminal statuses are `accepted` and `closed`.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Accepted | Self::Closed)
    }

    /// Returns `true` when moving from `self` to `next` is an allowed
    /// transition.
    ///
    /// The answer is derived solely from [`TASK_TRANSITIONS`] and requires no
    /// storage access.
    #[must_use]
    pub fn can_transition_to(self, next: TaskStatus) -> bool {
        TASK_TRANSITIONS
            .iter()
            .any(|(from, to)| *from == self && *to == next)
    }

    /// Requires that moving from `self` to `next` is an allowed transition.
    ///
    /// # Errors
    ///
    /// Returns an [`ErrorKind::Conflict`] [`DomainError`] when the transition
    /// is not listed in [`TASK_TRANSITIONS`]. The error carries only a static,
    /// non-sensitive message and never leaks the caller's inputs.
    pub fn require_transition(self, next: TaskStatus) -> Result<()> {
        if self.can_transition_to(next) {
            Ok(())
        } else {
            Err(DomainError::conflict(
                "task status transition is not allowed",
            ))
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "implementing" => Ok(Self::Implementing),
            "awaiting_review" => Ok(Self::AwaitingReview),
            "revising" => Ok(Self::Revising),
            "needs_user" => Ok(Self::NeedsUser),
            "failed" => Ok(Self::Failed),
            "delivery_unknown" => Ok(Self::DeliveryUnknown),
            "accepted" => Ok(Self::Accepted),
            "closed" => Ok(Self::Closed),
            _ => Err(DomainError::invalid_input("unknown task status")),
        }
    }
}

impl From<TaskStatus> for String {
    fn from(status: TaskStatus) -> Self {
        status.as_str().to_owned()
    }
}

impl TryFrom<String> for TaskStatus {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

/// Frozen kind of an implementation round.
///
/// The variant set and string spellings are fixed by the external contract:
/// `kind` is `implement` or `revise` in the persisted round row and in the
/// MCP round view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[non_exhaustive]
pub enum RoundKind {
    /// The initial implementation round created by `submit_task`.
    Implement,
    /// A revision round created by `request_changes`.
    Revise,
}

impl RoundKind {
    /// All kinds in the frozen vocabulary order.
    pub const ALL: [RoundKind; 2] = [Self::Implement, Self::Revise];

    /// Returns the exact `snake_case` contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Implement => "implement",
            Self::Revise => "revise",
        }
    }
}

impl fmt::Display for RoundKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RoundKind {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "implement" => Ok(Self::Implement),
            "revise" => Ok(Self::Revise),
            _ => Err(DomainError::invalid_input("unknown round kind")),
        }
    }
}

impl From<RoundKind> for String {
    fn from(kind: RoundKind) -> Self {
        kind.as_str().to_owned()
    }
}

impl TryFrom<String> for RoundKind {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

/// Frozen lifecycle status of a round.
///
/// The variant set and string spellings are fixed by the external contract
/// (`domain.round_statuses`). Every status is either
/// [`open`](RoundStatus::is_open) or [`closed`](RoundStatus::is_closed), never
/// both; the open set is the frozen `domain.round_open_statuses` list.
///
/// Round *transitions* are intentionally not modeled here: they are separate
/// future logic (see `domain.round_transitions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[non_exhaustive]
pub enum RoundStatus {
    /// The round finished and its outcome is published.
    Complete,
    /// The prompt delivery outcome is unknown.
    DeliveryUnknown,
    /// The round failed and cannot be recovered automatically.
    Failed,
    /// The round is blocked on a user decision.
    NeedsUser,
    /// The worker is observing the round.
    Observing,
    /// The prompt is about to be delivered.
    Pending,
    /// The prompt was delivered.
    Sent,
}

impl RoundStatus {
    /// All statuses in the frozen vocabulary order.
    pub const ALL: [RoundStatus; 7] = [
        Self::Complete,
        Self::DeliveryUnknown,
        Self::Failed,
        Self::NeedsUser,
        Self::Observing,
        Self::Pending,
        Self::Sent,
    ];

    /// The frozen open-status set (`domain.round_open_statuses`).
    pub const OPEN: [RoundStatus; 5] = [
        Self::DeliveryUnknown,
        Self::NeedsUser,
        Self::Observing,
        Self::Pending,
        Self::Sent,
    ];

    /// Returns the exact `snake_case` contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::DeliveryUnknown => "delivery_unknown",
            Self::Failed => "failed",
            Self::NeedsUser => "needs_user",
            Self::Observing => "observing",
            Self::Pending => "pending",
            Self::Sent => "sent",
        }
    }

    /// Returns `true` for the frozen open statuses.
    ///
    /// Open statuses are `delivery_unknown`, `needs_user`, `observing`,
    /// `pending` and `sent`.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(
            self,
            Self::DeliveryUnknown | Self::NeedsUser | Self::Observing | Self::Pending | Self::Sent
        )
    }

    /// Returns `true` for statuses that end a round.
    ///
    /// Closed statuses are `complete` and `failed`.
    #[must_use]
    pub const fn is_closed(self) -> bool {
        matches!(self, Self::Complete | Self::Failed)
    }
}

impl fmt::Display for RoundStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RoundStatus {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "complete" => Ok(Self::Complete),
            "delivery_unknown" => Ok(Self::DeliveryUnknown),
            "failed" => Ok(Self::Failed),
            "needs_user" => Ok(Self::NeedsUser),
            "observing" => Ok(Self::Observing),
            "pending" => Ok(Self::Pending),
            "sent" => Ok(Self::Sent),
            _ => Err(DomainError::invalid_input("unknown round status")),
        }
    }
}

impl From<RoundStatus> for String {
    fn from(status: RoundStatus) -> Self {
        status.as_str().to_owned()
    }
}

impl TryFrom<String> for RoundStatus {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

/// Persisted verifier lifecycle marker (`rounds.verifier_state`).
///
/// The storage invariant fixes the vocabulary to `running` and `done`; a
/// `done` result is never overwritten and is reused by recovery. The absence
/// of a verifier run is represented by [`Option::None`], not by a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[non_exhaustive]
pub enum VerifierState {
    /// A verifier run is in progress and may be re-run after a crash.
    Running,
    /// A verifier run completed and its result must be reused.
    Done,
}

impl VerifierState {
    /// All states in the frozen vocabulary order.
    pub const ALL: [VerifierState; 2] = [Self::Running, Self::Done];

    /// Returns the exact `snake_case` contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
        }
    }
}

impl fmt::Display for VerifierState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for VerifierState {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            _ => Err(DomainError::invalid_input("unknown verifier state")),
        }
    }
}

impl From<VerifierState> for String {
    fn from(state: VerifierState) -> Self {
        state.as_str().to_owned()
    }
}

impl TryFrom<String> for VerifierState {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

/// Outcome status of a completed verifier run (`verification.status`).
///
/// The variant set and string spellings are fixed by the reference
/// implementation (`verifier.py`, the `_STATUSES_*` constants and
/// `run_round_verification`): `passed`, `failed`, `timed_out`, `unsafe` and
/// `error`. A run that has not finished is represented by
/// [`VerifierState::Running`], not by a verification status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[non_exhaustive]
pub enum VerificationStatus {
    /// Every command exited successfully.
    Passed,
    /// At least one command exited non-zero.
    Failed,
    /// A command exceeded its timeout and was terminated.
    TimedOut,
    /// The agreed commands could not be run safely (policy rejection).
    Unsafe,
    /// The verifier itself failed before it could produce a verdict.
    Error,
}

impl VerificationStatus {
    /// All statuses in the frozen vocabulary order.
    pub const ALL: [VerificationStatus; 5] = [
        Self::Passed,
        Self::Failed,
        Self::TimedOut,
        Self::Unsafe,
        Self::Error,
    ];

    /// Returns the exact `snake_case` contract spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Unsafe => "unsafe",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for VerificationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for VerificationStatus {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "passed" => Ok(Self::Passed),
            "failed" => Ok(Self::Failed),
            "timed_out" => Ok(Self::TimedOut),
            "unsafe" => Ok(Self::Unsafe),
            "error" => Ok(Self::Error),
            _ => Err(DomainError::invalid_input("unknown verification status")),
        }
    }
}

impl From<VerificationStatus> for String {
    fn from(status: VerificationStatus) -> Self {
        status.as_str().to_owned()
    }
}

impl TryFrom<String> for VerificationStatus {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

/// Git fingerprint triple captured before and after a verifier run.
///
/// The field names and the `String` representation are exactly those proven by
/// `rounds.verifier_json` in `.docs/fixtures/sqlite/expected.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFingerprint {
    /// Current commit id.
    pub head: String,
    /// Digest of the staged index.
    pub index_fingerprint: String,
    /// Digest of the worktree.
    pub worktree_fingerprint: String,
}

/// One command executed by the verifier.
///
/// Only `command` is always present. The optional fields mirror the keys the
/// reference implementation adds per outcome (`timed_out` plus
/// `duration`/`exit_code` for a run command, `output_tail` for a failed
/// command, `reason` for a command that did not run); absent keys are omitted
/// on serialization, as in the contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerificationCommand {
    /// The exact command string that was (or was not) run.
    pub command: String,
    /// Present and `true` only when the command exceeded its timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out: Option<bool>,
    /// Wall-clock duration in seconds; absent when the command did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Process exit code; absent when the command did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Bounded tail of the combined output; present only for failed commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    /// Machine-readable reason a command did not run (`not_run`, `spawn_failed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Compact verifier outcome persisted as `rounds.verifier_json` and surfaced as
/// `verification` in the MCP task result.
///
/// # Proven boundary
///
/// The modeled fields cover the variants produced by the reference
/// implementation (`verifier.py:run_round_verification`) that are also proven
/// by `.docs/fixtures/sqlite/expected.json` and
/// `.docs/fixtures/mcp-cases.json`: `status`, `commands`, `index`, `reason`,
/// `log`, `before`, `after` and `side_effects`.
///
/// `commands` is absent for the early `unsafe` and `error` variants
/// (`{status, index, reason, log}` / `{status, reason, log}`), so it is
/// optional and omitted on serialization when it was absent instead of being
/// rewritten as an empty array. `index` and `reason` likewise appear only in
/// some variants. Unknown *keys* (for example `repositories`) are ignored on
/// deserialization, while unknown enum *values* are rejected safely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verification {
    /// Overall outcome status.
    pub status: VerificationStatus,
    /// Command entries in execution order; absent in the early `unsafe` and
    /// `error` variants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<VerificationCommand>>,
    /// Zero-based index of the command rejected by the safety policy; present
    /// only in the `unsafe` variant (and `-1` for a malformed command list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    /// Machine-readable reason for an `unsafe` or `error` outcome
    /// (`git_fingerprint_failed`, or a policy code such as
    /// `unsafe_shell_invocation`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Relative reference to the full verification log.
    pub log: String,
    /// Workspace fingerprint captured before the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<GitFingerprint>,
    /// Workspace fingerprint captured after the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<GitFingerprint>,
    /// Repository-qualified paths created or modified by the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effects: Option<Vec<String>>,
}

/// Minimal, typed domain view of one persisted round.
///
/// # Proven boundary
///
/// This is a deliberately minimal subset of the persisted `rounds` row and the
/// MCP round view: the stable identity/lifecycle fields plus the optional
/// verifier payload. The change-collection `result_json` payload and the
/// remaining SQLite-only columns (timestamps, message ids, hashes, `attempted`,
/// `project_id`) are storage concerns owned by stream 3 and are not modeled
/// here. Unknown keys are ignored on deserialization.
///
/// Nullable scalar fields serialize as `null`, matching the fixture rows; the
/// nested [`Verification`] object omits absent keys, matching the reference
/// implementation. `verification` corresponds to the persisted `verifier_json`
/// / MCP `verification` object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Round {
    /// Owning task.
    pub task_id: TaskId,
    /// One-based round number within the task.
    pub round_number: u32,
    /// Whether this is the initial or a revision round.
    pub kind: RoundKind,
    /// Current lifecycle status.
    pub status: RoundStatus,
    /// Idempotency key that created the round.
    pub request_id: String,
    /// Bound OpenCode session, when resolved.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Machine-readable failure code, when the round failed.
    #[serde(default)]
    pub error_code: Option<String>,
    /// Persisted verifier lifecycle marker, when a verifier run exists.
    #[serde(default)]
    pub verifier_state: Option<VerifierState>,
    /// Persisted verifier outcome, when the verifier finished.
    #[serde(default)]
    pub verification: Option<Verification>,
}

#[cfg(test)]
mod tests {
    use super::{
        DomainError, ErrorKind, GitFingerprint, ProjectId, Round, RoundKind, RoundStatus,
        TASK_TRANSITIONS, TaskId, TaskStatus, Verification, VerificationCommand,
        VerificationStatus, VerifierState, crate_name,
    };
    use std::any::TypeId;
    use std::error::Error;
    use std::str::FromStr;

    const SAMPLE_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn crate_is_wired() {
        assert_eq!(crate_name(), "bridge-domain");
    }

    #[test]
    fn constructors_set_kind_and_message() {
        let cases = [
            (
                DomainError::invalid_input("bad input"),
                ErrorKind::InvalidInput,
            ),
            (DomainError::not_found("missing"), ErrorKind::NotFound),
            (DomainError::conflict("busy"), ErrorKind::Conflict),
            (
                DomainError::permission_denied("denied"),
                ErrorKind::PermissionDenied,
            ),
            (DomainError::internal("boom"), ErrorKind::Internal),
        ];

        for (error, expected) in cases {
            assert_eq!(error.kind(), expected);
            assert_eq!(error.message(), error.to_string());
        }
    }

    #[test]
    fn display_uses_safe_message() {
        let error = DomainError::invalid_input("project id is invalid");
        assert_eq!(error.to_string(), "project id is invalid");
        assert_eq!(format!("{error}"), "project id is invalid");
    }

    #[test]
    fn source_is_exposed_through_error_trait() {
        #[derive(Debug)]
        struct Diagnostic;

        impl std::fmt::Display for Diagnostic {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("internal diagnostic")
            }
        }

        impl Error for Diagnostic {}

        let error = DomainError::internal("operation failed").with_source(Diagnostic);

        let source = error.source().expect("source must be present");
        assert_eq!(source.to_string(), "internal diagnostic");
    }

    #[test]
    fn error_without_source_has_no_source() {
        let error = DomainError::not_found("missing");
        assert!(error.source().is_none());
    }

    #[test]
    fn safe_output_redacts_diagnostic_details() {
        const SECRET: &str = "token=super-secret-value";

        #[derive(Debug)]
        struct LeakyDetail;

        impl std::fmt::Display for LeakyDetail {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(SECRET)
            }
        }

        impl Error for LeakyDetail {}

        let error = DomainError::internal("internal error").with_source(LeakyDetail);

        let display = format!("{error}");
        let debug = format!("{error:?}");
        let message = error.message().to_owned();

        assert!(
            !display.contains(SECRET),
            "Display leaked a secret: {display}"
        );
        assert!(!debug.contains(SECRET), "Debug leaked a secret: {debug}");
        assert!(
            !message.contains(SECRET),
            "message leaked a secret: {message}"
        );

        let source = error.source().expect("source must be present");
        assert!(source.to_string().contains(SECRET));
    }

    #[test]
    fn usable_as_dyn_error() {
        fn render(error: &dyn Error) -> String {
            error.to_string()
        }

        let error = DomainError::conflict("state conflict");
        assert_eq!(render(&error), "state conflict");
    }

    #[test]
    fn project_and_task_ids_are_distinct_types() {
        assert_ne!(TypeId::of::<ProjectId>(), TypeId::of::<TaskId>());
    }

    #[test]
    fn project_id_accepts_non_empty_strings() {
        for raw in ["proj", "a", "UPPER", "proj-1_2"] {
            let id = ProjectId::from_str(raw).expect("non-empty project id must parse");
            assert_eq!(id.as_str(), raw);
            assert_eq!(id.to_string(), raw);
        }
    }

    #[test]
    fn project_id_rejects_empty_input() {
        let error = ProjectId::from_str("").expect_err("empty project id must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.message(), "project id must not be empty");
    }

    #[test]
    fn project_id_serde_is_a_string() {
        let id = ProjectId::from_str("proj").expect("valid project id");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"proj\"");
        let back: ProjectId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
        assert!(serde_json::from_str::<ProjectId>("\"\"").is_err());
    }

    #[test]
    fn task_id_parses_uuid_and_renders_canonical_form() {
        let id = TaskId::from_str(SAMPLE_UUID).expect("valid uuid");
        assert_eq!(id.to_string(), SAMPLE_UUID);
        assert_eq!(id.as_uuid().to_string(), SAMPLE_UUID);

        let compact = SAMPLE_UUID.replace('-', "");
        let from_compact = TaskId::from_str(&compact).expect("uuid parser accepts compact form");
        assert_eq!(from_compact, id);
    }

    #[test]
    fn task_id_rejects_invalid_uuid_without_leaking_input() {
        const SECRET: &str = "not-a-uuid-token=secret";
        let error = TaskId::from_str(SECRET).expect_err("invalid uuid must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.message(), "task id is not a valid UUID");
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
        assert!(error.source().is_some());
    }

    #[test]
    fn task_id_serde_is_a_string() {
        let id = TaskId::from_str(SAMPLE_UUID).expect("valid uuid");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{SAMPLE_UUID}\""));
        let back: TaskId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);

        let error = serde_json::from_str::<TaskId>("\"nope\"").expect_err("invalid uuid must fail");
        assert!(!error.to_string().contains("nope"));
    }

    #[test]
    fn task_status_strings_match_frozen_vocabulary() {
        let expected = [
            "implementing",
            "awaiting_review",
            "revising",
            "needs_user",
            "failed",
            "delivery_unknown",
            "accepted",
            "closed",
        ];
        let actual: Vec<&str> = TaskStatus::ALL
            .iter()
            .map(|status| status.as_str())
            .collect();
        assert_eq!(actual, expected);

        for status in TaskStatus::ALL {
            assert_eq!(status.to_string(), status.as_str());
        }
    }

    #[test]
    fn task_status_from_str_round_trips_all() {
        for status in TaskStatus::ALL {
            let parsed = TaskStatus::from_str(status.as_str()).expect("known status must parse");
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn unknown_task_status_is_rejected_safely() {
        const SECRET: &str = "bogus_status_token=secret";
        let error = TaskStatus::from_str(SECRET).expect_err("unknown status must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.message(), "unknown task status");
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[test]
    fn task_status_serde_is_a_string() {
        for status in TaskStatus::ALL {
            let json = serde_json::to_string(&status).expect("serialize");
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            let back: TaskStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, status);
        }

        let error = serde_json::from_str::<TaskStatus>("\"bogus\"").expect_err("unknown must fail");
        assert!(!error.to_string().contains("bogus"));
    }

    #[test]
    fn task_status_classification_is_complete_and_exclusive() {
        let mut expected_active = vec![
            TaskStatus::AwaitingReview,
            TaskStatus::DeliveryUnknown,
            TaskStatus::Failed,
            TaskStatus::Implementing,
            TaskStatus::NeedsUser,
            TaskStatus::Revising,
        ];
        let mut expected_terminal = vec![TaskStatus::Accepted, TaskStatus::Closed];

        let mut active = Vec::new();
        let mut terminal = Vec::new();
        for status in TaskStatus::ALL {
            assert_ne!(status.is_active(), status.is_terminal());
            if status.is_active() {
                active.push(status);
            } else {
                terminal.push(status);
            }
        }

        active.sort_by_key(|status| status.as_str());
        terminal.sort_by_key(|status| status.as_str());
        expected_active.sort_by_key(|status| status.as_str());
        expected_terminal.sort_by_key(|status| status.as_str());

        assert_eq!(active, expected_active);
        assert_eq!(terminal, expected_terminal);
    }

    #[test]
    fn task_transition_table_has_no_duplicates_and_known_pairs() {
        let mut seen = Vec::new();
        for pair in TASK_TRANSITIONS {
            assert!(
                TaskStatus::ALL.contains(&pair.0) && TaskStatus::ALL.contains(&pair.1),
                "transition table references an unknown status: {pair:?}"
            );
            assert!(
                !seen.contains(&pair),
                "duplicate transition table entry: {pair:?}"
            );
            seen.push(pair);
        }
        assert_eq!(seen.len(), TASK_TRANSITIONS.len());
    }

    #[test]
    fn can_transition_to_matches_contract_for_all_pairs() {
        const ALLOWED: [(TaskStatus, TaskStatus); 26] = [
            (TaskStatus::Accepted, TaskStatus::Accepted),
            (TaskStatus::Closed, TaskStatus::Closed),
            (TaskStatus::Implementing, TaskStatus::Closed),
            (TaskStatus::AwaitingReview, TaskStatus::Closed),
            (TaskStatus::Revising, TaskStatus::Closed),
            (TaskStatus::NeedsUser, TaskStatus::Closed),
            (TaskStatus::Failed, TaskStatus::Closed),
            (TaskStatus::DeliveryUnknown, TaskStatus::Closed),
            (TaskStatus::AwaitingReview, TaskStatus::Accepted),
            (TaskStatus::AwaitingReview, TaskStatus::NeedsUser),
            (TaskStatus::AwaitingReview, TaskStatus::Revising),
            (TaskStatus::DeliveryUnknown, TaskStatus::Implementing),
            (TaskStatus::DeliveryUnknown, TaskStatus::Revising),
            (TaskStatus::Failed, TaskStatus::Implementing),
            (TaskStatus::Failed, TaskStatus::Revising),
            (TaskStatus::Implementing, TaskStatus::AwaitingReview),
            (TaskStatus::Implementing, TaskStatus::DeliveryUnknown),
            (TaskStatus::Implementing, TaskStatus::Failed),
            (TaskStatus::Implementing, TaskStatus::NeedsUser),
            (TaskStatus::NeedsUser, TaskStatus::Accepted),
            (TaskStatus::NeedsUser, TaskStatus::Implementing),
            (TaskStatus::NeedsUser, TaskStatus::Revising),
            (TaskStatus::Revising, TaskStatus::AwaitingReview),
            (TaskStatus::Revising, TaskStatus::DeliveryUnknown),
            (TaskStatus::Revising, TaskStatus::Failed),
            (TaskStatus::Revising, TaskStatus::NeedsUser),
        ];

        for from in TaskStatus::ALL {
            for to in TaskStatus::ALL {
                let expected = ALLOWED.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "unexpected decision for {from} -> {to}"
                );
            }
        }
    }

    #[test]
    fn any_non_terminal_status_can_close() {
        for status in TaskStatus::ALL {
            if status.is_active() {
                assert!(
                    status.can_transition_to(TaskStatus::Closed),
                    "{status} must be closable"
                );
            }
        }
        assert!(!TaskStatus::Accepted.can_transition_to(TaskStatus::Closed));
        assert!(TaskStatus::Closed.can_transition_to(TaskStatus::Closed));
    }

    #[test]
    fn require_transition_accepts_allowed_and_rejects_forbidden_safely() {
        for from in TaskStatus::ALL {
            for to in TaskStatus::ALL {
                let result = from.require_transition(to);
                if from.can_transition_to(to) {
                    assert!(result.is_ok(), "{from} -> {to} must be permitted");
                } else {
                    let error = result.expect_err("forbidden transition must fail");
                    assert_eq!(error.kind(), ErrorKind::Conflict);
                    assert_eq!(error.message(), "task status transition is not allowed");

                    let rendered = format!("{error} {error:?}");
                    assert!(
                        !rendered.contains(from.as_str()),
                        "error leaked the source status: {rendered}"
                    );
                    assert!(
                        !rendered.contains(to.as_str()),
                        "error leaked the target status: {rendered}"
                    );
                }
            }
        }
    }

    #[test]
    fn terminal_statuses_only_self_transition_or_close() {
        assert!(TaskStatus::Accepted.can_transition_to(TaskStatus::Accepted));
        assert!(TaskStatus::Closed.can_transition_to(TaskStatus::Closed));

        for to in TaskStatus::ALL {
            if to != TaskStatus::Accepted {
                assert!(
                    !TaskStatus::Accepted.can_transition_to(to),
                    "accepted must not transition to {to}"
                );
            }
            if to != TaskStatus::Closed {
                assert!(
                    !TaskStatus::Closed.can_transition_to(to),
                    "closed must not transition to {to}"
                );
            }
        }
    }

    #[test]
    fn round_kind_strings_match_frozen_vocabulary() {
        let expected = ["implement", "revise"];
        let actual: Vec<&str> = RoundKind::ALL.iter().map(|kind| kind.as_str()).collect();
        assert_eq!(actual, expected);

        for kind in RoundKind::ALL {
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn round_kind_from_str_round_trips_all() {
        for kind in RoundKind::ALL {
            let parsed = RoundKind::from_str(kind.as_str()).expect("known kind must parse");
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn unknown_round_kind_is_rejected_safely() {
        const SECRET: &str = "bogus_kind_token=secret";
        let error = RoundKind::from_str(SECRET).expect_err("unknown kind must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.message(), "unknown round kind");
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[test]
    fn round_kind_serde_is_a_string() {
        for kind in RoundKind::ALL {
            let json = serde_json::to_string(&kind).expect("serialize");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            let back: RoundKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, kind);
        }

        let error = serde_json::from_str::<RoundKind>("\"bogus\"").expect_err("unknown must fail");
        assert!(!error.to_string().contains("bogus"));
    }

    #[test]
    fn round_status_strings_match_frozen_vocabulary() {
        let expected = [
            "complete",
            "delivery_unknown",
            "failed",
            "needs_user",
            "observing",
            "pending",
            "sent",
        ];
        let actual: Vec<&str> = RoundStatus::ALL
            .iter()
            .map(|status| status.as_str())
            .collect();
        assert_eq!(actual, expected);

        for status in RoundStatus::ALL {
            assert_eq!(status.to_string(), status.as_str());
        }
    }

    #[test]
    fn round_status_from_str_round_trips_all() {
        for status in RoundStatus::ALL {
            let parsed = RoundStatus::from_str(status.as_str()).expect("known status must parse");
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn unknown_round_status_is_rejected_safely() {
        const SECRET: &str = "bogus_round_status_token=secret";
        let error = RoundStatus::from_str(SECRET).expect_err("unknown status must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.message(), "unknown round status");
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[test]
    fn round_status_serde_is_a_string() {
        for status in RoundStatus::ALL {
            let json = serde_json::to_string(&status).expect("serialize");
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            let back: RoundStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, status);
        }

        let error =
            serde_json::from_str::<RoundStatus>("\"bogus\"").expect_err("unknown must fail");
        assert!(!error.to_string().contains("bogus"));
    }

    #[test]
    fn round_status_open_set_matches_contract() {
        let expected_open = [
            "delivery_unknown",
            "needs_user",
            "observing",
            "pending",
            "sent",
        ];
        let actual_open: Vec<&str> = RoundStatus::OPEN
            .iter()
            .map(|status| status.as_str())
            .collect();
        assert_eq!(actual_open, expected_open);

        let mut open = Vec::new();
        let mut closed = Vec::new();
        for status in RoundStatus::ALL {
            assert_ne!(status.is_open(), status.is_closed());
            if status.is_open() {
                open.push(status);
            } else {
                closed.push(status);
            }
        }

        assert_eq!(open, RoundStatus::OPEN);
        assert_eq!(closed, [RoundStatus::Complete, RoundStatus::Failed]);
    }

    #[test]
    fn verifier_state_strings_match_frozen_vocabulary() {
        let expected = ["running", "done"];
        let actual: Vec<&str> = VerifierState::ALL
            .iter()
            .map(|state| state.as_str())
            .collect();
        assert_eq!(actual, expected);

        for state in VerifierState::ALL {
            assert_eq!(state.to_string(), state.as_str());
        }
    }

    #[test]
    fn verifier_state_from_str_round_trips_all() {
        for state in VerifierState::ALL {
            let parsed = VerifierState::from_str(state.as_str()).expect("known state must parse");
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn unknown_verifier_state_is_rejected_safely() {
        const SECRET: &str = "bogus_verifier_state_token=secret";
        let error = VerifierState::from_str(SECRET).expect_err("unknown state must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.message(), "unknown verifier state");
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[test]
    fn verifier_state_serde_is_a_string() {
        for state in VerifierState::ALL {
            let json = serde_json::to_string(&state).expect("serialize");
            assert_eq!(json, format!("\"{}\"", state.as_str()));
            let back: VerifierState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, state);
        }

        let error =
            serde_json::from_str::<VerifierState>("\"bogus\"").expect_err("unknown must fail");
        assert!(!error.to_string().contains("bogus"));
    }

    #[test]
    fn verification_status_strings_match_frozen_vocabulary() {
        let expected = ["passed", "failed", "timed_out", "unsafe", "error"];
        let actual: Vec<&str> = VerificationStatus::ALL
            .iter()
            .map(|status| status.as_str())
            .collect();
        assert_eq!(actual, expected);

        for status in VerificationStatus::ALL {
            assert_eq!(status.to_string(), status.as_str());
        }
    }

    #[test]
    fn verification_status_from_str_round_trips_all() {
        for status in VerificationStatus::ALL {
            let parsed =
                VerificationStatus::from_str(status.as_str()).expect("known status must parse");
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn unknown_verification_status_is_rejected_safely() {
        const SECRET: &str = "bogus_verification_status_token=secret";
        let error = VerificationStatus::from_str(SECRET)
            .expect_err("unknown verification status must fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(error.message(), "unknown verification status");
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[test]
    fn verification_status_serde_is_a_string() {
        for status in VerificationStatus::ALL {
            let json = serde_json::to_string(&status).expect("serialize");
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            let back: VerificationStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, status);
        }

        // Every reference-implementation status is accepted; only genuinely
        // unknown values are rejected.
        for proven in ["timed_out", "unsafe", "error"] {
            let json = format!("\"{proven}\"");
            assert!(
                serde_json::from_str::<VerificationStatus>(&json).is_ok(),
                "{proven} must be accepted"
            );
        }

        let error = serde_json::from_str::<VerificationStatus>("\"bogus\"")
            .expect_err("unknown status must be rejected");
        assert!(!error.to_string().contains("bogus"));
    }

    const FIXTURE_HEAD: &str = "1111111111111111111111111111111111111111";
    const FIXTURE_INDEX_FP: &str =
        "3333333333333333333333333333333333333333333333333333333333333333";
    const FIXTURE_WORKTREE_FP: &str =
        "4444444444444444444444444444444444444444444444444444444444444444";

    fn fingerprint_json() -> serde_json::Value {
        serde_json::json!({
            "head": FIXTURE_HEAD,
            "index_fingerprint": FIXTURE_INDEX_FP,
            "worktree_fingerprint": FIXTURE_WORKTREE_FP,
        })
    }

    #[test]
    fn verification_round_trips_persisted_fixture_payload() {
        // Mirrors `rounds.verifier_json` in `.docs/fixtures/sqlite/expected.json`.
        let payload = serde_json::json!({
            "after": fingerprint_json(),
            "before": fingerprint_json(),
            "commands": [
                {"command": "pytest -q", "duration": 1.234, "exit_code": 0}
            ],
            "log": "verification/task-1/round_1",
            "status": "passed"
        });

        let parsed: Verification = serde_json::from_value(payload.clone()).expect("deserialize");
        assert_eq!(parsed.status, VerificationStatus::Passed);
        let commands = parsed.commands.as_ref().expect("commands must be present");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].exit_code, Some(0));
        assert_eq!(commands[0].duration, Some(1.234));
        assert_eq!(commands[0].timed_out, None);
        assert!(parsed.before.is_some());
        assert!(parsed.after.is_some());
        assert!(parsed.side_effects.is_none());

        let encoded = serde_json::to_value(&parsed).expect("serialize");
        assert_eq!(encoded, payload);
    }

    #[test]
    fn verification_round_trips_mcp_failed_payload_with_side_effects() {
        // Mirrors the compact `verification` object in `.docs/fixtures/mcp-cases.json`.
        let payload = serde_json::json!({
            "after": fingerprint_json(),
            "before": fingerprint_json(),
            "commands": [
                {
                    "command": "pytest -q",
                    "duration": 0.5,
                    "exit_code": 1,
                    "output_tail": "1 failed"
                }
            ],
            "log": "verification/task-1/round_1",
            "side_effects": [".pytest_cache/v"],
            "status": "failed"
        });

        let parsed: Verification = serde_json::from_value(payload.clone()).expect("deserialize");
        assert_eq!(parsed.status, VerificationStatus::Failed);
        assert_eq!(
            parsed.side_effects,
            Some(vec![".pytest_cache/v".to_owned()])
        );
        let commands = parsed.commands.as_ref().expect("commands must be present");
        assert_eq!(commands[0].output_tail.as_deref(), Some("1 failed"));

        let encoded = serde_json::to_value(&parsed).expect("serialize");
        assert_eq!(encoded, payload);
    }

    #[test]
    fn verification_omits_absent_optional_keys() {
        let payload = serde_json::json!({
            "status": "passed",
            "commands": [],
            "log": "verification/task-1/round_1"
        });

        let parsed: Verification = serde_json::from_value(payload.clone()).expect("deserialize");
        assert_eq!(parsed.commands, Some(Vec::new()));
        assert!(parsed.before.is_none());
        assert!(parsed.after.is_none());

        let encoded = serde_json::to_value(&parsed).expect("serialize");
        assert_eq!(encoded, payload);
    }

    #[test]
    fn verification_rejects_unknown_status_safely() {
        let payload = serde_json::json!({
            "status": "bogus_status",
            "commands": [],
            "log": "verification/task-1/round_1"
        });

        let error = serde_json::from_value::<Verification>(payload)
            .expect_err("unknown status must be rejected");
        assert!(!error.to_string().contains("bogus_status"));
    }

    #[test]
    fn verification_round_trips_timed_out_payload() {
        // Mirrors `run_round_verification`: a timed-out command carries
        // `timed_out: true` plus `exit_code`/`duration`, and every command
        // after it is recorded as `not_run`.
        let payload = serde_json::json!({
            "status": "timed_out",
            "commands": [
                {
                    "command": "pytest -q",
                    "timed_out": true,
                    "exit_code": -9,
                    "duration": 900.0
                },
                {"command": "ruff check .", "reason": "not_run"}
            ],
            "log": "verification/task-1/round_1"
        });

        let parsed: Verification = serde_json::from_value(payload.clone()).expect("deserialize");
        assert_eq!(parsed.status, VerificationStatus::TimedOut);
        let commands = parsed.commands.as_ref().expect("commands must be present");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].timed_out, Some(true));
        assert_eq!(commands[0].exit_code, Some(-9));
        assert_eq!(commands[0].duration, Some(900.0));
        assert_eq!(commands[1].reason.as_deref(), Some("not_run"));
        assert_eq!(commands[1].timed_out, None);

        let encoded = serde_json::to_value(&parsed).expect("serialize");
        assert_eq!(encoded, payload);
    }

    #[test]
    fn verification_round_trips_unsafe_payload_without_commands() {
        // Mirrors the early `unsafe` return: no `commands` key at all.
        let payload = serde_json::json!({
            "status": "unsafe",
            "index": 0,
            "reason": "unsafe_shell_invocation",
            "log": "verification/task-1/round_1"
        });

        let parsed: Verification = serde_json::from_value(payload.clone()).expect("deserialize");
        assert_eq!(parsed.status, VerificationStatus::Unsafe);
        assert!(parsed.commands.is_none());
        assert_eq!(parsed.index, Some(0));
        assert_eq!(parsed.reason.as_deref(), Some("unsafe_shell_invocation"));

        let encoded = serde_json::to_value(&parsed).expect("serialize");
        assert_eq!(encoded, payload);
    }

    #[test]
    fn verification_round_trips_error_payload_without_commands() {
        // Mirrors the early `error` return: no `commands` key at all.
        let payload = serde_json::json!({
            "status": "error",
            "reason": "git_fingerprint_failed",
            "log": "verification/task-1/round_1"
        });

        let parsed: Verification = serde_json::from_value(payload.clone()).expect("deserialize");
        assert_eq!(parsed.status, VerificationStatus::Error);
        assert!(parsed.commands.is_none());
        assert_eq!(parsed.index, None);
        assert_eq!(parsed.reason.as_deref(), Some("git_fingerprint_failed"));

        let encoded = serde_json::to_value(&parsed).expect("serialize");
        assert_eq!(encoded, payload);
    }

    #[test]
    fn round_round_trips_representative_payload() {
        let payload = serde_json::json!({
            "task_id": SAMPLE_UUID,
            "round_number": 1,
            "kind": "implement",
            "status": "complete",
            "request_id": "req-1",
            "session_id": "ses-1",
            "error_code": null,
            "verifier_state": "done",
            "verification": {
                "after": fingerprint_json(),
                "before": fingerprint_json(),
                "commands": [
                    {"command": "pytest -q", "duration": 1.234, "exit_code": 0}
                ],
                "log": "verification/task-1/round_1",
                "status": "passed"
            }
        });

        let parsed: Round = serde_json::from_value(payload.clone()).expect("deserialize");
        assert_eq!(parsed.task_id, TaskId::from_str(SAMPLE_UUID).expect("uuid"));
        assert_eq!(parsed.round_number, 1);
        assert_eq!(parsed.kind, RoundKind::Implement);
        assert_eq!(parsed.status, RoundStatus::Complete);
        assert_eq!(parsed.request_id, "req-1");
        assert_eq!(parsed.session_id.as_deref(), Some("ses-1"));
        assert_eq!(parsed.error_code, None);
        assert_eq!(parsed.verifier_state, Some(VerifierState::Done));
        assert!(parsed.verification.is_some());

        let encoded = serde_json::to_value(&parsed).expect("serialize");
        assert_eq!(encoded, payload);
    }

    #[test]
    fn round_preserves_null_optional_scalars() {
        let payload = serde_json::json!({
            "task_id": SAMPLE_UUID,
            "round_number": 2,
            "kind": "revise",
            "status": "pending",
            "request_id": "req-2",
            "session_id": null,
            "error_code": null,
            "verifier_state": null,
            "verification": null
        });

        let parsed: Round = serde_json::from_value(payload.clone()).expect("deserialize");
        assert_eq!(parsed.kind, RoundKind::Revise);
        assert_eq!(parsed.status, RoundStatus::Pending);
        assert_eq!(parsed.session_id, None);
        assert_eq!(parsed.verifier_state, None);
        assert!(parsed.verification.is_none());

        let encoded = serde_json::to_value(&parsed).expect("serialize");
        assert_eq!(encoded, payload);
    }

    #[test]
    fn round_ignores_unproven_extra_keys() {
        let payload = serde_json::json!({
            "task_id": SAMPLE_UUID,
            "round_number": 1,
            "kind": "implement",
            "status": "complete",
            "request_id": "req-1",
            "project_id": "proj",
            "payload_hash": "hash-req-1",
            "created_at": "2026-01-01T00:00:00.000+00:00"
        });

        let parsed: Round = serde_json::from_value(payload).expect("extra keys must be ignored");
        assert_eq!(parsed.round_number, 1);
        assert_eq!(parsed.kind, RoundKind::Implement);
    }

    #[test]
    fn round_and_verification_are_distinct_types() {
        assert_ne!(TypeId::of::<Round>(), TypeId::of::<Verification>());
        assert_ne!(TypeId::of::<RoundKind>(), TypeId::of::<RoundStatus>());
    }

    #[test]
    fn git_fingerprint_serde_round_trips() {
        let payload = fingerprint_json();
        let parsed: GitFingerprint = serde_json::from_value(payload.clone()).expect("deserialize");
        assert_eq!(parsed.head, FIXTURE_HEAD);
        assert_eq!(parsed.index_fingerprint, FIXTURE_INDEX_FP);
        assert_eq!(parsed.worktree_fingerprint, FIXTURE_WORKTREE_FP);

        let encoded = serde_json::to_value(&parsed).expect("serialize");
        assert_eq!(encoded, payload);
    }

    #[test]
    fn verification_command_requires_only_command() {
        let parsed: VerificationCommand =
            serde_json::from_value(serde_json::json!({"command": "pytest -q"}))
                .expect("command-only entry must parse");
        assert_eq!(parsed.command, "pytest -q");
        assert_eq!(parsed.timed_out, None);
        assert_eq!(parsed.duration, None);
        assert_eq!(parsed.exit_code, None);
        assert_eq!(parsed.output_tail, None);
        assert_eq!(parsed.reason, None);

        let encoded = serde_json::to_value(&parsed).expect("serialize");
        assert_eq!(encoded, serde_json::json!({"command": "pytest -q"}));
    }
}
