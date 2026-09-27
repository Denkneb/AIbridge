# agent-bridge (Rust)

Rust-переписывание `agent-bridge`: Cargo workspace с доменной моделью
(`bridge-domain`), загрузчиком конфигурации (`bridge-config`), read-only
инспекцией SQLite (`bridge-storage`) и CLI (`agent-bridge-cli`). Цель —
сохранить контракты CLI, MCP и SQLite, перейти к единому Rust-бинарнику и
добавить GUI на GPUI. Миграция идёт поэтапно, без одномоментной замены Python.
Python и Rust никогда не делят рабочую БД или runtime state: у каждой реализации
свои state root, SQLite, locks, PID/ownership records, token-файлы, логи и
endpoints, а перенос истории — только односторонний импорт WAL-aware копии при
остановленном Python runtime.

## Статус миграции

- Завершены этапы конфигурации **2.1–2.9** (базовый TOML loader, project ID и
  workspace validation, endpoint/port validation, уникальность
  workspace/endpoints/token files, max rounds/model/optional paths,
  auto-approve permissions, trusted external directories, credentials reader,
  project env reader).
- Завершён этап **3.1. Read-only schema inspection**: crate `bridge-storage`
  открывает существующую БД строго read-only (`mode=ro&immutable=1`),
  проверяет согласованную пару `PRAGMA user_version=6` и
  `meta.schema_version='6'` и сверяет tables/columns/indexes/foreign keys с
  schema v6, не создавая, не мигрируя и не изменяя БД.
- Завершён этап **3.2. WAL, foreign keys и busy timeout**: `bridge-storage`
  предоставляет runtime read-write подключение, которое создаёт отсутствующий
  parent directory, применяет и проверяет `PRAGMA journal_mode=WAL`,
  `PRAGMA foreign_keys=ON` и `PRAGMA busy_timeout=30000` (fail closed) и
  закрывается RAII, не создавая schema и не меняя `PRAGMA user_version`.
- Завершён этап **3.3. Task row mapping**: `bridge-storage` преобразует строку
  schema v6 таблицы `tasks` в типизированную модель `Task` (`TaskId`,
  `ProjectId`, `TaskStatus`, JSON-поля `allowed_paths`, `test_commands`,
  `snapshot`) и fail closed отвергает неизвестный статус, невалидный task
  UUID, повреждённый JSON, несоответствие SQLite-типа и отрицательный
  `revision_count`, не добавляя query/list/write API.
- Завершён этап **3.4. Round row mapping**: `bridge-storage` преобразует строку
  schema v6 таблицы `rounds` в полную типизированную storage-модель `RoundRow`
  (все 21 колонка; `TaskId`, `ProjectId`, `RoundKind`, `RoundStatus`,
  `VerifierState` и доменный `Verification`), проверяет диапазон
  `round_number` 1..=u32::MAX, строгий SQLite-`attempted` 0/1, форму
  `result_json`, декодирует `verifier_json` как `Verification` и отвергает
  несогласованную пару `verifier_state`/`verifier_json`, не добавляя
  query/list/write API и не меняя fixtures.
- Завершён этап **3.5. Initialization совместимой пустой БД**:
  `bridge-storage` предоставляет публичный `initialize`, который на runtime
  connection атомарно (одна `BEGIN IMMEDIATE` транзакция с DDL и маркерами
  `PRAGMA user_version=6` + `meta.schema_version='6'`) и идемпотентно создаёт
  итоговую schema v6 (`meta`/`tasks`/`rounds`/`events`,
  `ux_tasks_active`/`ux_rounds_request`/`ix_events_task`) для отсутствующего файла
  или действительно пустой БД; уже совместимая v6 валидируется полным frozen
  contract и не меняется, а несовместимая, частичная или чужая БД, а также
  `user_version` вне `0/6` завершаются fail closed без repair/upgrade.
  Конкурентные вызовы сериализуются writer-транзакцией. DDL вынесен в единый
  production-контракт, используемый и тестами; fixtures не изменяются.
- Завершён этап **3.6. Get/list/pagination queries**: `bridge-storage`
  предоставляет read-only task query API на `StorageConnection`
  (`get_task`, `get_active_task`, `list_tasks`, `count_tasks`) поверх единого
  production-списка 15 колонок `tasks`; каждая найденная строка маппится через
  `Task::from_row`, данные строго изолированы по `project_id`, active-фильтр
  использует ровно словарь `TaskStatus::is_active`, `list_tasks` сортирует
  `updated_at DESC, created_at DESC, task_id DESC` с пагинацией
  `limit > 0`/`offset >= 0`, а типизированный `QueryError` не раскрывает ids,
  project, row data, SQL и пути. Query API ничего не пишет и не меняет schema.
- Завершён этап **3.7. Atomic task creation**: `bridge-storage` предоставляет
  типизированный `CreateTaskInput` и `StorageConnection::create_task`, который в
  одной `BEGIN IMMEDIATE` транзакции атомарно создаёт ровно три строки — task
  (`implementing`, `revision_count=0`), initial round (`round_number=1`,
  `implement`, `pending`, `attempted=0`) и event (`created`,
  `task created (implement)`) — с общим UTC RFC3339 timestamp с миллисекундами,
  сериализует `allowed_paths`/`test_commands` как JSON-массивы строк, принимает
  `snapshot` только как `None` или JSON object и возвращает созданный `Task`
  через существующий mapping. Ошибка round/event после task insert полностью
  откатывает все три строки; типизированный `CreateTaskError`
  (`ProjectBusy`/`TaskIdConflict`/`RequestConflict`/`InvalidInput`/
  `Serialization`/`TaskRow`/`InvalidPersistedState`/`Database`) не раскрывает
  ids, project, task text, workspace, request, hash, пути, SQL и JSON.
- Завершён этап **3.8. Request idempotency**: `StorageConnection::create_task`
  в той же `BEGIN IMMEDIATE` транзакции до inserts находит round по точной паре
  `project_id`+`request_id` и маппит его через `RoundRow::from_row`. Совпадающий
  `implement`/`round_number=1`/`payload_hash` с существующим task того же project
  возвращает типизированный `CreateTaskOutcome::Replayed` с исходным `Task` и
  ничего не пишет (новые `task_id`/payload игнорируются, timestamps, status,
  session, revision_count и JSON не меняются, event не добавляется); другой
  `payload_hash` или `kind != implement` — `RequestConflict`; немаппящийся round,
  не-первый `implement` round, отсутствующий/немаппящийся linked task или
  несовпадение project — fail-closed `InvalidPersistedState` без записей. Для
  нового request сохраняется семантика 3.7 (`ProjectBusy` при активной чужой
  задаче, точные task+initial round+event, `CreateTaskOutcome::Created`).
  Конкурентные одинаковые запросы дают ровно один `Created` и остальные
  `Replayed`, конкурентные разные hash — один `Created` и `RequestConflict`, без
  частичных строк; одинаковый `request_id` разных projects независим.
- Завершён этап **3.9a. Atomic round/task lifecycle updates**:
  `bridge-storage` предоставляет типизированные lifecycle-API на
  `StorageConnection` (`create_revision_round`, `bind_round_session`,
  `prepare_round`, `mark_round_sent`, `mark_round_observing`,
  `mark_worker_started`, `finish_round`). Каждый вызов выполняется в одной
  `BEGIN IMMEDIATE` транзакции, проверяет смену round-статуса по локальной
  таблице `ROUND_TRANSITIONS` (transcribed из `domain.round_transitions` для
  путей `pending->{sent,observing}`, `sent->observing` и
  `observing/needs_user/delivery_unknown->{complete,failed,needs_user,delivery_unknown}`)
  и проверяет каждую смену task-статуса через `TaskStatus::require_transition`
  до UPDATE. `create_revision_round` фиксирует `revise`-round, требует
  последовательный номер, очищает `tasks.session_id`, переводит task в
  `revising` и увеличивает `revision_count` ровно один раз, не выполняя replay
  request; `bind_round_session` пишет `rounds.session_id` и `tasks.session_id`
  одним timestamp; `prepare_round`/`mark_round_sent`/`mark_round_observing`
  сохраняют семантику `attempted`; `mark_worker_started` пишет start/deadline от
  одного clock sample; `finish_round` атомарно обновляет round/task и пишет
  ровно одно событие. Paired-записи и событие используют один timestamp, а
  ошибки (`MissingTask`/`MissingRound`/`ProjectMismatch`/`NotCurrentRound`/
  `NonSequentialRound`/`RequestConflict`/`InvalidRoundTransition`/
  `InvalidTaskTransition`/`InvalidJson`/`InvalidDeadline`/`InvalidInput` и др.)
  безопасны и не раскрывают ids, session/message ids, SQL, JSON и пути. Verifier
  persist-once, cooperative close, `reopen_failed_round` и schema/fixtures
  изменения не входят.
- Зафиксирована (документация, ещё не реализовано) стратегия изоляции storage:
  Python и Rust используют раздельные state root, SQLite, locks, PID/ownership
  records, token-файлы, логи и endpoints, а перенос истории — односторонний
  импорт WAL-aware копии при остановленном Python runtime без общей рабочей БД.
  Реализация — задачи **3.10** (storage isolation/импорт копии) и **3.11**
  (ownership/format marker).
- Следующий этап — **3.9b. Verifier persist-once** (весь поток 3.9 ещё не
  завершён).
- Полный план и очередь задач: [docs/implementation-plan.md](docs/implementation-plan.md).

## Документация

- [docs/README.md](docs/README.md) — обзор комплекта документов.
- [docs/implementation-plan.md](docs/implementation-plan.md) — план и статус миграции.
- [docs/architecture.md](docs/architecture.md) — целевая архитектура.
- [docs/existing-contract.md](docs/existing-contract.md) — существующий контракт.
- [docs/contract-manifest.json](docs/contract-manifest.json) — manifest контракта.
- [docs/testing-and-migration.md](docs/testing-and-migration.md) — тестирование и миграция.

## Fixtures и corpora

- Конфигурация: [docs/config-fixtures.md](docs/config-fixtures.md),
  [docs/fixtures/config-cases.json](docs/fixtures/config-cases.json)
- MCP: [docs/mcp-fixtures.md](docs/mcp-fixtures.md),
  [docs/fixtures/mcp-cases.json](docs/fixtures/mcp-cases.json)
- SQLite: [docs/sqlite-fixtures.md](docs/sqlite-fixtures.md),
  [docs/fixtures/sqlite/expected.json](docs/fixtures/sqlite/expected.json)
- Политика команд: [docs/command-policy-fixtures.md](docs/command-policy-fixtures.md),
  [docs/fixtures/command-policy-cases.json](docs/fixtures/command-policy-cases.json)
- Политика путей: [docs/path-policy-fixtures.md](docs/path-policy-fixtures.md),
  [docs/fixtures/path-policy-cases.json](docs/fixtures/path-policy-cases.json)
- Git snapshot: [docs/git-snapshot-fixtures.md](docs/git-snapshot-fixtures.md),
  [docs/fixtures/git-snapshot-cases.json](docs/fixtures/git-snapshot-cases.json)
- Мульти-репозиторная агрегация:
  [docs/multi-repository-fixtures.md](docs/multi-repository-fixtures.md),
  [docs/fixtures/multi-repository-cases.json](docs/fixtures/multi-repository-cases.json)

## Проверки

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
python3 docs/fixtures/sqlite/verify.py
git diff --check
```
