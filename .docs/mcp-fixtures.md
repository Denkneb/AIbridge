# Fixtures MCP-контракта `agent_bridge`

Машиночитаемый corpus: [fixtures/mcp-cases.json](fixtures/mcp-cases.json).
Это контрактные fixtures для будущих Rust-задач потоков 8 (MCP) и 3 (storage);
они описывают наблюдаемое поведение существующего Python-кода, а не целевую
реализацию. Rust здесь не реализуется.

Corpus намеренно детерминирован: без timestamps, секретов, реальных
tokens/passwords и абсолютных machine-specific путей. JSON-ключи отсортированы,
`cases` отсортированы по `id`. Все внешние значения заданы документированными
placeholder-ами. Один case описывает одну независимую ветвь поведения; полная
Python test suite не копируется.

## Источники

Corpus построен по фактическому коду и тестам Python-репозитория
`/home/denis/Python/agent_bridge` (только чтение), в первую очередь:

- `src/agent_bridge/mcp_server.py` — шесть MCP tools, compact/verbose
  `task_status`, `user_action`, idempotency, recovery-ветви, коды ошибок;
- `src/agent_bridge/mcp_http.py` — authenticated Streamable HTTP transport и его
  контракт ожидания;
- `src/agent_bridge/storage.py` — статусы task/round, атомарные переходы,
  `reopen_failed_round`, cooperative close, request idempotency;
- `src/agent_bridge/worker.py` — `_collect_changes`, per-repository результат,
  `deterministic_session_title`, recovery side effects;
- `src/agent_bridge/git_snapshot.py` — `validate_allowed_paths`,
  `group_allowed_paths_by_repo`, `take_external_snapshots`, `scope_violations`;
- `src/agent_bridge/verifier.py` — validation тестовых команд и compact
  verification summary;
- `tests/test_mcp.py`, `tests/test_mcp_http.py` — подтверждение наблюдаемых
  значений и ветвей.

Версия-источник зафиксирована в поле `source.commit`. Python-репозиторий не
изменялся.

## Schema corpus

Верхний уровень:

| Поле | Назначение |
| --- | --- |
| `corpus_version` | версия формата corpus |
| `source` | commit Python-репозитория и список модулей/tests |
| `placeholder_conventions` | словарь placeholder → описание подстановки |
| `normalization_rules` | правила нормализации нестабильных значений |
| `error_categories` | стабильные идентификаторы категорий ошибок и их смысл |
| `cases` | упорядоченный по `id` список case-ов |

Каждый case:

| Поле | Обязательность | Назначение |
| --- | --- | --- |
| `id` | всегда | стабильный идентификатор case |
| `tool` | всегда | один из шести tools: `project_info`, `submit_task`, `task_status`, `request_changes`, `accept_task`, `close_task` |
| `description` | всегда | краткое пояснение ветви |
| `setup` | всегда | семантическое описание состояния перед вызовом |
| `input` | всегда | нормализованные аргументы tool |
| `expectation` | всегда | `success` или `error` |
| `expect` | для `success` | ожидаемый семантический вывод |
| `error_category` | для `error` | стабильная категория из `error_categories` |
| `error_payload` | для `error` | дополнительные проверяемые поля ответа |
| `normalization` | всегда | применимые правила из `normalization_rules` |
| `evidence` | всегда | ссылки на модуль:функцию и/или `tests/...::test` |
| `non_contract_fields` | опционально | JSON-пути, текст которых не сравнивается |

### `setup`

`setup` описывает состояние, которое harness должен материализовать до вызова
tool. Используемые ключи:

| Ключ | Значение |
| --- | --- |
| `server` | `ready`, `idle`, `unavailable`, `unhealthy`, `wrong_directory`, `busy` или `retry` |
| `task` | `null` или объект задачи |
| `workspace` | `git_repo`, `dirty_paths`, `trusted_roots`, `snapshot_raises` |
| `external_repositories` | список `root`/`dirty_paths`/`allowed_paths` |
| `snapshot_dirty_paths` | submit-time snapshot dirty list (fallback для baseline) |
| `existing_round` | request_id/kind/payload_hash уже сохранённого round |
| `open_round`, `error_code`, `blocker_pending` | условия recovery-ветви |
| `session_history` | семантическое описание истории сессии для recovery |
| `recovery` | ожидаемое поведение recovery worker-а |
| `max_rounds` | переопределение лимита ревизий |

Объект `task`:

| Ключ | Значение |
| --- | --- |
| `task_id` | `${TASK_ID}` |
| `status` | task status из domain manifest |
| `session_id` | `${SESSION_ID}` / `${REVISION_SESSION_ID}` / `null` |
| `revision_count` | число ревизий |
| `close_requested` | присутствует и `true`, если запрошено закрытие |
| `worker_lock` | `free` или `held` |
| `worker_window` | `started_at`/`deadline_at` (нормализованные) |
| `rounds` | список round-ов с `round_number`, `kind`, `status`, `request_id`, `stored_result` |

`stored_result` повторяет только те поля persisted `result_json`, на которые
направлена ветвь (`changed_paths`, `scope_violations`, `verification`,
`repositories`, `baseline_dirty_paths`, `usage`, `model`, `blockers`,
`tool_errors`, `error`, `head_before`, `head_after`, `allow_dirty`,
`allow_commit`).

### `expect`

| Ключ | Значение |
| --- | --- |
| `shape` | `exact` (набор ключей ответа точно равен `fields`), `contains` (все `fields` присутствуют) |
| `fields` | проверяемые top-level ключи ответа |
| `values` | проверяемые нормализованные значения (включая вложенные объекты) |
| `absent` | ключи, которых в ответе быть не должно |
| `invariants` | текстовые инварианты между несколькими ответами |
| `side_effects` | наблюдаемые изменения состояния/БД/spawn |

`expect` не повторяет весь payload: он содержит только те поля, на которые
направлен case. Compact и verbose `task_status` описаны отдельными cases, при
этом verbose-case не дублирует все payloads, а фиксирует полный набор полей и
ссылается на соответствующий compact case.

## Placeholder conventions

Полный список — в `placeholder_conventions`. Ключевые:

- `${PROJECT_ID}`, `${TASK_ID}`, `${OTHER_TASK_ID}` — стабильные синтетические
  идентификаторы (`proj`, `task-1`, `task-2`); реальный UUID нормализуется сюда;
- `${SESSION_ID}`, `${REVISION_SESSION_ID}`, `${MESSAGE_ID}` — session/message ids;
- `${WORKSPACE}`, `${EXTERNAL_REPO}`, `${FOREIGN_PATH}`, `${FIXTURE_DIR}` —
  абсолютные пути; `${EXTERNAL_DIRTY_PATH}` = `${EXTERNAL_REPO}/lib.py`;
- `${HEAD_BEFORE}`, `${HEAD_AFTER}`, `${FINGERPRINT}` — Git-отпечатки;
- `${TIMESTAMP}`, `${DURATION}` — timestamps и длительности;
- `${CONSOLE_COMMAND}`, `${ATTACH_COMMAND}` — shlex-join argv команд
  `agent-bridge console` / `attach-opencode`;
- `${SESSION_TITLE}` — детерминированный `agent-bridge ${TASK_ID} round <n>`;
- `${LOG_PATH}` — относительный путь verification-лога
  `verification/${TASK_ID}/round_<n>`;
- `${OUTPUT_TAIL}`, `${DETAIL}` — не являющийся контрактом текст.

## Normalization rules

Полный список — в `normalization_rules`. Нормализуются: UUID task ids, session
и message ids, timestamps, filesystem paths, deadlines и durations, PIDs, Git
HEAD/index/worktree fingerprints, а также human-readable `detail` и
`output_tail`. Human-readable `user_action.message`/`instructions` помечены в
`non_contract_fields`: контрактом являются `user_action.type`,
`session_id`, `session_title`, `command` и `fallback_command`.

`needs_user.user_action` должен быть **byte-identical** между compact
(`verbose=false`) и verbose (`verbose=true`) ответами; это зафиксировано
инвариантом в `task-status-needs-user-user-action-verbosity-invariant`.

## Покрытие

- `project_info`: immutable binding с активной задачей и без неё;
- `submit_task`: clean/dirty success, external-repo snapshot, idempotency,
  request_conflict, validation (`request_id`, `task`, `allowed_paths`,
  `test_commands`, `git_policy`), `project_busy`, `dirty_workspace`,
  `dirty_paths_outside_scope` (main и external), `not_a_git_repo`,
  `git_snapshot_failed`, `server_unavailable`/`unhealthy`/`wrong_directory`;
- `task_status`: compact `implementing`/`revising`; default
  `awaiting_review` (минимальный набор, baseline persisted/fallback/omitted,
  verification summary, external scope violation, omit repositories);
  `needs_user` (diagnostics, `user_action`, no-session, verbosity-invariant);
  `failed`/`delivery_unknown` (exact reason); `accepted`/`closed` (short);
  `no_active_task`/`unknown_task`; `wait_seconds` boundaries
  (`-1`, `True`, `301`, `0`, `300`) и ранний возврат; verbose full contract,
  repositories single/multi и external git-policy violations; recovery side
  effects (`needs_user` resolved/pending, failed assistant_error
  initial/revision, nonrecoverable failed, delivery_unknown waits for server);
- `request_changes`: success, idempotency, conflict, validation,
  `not_awaiting_review`, `revision_limit`, pending `needs_user`, recovery;
- `accept_task`: success, idempotency, `unknown_task`,
  `not_awaiting_review`, `worker_running`, `server_busy`/`server_unavailable`,
  pending `needs_user`;
- `close_task`: direct close, cooperative `close_requested`, idempotent
  terminal, `reason_required`, `unknown_task`, `server_busy`/`server_unavailable`.

## Использование в дифференциальных Python/Rust tests

1. Читать `cases` из JSON.
2. Материализовать `setup` (SQLite rows, snapshot, worker lock, mock OpenCode
   endpoint) и нормализовать вход по `normalization`.
3. Вызвать tool в Python-реализации и в Rust-реализации.
4. Для `error` проверить одинаковую `error_category` и поля `error_payload`;
   literal `detail` и `output_tail` не сравниваются.
5. Для `success` сравнить `expect.values`, проверить `expect.fields`/`shape`,
   отсутствие `expect.absent` и соблюдение `expect.invariants`; для `exact`
   набор ключей должен совпадать.
6. Проверить `side_effects` по БД/состоянию, где они наблюдаемы.
7. Сравнивать `user_action` между compact и verbose байт-в-байт.

Такой harness воспроизводит corpus на текущей Python suite и позже становится
общим differential-раннером Python/Rust.
