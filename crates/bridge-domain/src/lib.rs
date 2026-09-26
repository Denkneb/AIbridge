//! Minimal domain crate for the agent-bridge Rust implementation.
//!
//! This crate provides the shared, typed error model used by future
//! components together with the core domain identifiers ([`ProjectId`],
//! [`TaskId`]) and the frozen [`TaskStatus`] vocabulary. It deliberately
//! keeps a strict separation between the *safe* message that may be shown
//! to a user and the *diagnostic* source that may contain sensitive
//! internal details.

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

#[cfg(test)]
mod tests {
    use super::{DomainError, ErrorKind, ProjectId, TaskId, TaskStatus, crate_name};
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
}
