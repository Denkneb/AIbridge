//! Fail-open hook context uses only narrow validated identity/status fields.
use crate::diagnostics::{Brief, safe_timestamp};
use bridge_domain::{ProjectId, TaskStatus};
use serde_json::{Value, json};
pub const MAX_CONTEXT_CHARS: usize = 1200;
pub fn context(briefs: &[Brief]) -> Result<Option<Value>, &'static str> {
    if briefs.is_empty() {
        return Ok(None);
    }
    let mut lines = vec![];
    for b in briefs {
        if b.project_id.len() > 64
            || !b
                .project_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
            || !b
                .project_id
                .bytes()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            || ProjectId::try_from(b.project_id.clone()).is_err()
            || !safe_timestamp(&b.updated_at)
            || !b.status.is_active()
            || b.phase.is_some_and(|p| {
                !matches!(p, "agent" | "verifying")
                    || !matches!(b.status, TaskStatus::Implementing | TaskStatus::Revising)
            })
        {
            return Err("unsafe_hook_context");
        }
        let mut line = format!(
            "[agent-bridge] project {}, task {}, status {}, updated_at {}.",
            b.project_id,
            b.task_id,
            b.status.as_str(),
            b.updated_at
        );
        if briefs.len() == 1 {
            line.push_str(match b.status {
   TaskStatus::Implementing|TaskStatus::Revising if b.phase==Some("verifying")=>" Финальный ответ OpenCode готов, идут авторитетные проверки (verifier); дождись awaiting_review.",
   TaskStatus::Implementing|TaskStatus::Revising=>" OpenCode работает; при необходимости опроси task_status(wait_seconds=300) без ID.",
   TaskStatus::WaitingDependencies=>" Задача ждёт accepted-зависимостей; вызови task_status без ID и проверь workflow gate (waiting/ready/blocking).",
   TaskStatus::AwaitingReview=>" Вызови task_status без ID и независимо проверь git diff/status, новые файлы, allowed_paths, index/HEAD затронутых репозиториев, прежде чем решать accept_task/request_changes.",
   TaskStatus::NeedsUser=>" Вызови task_status без ID и покажи его user_action (message, command, session_id, session_title, fallback_command, instructions) дословно; внешний доступ автоматически не разрешай.",
   TaskStatus::Failed|TaskStatus::DeliveryUnknown=>" Выполнение остановилось (блокер); вызови task_status без ID, чтобы получить детали.",_=>""});
        }
        lines.push(line);
    }
    if briefs.len() > 1 {
        lines.push("[agent-bridge] несколько активных задач; вызывай task_status с явным task_id (без ID отвечает ambiguous_task).".into());
    }
    let context = lines
        .join("\n")
        .chars()
        .take(MAX_CONTEXT_CHARS)
        .collect::<String>();
    Ok(Some(
        json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":context}}),
    ))
}
