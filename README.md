# agent-bridge (Rust)

Rust-переписывание `agent-bridge`: Cargo workspace с доменной моделью
(`bridge-domain`), загрузчиком конфигурации (`bridge-config`), read-only
инспекцией SQLite (`bridge-storage`), primitives политики команд
(`bridge-command-policy`), лексической политики путей (`bridge-path-policy`) и
CLI (`agent-bridge-cli`). Цель —
сохранить контракты CLI, MCP и SQLite, перейти к единому Rust-бинарнику и
добавить GUI на GPUI. Миграция идёт поэтапно, без одномоментной замены Python.
Python и Rust никогда не делят рабочую БД или runtime state: у каждой реализации
свои state root, SQLite, locks, PID/ownership records, token-файлы, логи и
endpoints. Rust всегда создаёт и использует собственную пустую БД и отдельную
историю задач; импорт, копирование или перенос Python state/history в Rust не
поддерживается и не планируется.

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
- Завершён этап **3.9b. Verifier persist-once**: `bridge-storage` предоставляет
  типизированные API на `StorageConnection` — `begin_verifier` и
  `complete_verifier` (типы `CompleteVerifierInput`, `VerifierUpdateOutcome`).
  Оба вызова выполняются в одной `BEGIN IMMEDIATE` транзакции и переиспользуют
  проверку 3.9a (существование task/round, project membership, current round), а
  также согласованность пары `verifier_state`/`verifier_json` через
  `RoundRow::from_row`. `begin_verifier` переводит round без verifier-состояния в
  `running`, идемпотентно обновляет уже `running` и никогда не откатывает
  `done` (возвращает сохранённый результат); `complete_verifier` однократно
  фиксирует `done` + `verifier_json`. Повтор идентичного завершённого результата
  возвращает `Replayed` без записи (Python-семантика reuse), а отличающийся
  результат — fail-closed `VerifierResultConflict` без записей, поэтому
  завершённый результат нельзя молча заменить. Verifier-переходы не пишут
  events, а повреждённая пара/state отвергается fail closed. Cooperative close,
  `reopen_failed_round` и schema/fixtures изменения не входят.
- Завершён этап **3.9c. reopen_failed_round**: `bridge-storage` предоставляет
  типизированный `StorageConnection::reopen_failed_round` и
  `ReopenFailedRoundOutcome` (`Reopened`/`NotEligible`). В одной
  `BEGIN IMMEDIATE` транзакции метод читает task и его current round, маппит обе
  строки через production-контракт и восстанавливает только если
  `task.status=failed`, `close_requested_at` отсутствует, текущий round
  `failed` с `error_code="assistant_error"` и round согласован с task. Round
  переводится `failed->observing`, `error_code` очищается, task возвращается в
  `implementing` (implement) или `revising` (revise), и пишется ровно одно
  событие `reopened` (`failed assistant_error reopened for recovery`) с общим
  timestamp. Неизвестная/неподходящая задача и повторный вызов возвращают
  `NotEligible` без записей (Python no-op), другие error_code, pending
  cooperative close и не-current/несогласованный round не восстанавливаются, а
  повреждённые строки и несогласованная пара task/round отвергаются fail closed
  без частичных записей. Ошибки не раскрывают ids, payload, SQL, JSON и пути.
  Cooperative close не входит.
- Завершён этап **3.9d. Cooperative close** — storage-часть этапа 3.9 закрыта:
  `bridge-storage` предоставляет типизированные `StorageConnection::request_task_close`
  (`RequestTaskCloseOutcome`: `Requested`/`AlreadyRequested`/`UnknownTask`/
  `Terminal`) и `StorageConnection::complete_requested_close`
  (`CompleteRequestedCloseOutcome`: `Closed`/`NoCloseRequest`/`Terminal`/
  `UnknownTask`), а `finish_round` учитывает pending close. В одной
  `BEGIN IMMEDIATE` транзакции `request_task_close` по Python-семантике
  возвращает `UnknownTask` для отсутствующей задачи, `Terminal(status)` для
  `accepted`/`closed`, при первом запросе пишет `close_requested_at`,
  `close_reason` (обрезается до 300 Unicode-символов, как Python `reason[:300]`),
  `updated_at` и ровно одно событие `close_requested` (`round_number=NULL`,
  `task close requested`) с единым timestamp, а повтор возвращает
  `AlreadyRequested` без изменения timestamp/reason и без нового события.
  `complete_requested_close` переводит nonterminal-задачу с запросом в `closed`
  и пишет ровно одно событие `closed` (`round_number=NULL`,
  `task closed: <reason>`; пустой/отсутствующий reason даёт точный fallback
  `requested while worker was running`), а unknown/terminal/без-запроса/повтор
  — типизированный no-op без записей. `finish_round` всегда пишет запрошенные
  round fields/status, но при pending close применяет фактический переход task в
  `closed` (валидируется через `TaskStatus::require_transition`) и пишет событие
  `closed` вместо round-finished, поэтому caller-supplied `task_status` не
  обходит pending close. Paired-записи и события атомарны и используют один
  timestamp, event failure откатывает всё, а ошибки не раскрывают ids, reason,
  response, SQL, JSON и пути. Pending close по-прежнему запрещает
  `reopen_failed_round`. Schema/version/fixtures не менялись.
- Завершён этап **3.10. Storage isolation и создание нового Rust state**:
  `bridge-storage` предоставляет типизированный `RustStateLayout` — storage-level
  контракт одного Rust-owned project state. Layout строится только из явно
  переданного Rust state root и `ProjectId` и выводит все runtime-пути внутри
  этого root: `<root>/<project_id>/state.sqlite`, project-scoped locks
  (`mcp.lock`, `worker.lock`) и root-scoped `runtime.lock`, PID/ownership records
  (`<kind>.process.json`), логи (`mcp.server.log`, `opencode.server.log`,
  `worker.log`), token- и endpoint-файлы. `project_id` и artifact-имена
  проверяются как единый безопасный path component, поэтому путь не может выйти
  за root, а типизированный `StateLayoutError` не раскрывает id, имена и пути.
  `RustStateLayout::initialize` создаёт только собственную новую пустую БД schema
  v6 по derived Rust-пути и идемпотентно сохраняет существующие Rust-rows; API,
  читающего, копирующего или импортирующего Python SQLite/history, нет. Тесты на
  независимых временных Rust/Python roots доказывают изоляцию путей, пустую
  schema v6 без Python rows, неизменность байтов Python state после
  инициализации и Rust-записей и сохранение Rust-rows при повторной
  инициализации. Schema v6, version и fixtures не менялись.
- Завершён этап **3.11. Ownership/format marker и fail-closed guard** —
  storage-поток 3 закрыт. `bridge-storage` помечает Rust-owned state
  versioned sidecar marker `<project_dir>/.agent-bridge-state.json`
  (`implementation="rust"`, `format_version=1`, `project_id`, `state_root`) и
  additive-строкой `meta.runtime_owner='rust'` в schema v6. Поле `state_root` —
  это обязательный namespace-ключ: лексически нормализованный (`.` и повторные
  разделители убираются; `..` гасит только предшествующий обычный компонент, а
  несведённый ведущий `..` сохраняется, поэтому `../a` и `../../a` не
  сталкиваются с `a`) и hex-кодированный из стабильных Unix OS-байтов root
  (`OsStrExt::as_bytes`, явный контракт поддерживаемой платформы Unix/Linux;
  не unspecified `OsStr::as_encoded_bytes`), поэтому скопированный или
  перенесённый под другой Rust root state не совпадает.
  Маркеры выставляются только `RustStateLayout::initialize` для собственного
  нового пустого Rust state; `RustStateLayout::open` до выдачи writable
  `StorageConnection` проверяет sidecar marker, поддерживаемый `format_version`,
  implementation, project namespace, нормализованный `state_root`,
  `meta.runtime_owner` и frozen schema v6 contract. Любое отсутствующее,
  malformed (в том числе без `state_root`), unsupported, foreign или
  противоречивое состояние (missing/foreign marker, namespace или state_root
  mismatch, missing/foreign `meta.runtime_owner`, sidecar/SQLite disagreement,
  несовместимая schema) завершается fail closed без записей, не перезаписывает
  чужой marker/state и не читает, не импортирует и не меняет Python
  state/history. Sidecar пишется crash-safe: приватный уникальный временный файл
  (`create_new`), `sync_all`, no-clobber публикация через hard link без
  перезаписи существующего маркера и синхронизация каталога; порядок «сначала
  marker, затем DB» делает прерванную инициализацию обнаруживаемой и
  восстановимой, а параллельные инициализаторы не делят temp-файл. Generic
  `initialize`/`inspect`, `PRAGMA user_version=6`, DDL и contract fixtures не
  меняются. Типизированный `RustStateError` не раскрывает project id, namespace,
  marker contents, SQL, пути и secrets.
- Завершён этап **4.1. Простые command tokens** — начат поток 4 (security и
  Git). Новый crate `bridge-command-policy` экспортирует узкие primitives
  базовой семантики Python `command_policy.py`: `split_command`
  (POSIX-токенизация, эквивалентная `shlex.split(posix=True)` с
  `comments=False`), `basename`, `is_assignment` и `leading_assignments` с
  last-value-wins для повторяющихся имён. Токенизатор реализован напрямую, а не
  через crate `shlex`, потому что `shlex` трактует `#` как комментарий и
  `\<newline>` как продолжение строки, тогда как Python — нет; NUL отклоняется
  fail closed. Типизированный `TokenizeError` не содержит исходную команду или
  токены. Policy-решения (запрещённые Git writes, wrappers, shell
  metacharacters, globs, permission decision) намеренно не входят в 4.1.
- Завершён этап **4.2. Запрещённые Git writes и wrappers**. Crate
  `bridge-command-policy` переносит token-level семантику reference
  `command_policy.py` (`git_invocation_problem`, `env_invocation_problem`,
  `tokens_problem`): запрещённые `git add`/`commit`/`push` распознаются через
  leading assignments, path-prefixed executable (`/usr/bin/git`, `./git`) и
  wrappers `env`, `sudo`, `command`, `exec`, `nohup`, `nice`, `time`, включая
  вложенность; global options `git` (`-C`/`-c`/`--git-dir`/`--work-tree`/
  `--namespace`/`--exec-path`/`--config-env`/`--super-prefix`/`--shallow-file`)
  пропускаются, glob в позиции подкоманды даёт `unprovable_git_glob`. Узкий
  typed API — `tokens_problem` и `policy_decision` с `PolicyReason`
  (`git_write_blocked`/`unprovable_git_glob`/`unprovable_wrapper_command`) и
  `PolicyDecision`; ни решение, ни причины не несут command text, argv, пути или
  secrets. Fail-closed сверх reference (документированное intentional
  difference): `env` command-splitting отклоняется не только для `-S`/
  `--split-string`, но и для combined short-option cluster (`-iS`) и
  однозначных long-option аббревиатур (`--split`, `--spl`), а неизвестные,
  неоднозначные и value-missing `env` options завершаются
  `unprovable_wrapper_command`; reference пропускает `env -iS 'git push'` и
  `env --split 'git push'`.
- Завершён этап **4.3. Fail-closed compound shell syntax**. Crate
  `bridge-command-policy` добавляет raw-pattern entry point
  `bash_pattern_problem` (и typed `bash_pattern_decision`): raw-строка
  проверяется до токенизации, поэтому неразрешимый shell-синтаксис отклоняется
  fail closed как `unprovable_shell_syntax` даже внутри кавычек — separators
  (`;`/`&&`/`||`), pipes (`|`), background (`&`), redirects (`<`/`>`), command
  substitution (`$(`), backticks, subshells, brace/variable expansion (`$`,
  `${}`, `{}`) и newline/CR. Пустой/whitespace-only вход даёт
  `empty_bash_pattern`, NUL или неразбираемая строка — `unparsable_bash_pattern`.
  `tokens_problem` расширен: general glob даёт `unprovable_glob_command`;
  shell executables (`sh`/`bash`/`zsh`/`dash`/`ksh`/`fish`, включая
  path-prefixed) либо перепроверяются через `-c` (вложенный `tokens_problem`),
  либо отклоняются как `unsafe_shell_invocation` (без `-c`, включая combined
  `-lc`) и `shell_command_missing` (`-c` без строки); `eval` склеивает аргументы
  и заново проверяет их через `bash_pattern_problem`. Новые `PolicyReason`
  (`empty_bash_pattern`/`unprovable_shell_syntax`/`unprovable_glob_command`/
  `unsafe_shell_invocation`/`shell_command_missing`/`unparsable_bash_pattern`)
  не несут command text, argv, пути и secrets. Семантика совпадает с reference,
  включая raw-скан до токенизации и отклонение shell без `-c`; verifier-конверт
  `missing_executable` остаётся задачей 5.1. Тесты 4.1–4.2 не регрессировали.
- Завершён этап **4.4. Workspace-relative allowed paths**. Новый crate
  `bridge-path-policy` реализует только workspace-relative лексическую
  валидацию и нормализацию `allowed_paths` (reference
  `git_snapshot.validate_allowed_paths`) без обращения к файловой системе.
  Пустой список разрешён; для каждого entry сохраняется reference-порядок
  проверок: непустая строка (`invalid_allowed_paths_entry`), backslash
  (`backslash_in_path`), компонент `..` до нормализации (`parent_traversal`),
  сведение к empty/`.` включая `/` и `//` (`empty_or_dot_path`) и абсолютный
  путь (`absolute_workspace_path`). Относительные file/directory scopes
  нормализуются детерминированно, trailing `/` сохраняет различие file и
  directory scope, а missing пути обрабатываются лексически. Узкий typed API —
  `validate_allowed_paths` и boundary-уровень `validate_allowed_path_entries`
  (`AllowedPathEntry::NonText` для non-string), а типизированный
  `PathPolicyReason` не раскрывает входные пути и secrets. Table-driven тесты
  покрывают относящиеся к 4.4 cases/variants из
  `docs/fixtures/path-policy-cases.json`. Symlink confinement (4.5), trusted
  external directories и абсолютные внешние пути (4.6), scope/snapshot
  (4.7–4.9) и MCP-envelope не входят. Существующие crates не менялись.
- Завершён этап **4.5. Symlink confinement**. `bridge-path-policy` расширен
  filesystem-aware слоем поверх лексической семантики 4.4 без изменения
  существующего API. Новые typed-функции `validate_workspace_allowed_paths` и
  `validate_workspace_allowed_path_entries` канонизируют workspace и разрешают
  каждый лексически нормализованный relative scope по семантике Python
  `Path.resolve(strict=False)`: существующие symlink-компоненты (включая
  промежуточные) следуются, `.`/`..` внутри target сворачиваются, missing tail
  присоединяется лексически. Resolved target вне canonical workspace
  отклоняется стабильной категорией `workspace_escape` (существующий и dangling
  symlink escape), а разрешённый entry сохраняет нормализованный исходный
  workspace-relative scope с trailing-slash различием file/directory. Symlink
  loops и I/O/canonicalization failures (неканонизируемый workspace,
  non-`NotFound` metadata-ошибки) завершаются fail closed типизированными
  `SymlinkLoop`/`ResolutionFailure`; ошибки не несут payload и не раскрывают
  пути, workspace, target и OS text. Table-driven тесты воспроизводят четыре
  относящиеся к 4.5 ветви `docs/fixtures/path-policy-cases.json`
  (`validate-relative-symlink-inside-scope`,
  `validate-relative-symlink-escape`, `validate-dangling-symlink-inside-scope`,
  `validate-relative-dangling-symlink-outside`), промежуточные symlink,
  loop/failure и отсутствие утечки; тесты 4.4 не регрессировали. Absolute
  external paths и trusted roots (4.6), scope/snapshot (4.7–4.9) и MCP-envelope
  не входят.
- **Поток 3 завершён, поток 4 продолжается.** Следующий шаг — **4.6**
  (external Git repository paths).
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
