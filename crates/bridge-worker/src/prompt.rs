//! Initial and revision prompt construction (tasks 7.4 and 7.5).
//!
//! This module is the pure, side-effect-free half of the prompt happy paths.
//! [`initial_prompt`] renders the exact reference `prompts.INSTRUCTION_TEMPLATE`
//! for an initial (`implement`) round and [`revision_prompt`] renders the exact
//! reference `prompts.REVISION_TEMPLATE` for a revision (`revise`) round, both
//! from the persisted [`Task`] and the workspace the OpenCode client is bound
//! to. They perform no I/O and no storage access, so they can be unit-tested in
//! isolation.
//!
//! # Reference semantics
//!
//! Both outputs are byte-for-byte the reference `prompts.implementation_prompt`
//! / `prompts.revision_prompt` results:
//!
//! * `allowed_paths` is `", ".join(task.allowed_paths)` and `test_commands` is
//!   `"; ".join(task.test_commands)`, each falling back to the literal
//!   `"<none>"` for an empty list;
//! * the baseline rule is derived from `task.snapshot.dirty_paths` plus every
//!   `task.snapshot.external_repositories[].dirty_paths` qualified with the
//!   external repository `root` (trailing slashes stripped), and is the exact
//!   reference "До задачи уже существовали изменения: …" sentence when the
//!   baseline is non-empty or "До задачи рабочее дерево было чистым." otherwise;
//! * the Git rule is the permissive "Разрешены git add …" sentence only when
//!   `task.snapshot.allow_commit` is the literal JSON `true`, and the strict
//!   "Не выполняй git add …" sentence otherwise.
//!
//! Python renders each `str(path)`/`str(root)` value; a non-string scalar is
//! rendered like Python (`true` -> `True`, `null` -> `None`, numbers via their
//! canonical spelling). A JSON container inside `dirty_paths` (which the bridge
//! never writes) is skipped rather than rendered as an ambiguous path string:
//! this is a deliberate fail-closed narrowing over Python `str(container)`.
//! Falsy external roots (`null`, `false`, `0`, `""`, empty containers) are
//! skipped exactly like the reference `if not ext.get("root")`.
//!
//! Substitution is performed by a single `format!` pass, so a task text or
//! revision findings text that contains a placeholder-looking token is inserted
//! verbatim and is never re-expanded, matching Python `str.format` value
//! handling.

use std::path::Path;

use bridge_storage::Task;
use serde_json::Value;

/// Renders the exact initial (`implement`) prompt for `task` and `workspace`.
///
/// The workspace is the configured, canonical workspace the OpenCode client is
/// bound to (`config.workspace` in the reference). The function is pure and
/// cannot fail; malformed snapshot fields degrade to the reference defaults.
#[must_use]
pub fn initial_prompt(task: &Task, workspace: &Path) -> String {
    let allowed_paths = join_or_none(&task.allowed_paths, ", ");
    let test_commands = join_or_none(&task.test_commands, "; ");
    let (baseline_rule, git_rule) = git_rules(task.snapshot.as_ref());

    format!(
        "Работай только над задачей {task_id} в workspace {workspace}.\n\
Прочитай применимые AGENTS.md и проверь Git status.\n\
Меняй только согласованные allowed_paths: {allowed_paths}.\n\
Относительные пути лежат в основном workspace; абсолютные — в доверенных\n\
внешних Git-репозиториях: у каждого затронутого репозитория свой HEAD, index\n\
и статус, проверь и отрази в отчёте git status/diff, index и HEAD каждого из них.\n\
{baseline_rule}\n\
{git_rule}\n\
Запусти согласованные test_commands и сохрани реальные результаты: {test_commands}.\n\
Во время разработки ты можешь выполнять любые целевые проверки, но после твоего\n\
финального ответа мост авторитетно повторит ровно согласованные test_commands\n\
в указанном порядке; их результат не зависит от твоего отчёта.\n\
Не выполняй push, deploy или реальные внешние вызовы.\n\
Не меняй соседние проекты и конфигурацию моста.\n\
По завершении верни: изменения, список файлов, команды проверок,\n\
коды завершения, результаты, что не проверено, оставшиеся проблемы.\n\
После отчёта прекрати изменения и жди ревью.\n\
\n\
Задача:\n\
{task}\n",
        task_id = task.task_id,
        workspace = workspace.display(),
        allowed_paths = allowed_paths,
        baseline_rule = baseline_rule,
        git_rule = git_rule,
        test_commands = test_commands,
        task = task.text,
    )
}

/// Renders the exact revision (`revise`) prompt for `task`, `workspace`, the
/// persisted revision `findings` and the one-based `round_number`.
///
/// The workspace is the configured, canonical workspace the OpenCode client is
/// bound to (`config.workspace` in the reference). `findings` is the persisted
/// current round's `findings` text; the caller passes the reference
/// `round_obj.findings or ""` fallback, so `None` becomes the empty string. The
/// function is pure and cannot fail; malformed snapshot fields degrade to the
/// reference defaults.
///
/// The revision is deliberately self-contained, exactly like the reference
/// `prompts.revision_prompt`: the deterministic new session must not rely on the
/// previous rounds, so the original task text, the findings and the round number
/// are always rendered in full. Findings are inserted verbatim through a single
/// `format!` pass, so placeholder-looking text in the findings (or the task) is
/// never re-expanded.
#[must_use]
pub fn revision_prompt(task: &Task, workspace: &Path, findings: &str, round_number: u32) -> String {
    let allowed_paths = join_or_none(&task.allowed_paths, ", ");
    let test_commands = join_or_none(&task.test_commands, "; ");
    let (baseline_rule, git_rule) = git_rules(task.snapshot.as_ref());

    format!(
        "Доработай задачу {task_id} в workspace {workspace}, раунд {round_number}.\n\
Это новая независимая сессия: не полагайся на контекст предыдущих раундов,\n\
прочитай исходную задачу и замечания ниже и сначала проверь текущее состояние\n\
Git и файлов командой git status и просмотром файлов.\n\
Меняй только согласованные allowed_paths: {allowed_paths}.\n\
Относительные пути лежат в основном workspace; абсолютные — в доверенных\n\
внешних Git-репозиториях: у каждого затронутого репозитория свой HEAD, index\n\
и статус, проверь и отрази в отчёте git status/diff, index и HEAD каждого из них.\n\
{baseline_rule}\n\
{git_rule}\n\
Исправь только перечисленные замечания ревью. Не откатывай и не переделывай уже\n\
корректные изменения, сделанные в предыдущих раундах.\n\
Снова запусти согласованные test_commands: {test_commands} и сохрани реальные результаты.\n\
Во время доработки ты можешь выполнять любые целевые проверки, но после твоего\n\
финального ответа мост авторитетно повторит ровно согласованные test_commands;\n\
их результат не зависит от твоего отчёта.\n\
Не выполняй push или deploy.\n\
Верни: что изменено, результаты проверок, что не проверено и какие ограничения остались.\n\
\n\
Исходная задача:\n\
{task}\n\
\n\
Замечания ревью:\n\
{findings}\n",
        task_id = task.task_id,
        workspace = workspace.display(),
        round_number = round_number,
        allowed_paths = allowed_paths,
        baseline_rule = baseline_rule,
        git_rule = git_rule,
        test_commands = test_commands,
        task = task.text,
        findings = findings,
    )
}

/// Joins `values` with `separator`, or returns the reference `"<none>"`.
fn join_or_none(values: &[String], separator: &str) -> String {
    if values.is_empty() {
        "<none>".to_owned()
    } else {
        values.join(separator)
    }
}

/// Renders the `(baseline_rule, git_rule)` pair exactly like `prompts._git_rules`.
fn git_rules(snapshot: Option<&Value>) -> (String, String) {
    let baseline = baseline_paths(snapshot);
    let baseline_rule = if baseline.is_empty() {
        "До задачи рабочее дерево было чистым.".to_owned()
    } else {
        format!(
            "До задачи уже существовали изменения: {}. Считай их согласованным baseline; \
             не выполняй reset, clean, stash и не теряй исходное содержимое.",
            baseline.join(", ")
        )
    };

    let allow_commit = snapshot
        .and_then(Value::as_object)
        .and_then(|object| object.get("allow_commit"))
        == Some(&Value::Bool(true));
    let git_rule = if allow_commit {
        "Разрешены git add и один или несколько новых commit только для allowed_paths \
         в соответствующем репозитории. Не выполняй amend, rebase, reset или изменение \
         существующей истории."
            .to_owned()
    } else {
        "Не выполняй git add, commit и любые изменения index или HEAD ни в одном \
         затронутом репозитории."
            .to_owned()
    };
    (baseline_rule, git_rule)
}

/// Collects the qualified baseline dirty paths exactly like
/// `prompts._baseline_paths`.
fn baseline_paths(snapshot: Option<&Value>) -> Vec<String> {
    let Some(object) = snapshot.and_then(Value::as_object) else {
        return Vec::new();
    };

    let mut baseline = Vec::new();
    if let Some(paths) = object.get("dirty_paths").and_then(Value::as_array) {
        baseline.extend(paths.iter().filter_map(python_scalar_str));
    }
    if let Some(externals) = object
        .get("external_repositories")
        .and_then(Value::as_array)
    {
        for external in externals {
            let Some(external) = external.as_object() else {
                continue;
            };
            let Some(root) = external.get("root") else {
                continue;
            };
            if is_falsy(root) {
                continue;
            }
            let Some(root) = python_scalar_str(root) else {
                continue;
            };
            let root = root.trim_end_matches('/');
            if let Some(paths) = external.get("dirty_paths").and_then(Value::as_array) {
                baseline.extend(
                    paths
                        .iter()
                        .filter_map(python_scalar_str)
                        .map(|path| format!("{root}/{path}")),
                );
            }
        }
    }
    baseline
}

/// Renders a JSON scalar like Python `str(value)`.
///
/// A JSON array or object has no unambiguous path spelling, so it is skipped
/// (fail closed) instead of being rendered as a container literal.
fn python_scalar_str(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Bool(true) => Some("True".to_owned()),
        Value::Bool(false) => Some("False".to_owned()),
        Value::Number(number) => Some(number.to_string()),
        Value::Null => Some("None".to_owned()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// Mirrors Python truthiness for the JSON values a snapshot can carry.
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(value) => !value,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(values) => values.is_empty(),
        Value::Object(values) => values.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::{initial_prompt, revision_prompt};
    use bridge_domain::{ProjectId, TaskId};
    use bridge_storage::Task;
    use serde_json::json;
    use std::path::Path;
    use std::str::FromStr;

    const TASK_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn task(snapshot: Option<serde_json::Value>) -> Task {
        Task {
            task_id: TaskId::from_str(TASK_UUID).expect("task id"),
            project_id: ProjectId::from_str("proj-1").expect("project id"),
            workspace: "/ws".to_owned(),
            status: bridge_domain::TaskStatus::Implementing,
            session_id: None,
            text: "do work".to_owned(),
            allowed_paths: vec!["Cargo.toml".to_owned(), "src".to_owned()],
            test_commands: vec!["cargo test".to_owned(), "cargo clippy".to_owned()],
            created_at: "2026-01-01T00:00:00.000Z".to_owned(),
            updated_at: "2026-01-01T00:00:00.000Z".to_owned(),
            base_head: None,
            snapshot,
            revision_count: 0,
            close_requested_at: None,
            close_reason: None,
            budget: None,
        }
    }

    #[test]
    fn clean_baseline_and_strict_git_rule_are_rendered() {
        let prompt = initial_prompt(&task(None), Path::new("/home/ws"));
        assert!(prompt.starts_with(
            "Работай только над задачей 550e8400-e29b-41d4-a716-446655440000 \
             в workspace /home/ws.\n"
        ));
        assert!(prompt.contains("allowed_paths: Cargo.toml, src."));
        assert!(
            prompt
                .contains("test_commands и сохрани реальные результаты: cargo test; cargo clippy.")
        );
        assert!(prompt.contains("До задачи рабочее дерево было чистым."));
        assert!(prompt.contains(
            "Не выполняй git add, commit и любые изменения index или HEAD ни в одном \
             затронутом репозитории."
        ));
        assert!(prompt.ends_with("Задача:\ndo work\n"));
    }

    #[test]
    fn dirty_baseline_and_permissive_git_rule_are_rendered() {
        let snapshot = json!({
            "dirty_paths": ["Cargo.lock", "README.md"],
            "allow_commit": true,
            "external_repositories": [
                {"root": "/ext/repo/", "dirty_paths": ["a.rs"]},
                {"root": "", "dirty_paths": ["ignored.rs"]},
            ],
        });
        let prompt = initial_prompt(&task(Some(snapshot)), Path::new("/home/ws"));
        assert!(prompt.contains(
            "До задачи уже существовали изменения: Cargo.lock, README.md, /ext/repo/a.rs. \
             Считай их согласованным baseline; не выполняй reset, clean, stash и не теряй \
             исходное содержимое."
        ));
        assert!(prompt.contains("Разрешены git add и один или несколько новых commit"));
        assert!(!prompt.contains("ignored.rs"));
    }

    #[test]
    fn empty_lists_use_the_none_placeholder() {
        let mut task = task(None);
        task.allowed_paths.clear();
        task.test_commands.clear();
        let prompt = initial_prompt(&task, Path::new("/ws"));
        assert!(prompt.contains("allowed_paths: <none>."));
        assert!(prompt.contains("test_commands и сохрани реальные результаты: <none>."));
    }

    #[test]
    fn placeholder_like_task_text_is_not_re_expanded() {
        let mut task = task(None);
        task.text = "{task_id} {task} {workspace}".to_owned();
        let prompt = initial_prompt(&task, Path::new("/ws"));
        assert!(prompt.ends_with("Задача:\n{task_id} {task} {workspace}\n"));
    }

    #[test]
    fn revision_prompt_renders_round_number_task_and_findings() {
        let mut task = task(None);
        task.text = "original task".to_owned();
        let prompt = revision_prompt(&task, Path::new("/home/ws"), "fix the bug", 2);
        assert!(prompt.starts_with(
            "Доработай задачу 550e8400-e29b-41d4-a716-446655440000 \
             в workspace /home/ws, раунд 2.\n"
        ));
        assert!(prompt.contains("Меняй только согласованные allowed_paths: Cargo.toml, src."));
        assert!(prompt.contains(
            "Снова запусти согласованные test_commands: cargo test; cargo clippy \
                 и сохрани реальные результаты."
        ));
        assert!(prompt.contains("До задачи рабочее дерево было чистым."));
        assert!(prompt.contains(
            "Не выполняй git add, commit и любые изменения index или HEAD ни в одном \
             затронутом репозитории."
        ));
        assert!(prompt.contains("Исходная задача:\noriginal task\n"));
        assert!(prompt.ends_with("Замечания ревью:\nfix the bug\n"));
    }

    #[test]
    fn revision_prompt_renders_empty_findings_as_an_empty_section() {
        let task = task(None);
        let prompt = revision_prompt(&task, Path::new("/ws"), "", 3);
        assert!(prompt.ends_with("Замечания ревью:\n\n"));
    }

    #[test]
    fn revision_prompt_uses_baseline_and_permissive_git_rule() {
        let snapshot = json!({
            "dirty_paths": ["Cargo.lock"],
            "allow_commit": true,
            "external_repositories": [
                {"root": "/ext/repo/", "dirty_paths": ["a.rs"]},
            ],
        });
        let prompt = revision_prompt(&task(Some(snapshot)), Path::new("/ws"), "note", 4);
        assert!(prompt.contains(
            "До задачи уже существовали изменения: Cargo.lock, /ext/repo/a.rs. \
             Считай их согласованным baseline; не выполняй reset, clean, stash и не теряй \
             исходное содержимое."
        ));
        assert!(prompt.contains("Разрешены git add и один или несколько новых commit"));
    }

    #[test]
    fn revision_prompt_does_not_re_expand_placeholders_in_task_or_findings() {
        let mut task = task(None);
        task.text = "{task_id} {findings} {workspace}".to_owned();
        let prompt = revision_prompt(
            &task,
            Path::new("/ws"),
            "{round_number} {task} {baseline_rule}",
            5,
        );
        assert!(prompt.contains("Исходная задача:\n{task_id} {findings} {workspace}\n"));
        assert!(prompt.ends_with("Замечания ревью:\n{round_number} {task} {baseline_rule}\n"));
    }
}
