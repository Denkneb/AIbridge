//! Session-filtered question blockers (7.7). Questions are never answered here.
use crate::{
    permission::{
        NEEDS_USER_ERROR_CODE, PermissionBlockerError, PermissionBlockerErrorKind, UserAction,
        inspect_round_state, open_state,
    },
    round_session_title,
};
use bridge_domain::{RoundStatus, TaskStatus};
use bridge_opencode::{OpenCodeClient, QuestionBlocker, QuestionError};
use bridge_storage::{
    FinishRoundInput, RoundRef, RoundRow, RoundUpdateError, RustStateLayout, Task,
};
use std::{error::Error, fmt};

pub enum QuestionBlockerError {
    Guard(PermissionBlockerError),
    Questions(QuestionError),
    Storage(RoundUpdateError),
}
impl fmt::Display for QuestionBlockerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Guard(_) => "question blocker state invalid",
            Self::Questions(_) => "opencode questions could not be listed",
            Self::Storage(_) => "question blocker could not be persisted",
        })
    }
}
impl fmt::Debug for QuestionBlockerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
impl Error for QuestionBlockerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(match self {
            Self::Guard(e) => e,
            Self::Questions(e) => e,
            Self::Storage(e) => e,
        })
    }
}
impl From<PermissionBlockerError> for QuestionBlockerError {
    fn from(error: PermissionBlockerError) -> Self {
        Self::Guard(error)
    }
}
/// Persisted blocker envelope. Debug redacts task/session/question content.
pub struct BlockedQuestions {
    session_id: String,
    questions: Vec<QuestionBlocker>,
    user_action: UserAction,
    round: RoundRow,
    task: Task,
}
impl BlockedQuestions {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    #[must_use]
    pub fn questions(&self) -> &[QuestionBlocker] {
        &self.questions
    }
    #[must_use]
    pub fn user_action(&self) -> &UserAction {
        &self.user_action
    }
    #[must_use]
    pub fn round(&self) -> &RoundRow {
        &self.round
    }
    #[must_use]
    pub fn task(&self) -> &Task {
        &self.task
    }
}
impl fmt::Debug for BlockedQuestions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BlockedQuestions { .. }")
    }
}
#[derive(Debug)]
pub enum QuestionBlockerOutcome {
    NoBlocker,
    Blocked(Box<BlockedQuestions>),
}
/// Caller holds the worker lifecycle fence across this call. Guards and current
/// session are reread after HTTP; close remains authoritative at atomic finish.
/// # Errors
/// Invalid ownership/state/session, malformed HTTP and failed persistence refuse.
pub fn handle_question_blocker(
    client: &OpenCodeClient,
    layout: &RustStateLayout,
    round: RoundRef,
) -> Result<QuestionBlockerOutcome, QuestionBlockerError> {
    if round.round_number == 0 {
        return Err(PermissionBlockerError::new(PermissionBlockerErrorKind::InvalidInput).into());
    }
    if layout.project_id() != &round.project_id {
        return Err(PermissionBlockerError::new(PermissionBlockerErrorKind::TaskMismatch).into());
    }
    let mut storage = open_state(layout)?;
    inspect_round_state(&storage, &round, client.workspace())?;
    let listed = client
        .list_questions()
        .map_err(QuestionBlockerError::Questions)?;
    let state = inspect_round_state(&storage, &round, client.workspace())?;
    let questions = listed
        .iter()
        .filter(|q| q.belongs_to_session(&state.session_id))
        .map(|q| q.blocker())
        .collect::<Vec<_>>();
    if questions.is_empty() {
        return Ok(QuestionBlockerOutcome::NoBlocker);
    }
    let user_action = UserAction::for_session(
        state.session_id.clone(),
        round_session_title(round.task_id, round.round_number),
    );
    let (task, row) = if state.already_blocked {
        (state.task, state.row)
    } else {
        let blockers = questions
            .iter()
            .map(|q| serde_json::json!({"type":q.kind(),"text":q.text()}))
            .collect::<Vec<_>>();
        let outcome = storage
            .finish_round(FinishRoundInput {
                round,
                round_status: RoundStatus::NeedsUser,
                task_status: TaskStatus::NeedsUser,
                response_message_id: None,
                response: None,
                error_code: Some(NEEDS_USER_ERROR_CODE.into()),
                result_json: Some(serde_json::json!({"blockers":blockers})),
            })
            .map_err(QuestionBlockerError::Storage)?;
        if outcome.task.status == TaskStatus::Closed || outcome.task.close_requested_at.is_some() {
            return Err(
                PermissionBlockerError::new(PermissionBlockerErrorKind::CloseRequested).into(),
            );
        }
        (outcome.task, outcome.round)
    };
    Ok(QuestionBlockerOutcome::Blocked(Box::new(
        BlockedQuestions {
            session_id: state.session_id,
            questions,
            user_action,
            round: row,
            task,
        },
    )))
}
