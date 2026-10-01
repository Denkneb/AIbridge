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
- Завершён этап **4.6. External Git repository paths**. `bridge-path-policy`
  расширен trusted-root слоем поверх семантики 4.4–4.5 без изменения
  существующего API. Новые typed-функции
  `validate_allowed_paths_with_trusted_roots` и
  `validate_allowed_path_entries_with_trusted_roots` канонизируют workspace и
  trusted external roots, относительные entries обрабатывают как 4.5, а
  абсолютные разрешают по семантике Python `Path.resolve(strict=False)` и
  проверяют в reference-порядке: путь внутри canonical workspace отклоняется как
  `absolute_workspace_path` до trusted-root lookup, путь вне trusted roots — как
  `outside_trusted_roots` (включая symlink escape), отсутствие Git-репозитория —
  как `not_git_repository`, а repo root вне trusted roots — как
  `external_repo_root_outside_trusted`, поэтому вложенный trusted subdir внутри
  более высокого репозитория не расширяет доверие. Канонический root
  содержащего worktree находится через `git rev-parse --show-toplevel` (missing
  leaf использует существующего предка, repo root и child поддержаны); probe
  ограничен таймаутом с принудительным kill/reap (timeout — fail-closed
  `path_resolution_failure`), а stdout читается как raw Unix path bytes без
  lossy/strict UTF-8, поэтому repo root с non-UTF-8 компонентом распознаётся.
  Нормализованный scope типизирован `ValidatedAllowedPath` (relative остаётся
  относительным, external — каноническим абсолютным, `PathScope` сохраняет
  file/directory различие), а `group_allowed_paths_by_repo` группирует raw
  entries по каноническому repository root без применения trusted roots
  (relative — в main workspace, absolute — в найденный репозиторий, main bucket
  всегда первый). Ошибки payload-free и не раскрывают workspace, roots, пути,
  Git output или OS text. Table-driven тесты покрывают относящиеся к 4.6 cases
  `docs/fixtures/path-policy-cases.json` для `validate_allowed_paths` и
  `group_allowed_paths_by_repo`, repo root/child, missing leaf, symlink escape,
  nested/missing trusted root, boundary non-string и regression 4.4–4.5.
- Завершён этап **4.7. Snapshot основного repository** (подзадачи
  **4.7a. Базовый снимок** и **4.7b. Worktree manifest и
  `worktree_fingerprint`**). Новый workspace-crate `bridge-git` (без зависимостей
  от других bridge-crates) реализует изолированный read-only слой базового
  состояния одного Git worktree. Проверка worktree эквивалентна
  `git rev-parse --is-inside-work-tree`: `false`/nonzero — typed
  `GitError::NotRepository`, а spawn/wait/timeout/I/O и malformed output — fail
  closed инфраструктурная ошибка. `head` (`git rev-parse HEAD`) даёт `None` для
  валидного repository без commit и принимает успешный ответ только как opaque
  ASCII commit id строгой формы (40/64 lowercase hex) без lossy decode.
  `status --porcelain=v1 -z` парсится собственным parser-ом: обычные entries и
  обе стороны rename/copy, пути как Unix `OsString` bytes без lossy `String`,
  malformed porcelain — fail closed, результат сортируется и дедуплицируется.
  `index_fingerprint` — SHA-256 от точных bytes `git ls-files --stage -z`, NUL
  separator, затем `git ls-files -v -z` (SHA-256 реализован в crate и сверен с
  FIPS 180-4 vectors). `RepositorySnapshot` содержит HEAD, raw status/dirty
  paths, index fingerprint, а с 4.7b — ещё manifest и worktree fingerprint.
  Runner ограничен: фиксированный `git`, stdin
  null, stderr отбрасывается, stdout — raw bytes отдельным потоком (нет pipe
  deadlock), wall-clock timeout с kill/reap; shell и Git write-команды
  отсутствуют, а payload-free `GitError` не раскрывает workspace, argv,
  stdout/stderr, Git config, OS errors и secrets. Тесты — только во временных
  synthetic repositories (clean, no commit, dirty tracked/untracked, staged,
  intent-to-add, rename/copy, malformed porcelain, non-repository,
  timeout/kill/reap, non-UTF-8 Unix paths, стабильность index fingerprint).
  4.7b добавляет детерминированный manifest рабочего дерева и стабильный SHA-256
  `worktree_fingerprint`: tracked + non-ignored untracked files, ignored entries
  исключает сам Git, пути — точные Unix bytes без lossy UTF-8, digest учитывает
  content/executable bit/symlink target, а entries упорядочены ровно как Python
  `sorted()` над surrogateescape-decoded именами (code point order: одиночный
  invalid byte `0xff` идёт перед supplementary `U+1F600`, raw-byte порядок был бы
  обратным) с дедупликацией. Fingerprint воспроизводит compact digest reference
  `verifier.fingerprint` (`path || 0x00 || hex digest || 0x00` по отсортированным
  entries, затем `status\0` и raw status) и сверен с Python reference на
  synthetic repository, включая mixed supplementary-Unicode/invalid-byte пути. Fail-closed расширение: файловая ошибка —
  payload-free `ManifestIo`, listed entry не обычный файл/symlink —
  `UnsupportedFileType`. changed/committed paths, history ancestry, scope/policy
  violations и comparison/violations (4.9), worker/MCP integration не входят;
  существующие crates и fixtures не менялись.
- Завершён этап **4.8. Multi-repository snapshots**. `bridge-git` получил модуль
  `multi_repo`, оркестрирующий read-only снимки основного workspace и затронутых
  внешних репозиториев поверх `take_snapshot` (4.7) и типизированных bucket-ов
  `bridge-path-policy::group_allowed_paths_by_repo` (4.6); добавлена локальная
  dependency `bridge-path-policy` (сам crate не менялся). Узкий typed API —
  `take_multi_repository_snapshot(main_workspace, &[AllowedPathGroup])`,
  `MultiRepositorySnapshot`/`RepositoryGroupSnapshot` (`root()`,
  `allowed_paths()`, `snapshot()`) и payload-free `MultiRepoError`. Контракт:
  ровно один snapshot на каждый уникальный canonical repository root; main
  repository всегда первый даже при пустом `allowed_paths`, внешние — в
  детерминированном порядке по canonical root; каждый entry сохраняет canonical
  root, raw allowed entries своего bucket-а и snapshot, поэтому одинаковые
  relative пути в разных репозиториях не смешиваются. Перед snapshot bucket root
  сверяется с фактическим canonical Git worktree root
  (`git rev-parse --show-toplevel` + canonicalize + точное равенство), а
  неканонизируемый/исчезнувший/дублирующийся/не-repository/подменённый или
  вложенный root, как и любая Git/FS infrastructure-ошибка, завершаются fail
  closed типизированной payload-free ошибкой (`workspace_resolution_failed`,
  `repository_resolution_failed`, `missing_main_repository`,
  `main_repository_mismatch`, `duplicate_repository_root`, `not_a_git_repo`,
  `repository_root_mismatch`, `MultiRepoError::Git`). Ошибки не раскрывают roots,
  allowed paths, Git output, argv, OS text и secrets; shell и Git write-команды
  отсутствуют. Тесты во временных synthetic repositories покрывают main-only с
  пустыми allowed paths, main + один/два внешних repository с детерминированным
  порядком, repo root + child в одном snapshot, одинаковый relative filename в
  разных repos без смешивания, независимые dirty/head/index/worktree состояния,
  fail-closed duplicate/root-mismatch/disappeared/non-repository, payload-free
  errors и regression 4.7. Семантика сверена с Python reference multi-repository
  verifier flow и fixtures `docs/fixtures/path-policy-cases.json` /
  `docs/fixtures/git-snapshot-cases.json`. Comparison/violations реализованы
  следующим шагом 4.9; worker/MCP integration не входит.
- Завершён этап **4.9. Snapshot comparison и violations**. `bridge-git`
  предоставляет `compare_repository_snapshot` и
  `compare_multi_repository_snapshot`, типизированные per-repository результаты
  и стабильные policy-коды. Реализованы manifest-based `changed_paths`,
  commit-based `committed_paths`, file/directory scope violations, ancestry,
  `head_changed`/`index_changed`, `allow_commit`, missing main/external repository
  и абсолютная qualification external scope при repository-relative результате.
  Все Git-вызовы read-only и bounded; non-UTF-8 пути сохраняются и сортируются
  по reference surrogateescape-семантике, ошибки payload-free.
- Завершён этап **5.1. Test command validation**. `bridge-command-policy`
  предоставляет `validate_test_commands(commands: &[&str]) ->
  Vec<TestCommandProblem>`: пустой список валиден, каждая команда проверяется
  fail-closed `bash_pattern_problem`, а команда только из ведущих `NAME=value`
  assignments отклоняется стабильной причиной `missing_executable`; typed
  `TestCommandReason`/`TestCommandProblem` несут только индекс и причину без
  command text, argv, путей и secrets.
- Завершён этап **5.2. Один command runner**. Новый workspace-crate
  `bridge-verifier` предоставляет `run_test_command(workspace, command, timeout,
  tail_bytes) -> Result<CommandRunOutcome, CommandRunError>`: fail-closed
  валидация до spawn, argv отдельными OS-аргументами без shell, ведущие
  `NAME=value` assignments в окружении, запуск в заданном `workspace` лидером
  собственной process group, stdin закрыт, bounded stdout/stderr tail только у
  failed-команды, timeout с kill/reap всей process group, payload-free
  `Rejected`/`Spawn`/`Wait`.
- Завершён этап **5.3. Последовательность команд**. `bridge-verifier`
  предоставляет `run_test_command_sequence(workspace, commands, timeout,
  tail_bytes) -> Result<TestCommandSequenceOutcome, CommandSequenceError>` поверх
  `run_test_command`: fail-closed валидация **всего** списка до любого spawn
  (при отклонении любой команды не запускается ни одна), строгий исходный
  порядок, остановка на первом non-zero exit/timeout/spawn failure без запуска
  последующих команд, пустой список даёт пустой успешный результат, результат
  содержит только реально запущенные команды и payload-free `Debug`/`Display`.
- Завершён этап **5.4. HEAD/workspace fingerprints**. `bridge-verifier`
  предоставляет `run_test_command_sequence_fingerprinted(workspace, commands,
  timeout, tail_bytes) -> Result<FingerprintedSequenceOutcome,
  FingerprintedSequenceError>` поверх общего с 5.3 внутреннего loop-примитива
  (публичный контракт 5.3 сохранён) и read-only `bridge_git::take_snapshot`
  (без дублирования Git логики): reference-порядок (fail-closed pre-validation
  всего списка → пустой список без fingerprint → before snapshot → sequence →
  after snapshot даже при non-zero/timeout/spawn failure), typed
  `WorkspaceFingerprint` с доступорами `head()`/`index_fingerprint()`/
  `worktree_fingerprint()`, fail-closed `BeforeSnapshot`
  (`git_fingerprint_failed`) без запуска ни одной команды, spawn/wait failure
  как reference `spawn_failed` entry — сохранённый в outcome typed
  `FingerprintedRunFailure` с предыдущими outcomes и всё равно захваченным
  after, after-failure по reference (команды, run failure и before сохранены,
  `after` отсутствует, `after_snapshot_failed()`, а typed общий статус
  `FingerprintedSequenceStatus` перезаписывается на `git_fingerprint_failed`,
  поэтому `succeeded()` ложен и infra-failure не интерпретируется как успех),
  payload-free ошибки и редактированные `Debug`/`Display` без workspace path,
  command text, Git output и fingerprints.
- Завершён этап **5.5. Side-effect detection** (path-based contract).
  `FingerprintedSequenceOutcome::side_effects() -> Option<WorkspaceSideEffects>`
  возвращает ровно frozen reference `Verification.side_effects`:
  repository-relative пути, созданные или изменённые прогоном, — записи
  worktree manifest (из тех же read-only `bridge_git::take_snapshot`, что дают
  `workspace`-fingerprint), у которых изменился digest, плюс созданные и
  удалённые пути; список отсортирован и дедуплицирован, пути хранятся как
  `OsString` (например `.pytest_cache/v`). Пути не выводятся из агрегатного
  fingerprint digest. Сравнение выполняется независимо от исхода команд
  (non-zero, timeout, recorded spawn/wait failure). Clean non-empty прогон даёт
  пустой `WorkspaceSideEffects` (`Some` без путей), пустой список команд не
  захватывает fingerprints (`side_effects()` `None`), а недоступный after
  fingerprint остаётся infrastructure failure (`side_effects()` `None`,
  `after_snapshot_failed()` и статус `git_fingerprint_failed`,
  `succeeded()==false`) и не мис-классифицируется как clean path result.
  Точные пути доступны только через `paths()`; `Debug`/`Display` outcome и
  side effects редактированы (только число путей) и не раскрывают пути,
  commit id, digest, command text, output и secrets.
- Завершён этап **5.6. Persist-once semantics** (поток 5 закрыт).
  `bridge-verifier` предоставляет узкий typed orchestration API
  `run_round_verification_persisted(storage, round, workspace, commands,
  timeout, tail_bytes) -> Result<PersistedVerification,
  PersistVerificationError>`, который связывает завершённый verifier flow
  (5.1–5.5) с атомарным storage lifecycle 3.9b, не дублируя command,
  fingerprint или storage logic. Порядок reference сохранён точно: сначала
  `begin_verifier` (до любого spawn), затем существующий
  `run_test_command_sequence_fingerprinted` (fail-closed pre-validation всего
  списка → before snapshot → commands → after snapshot), затем однократный
  `complete_verifier`. Если round уже `verifier_state=done`, сохранённая
  `Verification` переиспользуется и **ни одна** test command не запускается
  (`PersistedVerificationOutcome::Reused`). Для нового или `running` verifier
  outcome конвертируется в компактный `bridge_domain::Verification`
  (`passed`/`failed`/`timed_out` для прогона с per-command
  `duration`/`exit_code`/`timed_out`/`output_tail`/`reason`, `unsafe` с
  `index`/`reason` для отклонённого списка и `error` с
  `git_fingerprint_failed` для упавшего fingerprint — причём before-snapshot
  failure не сохраняет ни команд, ни fingerprint (fail closed до любого
  spawn), а after-snapshot failure сохраняет фактически выполненные команды
  (включая recorded `spawn_failed`/`wait_failed` entry) и `before`, оставляя
  `after`/`side_effects` отсутствующими и перезаписывая лишь общий статус;
  `before`/`after` — compact triple из того же snapshot, `side_effects` —
  отсортированные пути без пустого ключа, `log` — frozen reference
  `verification/<task_id>/round_<round_number>`). Первый результат
  фиксируется ровно один раз: идентичный уже сохранённый результат даёт
  `Replayed` без записи, отличающийся завершается fail closed typed
  `PersistVerificationError::Conflict` (`verifier_result_conflict`) без
  перезаписи, storage failures дают `Storage`, а отсутствие persisted
  verification — `InvalidState`. `Debug`/`Display` редактированы и не
  раскрывают task/project ids, команды, пути, output, fingerprints, SQL и
  secrets. Focused tests покрывают первый запуск и persistence, `done` reuse
  без spawn, идемпотентный повтор, recovery из `running`, typed conflict
  mapping, storage/verifier failures, статусы `unsafe`/`error`/`timed_out`/
  `failed`/`spawn_failed`, сохранение команд/`before` при after-snapshot failure
  (в том числе вместе со `spawn_failed` entry), persistence side effects и
  redaction.
- Завершён этап **6.1. HTTP transport и basic auth**. Новый workspace-crate
  `bridge-opencode` реализует минимальный переиспользуемый HTTP/1.1 transport к
  уже валидированному OpenCode endpoint поверх типизированных значений
  `bridge-config` (`Endpoint`/`Secret`): конфигурационная валидация не
  дублируется. `BasicAuth::new(Secret)` формирует
  `Authorization: Basic base64("opencode:<password>")` с константным username
  `opencode`, как reference `credentials.USERNAME`; `BasicAuth::from_project`
  переиспользует `ProjectEntry::read_password` и отображает ошибку чтения
  credential в `TransportError::InvalidAuth`. `HttpTransport::new(endpoint,
  auth, timeout)` и `request(&HttpRequest)` выполняют один запрос по свежему
  `TcpStream` с `Connection: close`, ограниченный общим deadline
  (`DEFAULT_TIMEOUT = 30s`, как reference). Deadline охватывает connect, запись
  и чтение: перед каждой частичной записью и каждым чтением remaining
  пересчитывается, поэтому медленно читающий или вовсе не читающий peer не
  продлевает запрос; `Ok(0)` (WriteZero) трактуется как разрыв сокета, а
  `Interrupted` повторяется под тем же deadline, и успешный `2xx` не
  возвращается после истечения deadline. Тело ответа фреймится по
  `Transfer-Encoding: chunked`/`Content-Length`/EOF; успешный `204` без
  `Content-Length` распознаётся как bodyless до framing и возвращает пустой
  body, не дожидаясь закрытия соединения, а `304` остаётся non-success ошибкой
  `HttpStatus(304)` и его body не читается. Статус-строка парсится строго:
  поддерживаются только `HTTP/1.0`/`HTTP/1.1`, код — ровно три ASCII-цифры в
  диапазоне `100..=599`, иначе `TransportError::Protocol`. Типизированный `TransportError` различает
  timeout, connection/transport failure, invalid auth setup, HTTP 401
  (`Unauthorized`), 404 (`NotFound`), прочие non-success (`HttpStatus`),
  malformed request и malformed response; успехом считается только `2xx`, тело
  non-success ответа не читается. Пароль, `Authorization`, path/query, тело
  ответа, workspace-пути и OS-детали редактированы в `Debug`/`Display` и
  публичных типах. OpenAPI compatibility (6.3), session/message APIs (6.4+),
  worker/MCP wiring и public API других crates не входят. Focused tests с
  локальным loopback mock server (без внешней сети) покрывают Base64-векторы,
  точный Basic-header, `InvalidAuth`, GET/POST construction, timeout, connection
  failure, mapping статусов, chunked/EOF body, `204` без `Content-Length` при
  открытом соединении, большой POST к медленному/не читающему peer в пределах
  deadline, детерминированный write-loop тест на остановку по общему deadline
  при успешных частичных записях, strict status parser, malformed
  request/response, zero-timeout fail-closed и redaction.
- Завершён этап **6.2. Health и workspace identity**. Типизированный
  `OpenCodeClient` связывает transport 6.1 с одним каноническим workspace.
  `OpenCodeClient::from_project(&ProjectEntry, timeout)` берёт endpoint,
  credential и workspace из уже валидированного `bridge-config`;
  `health()` отправляет `GET /global/health?directory=<workspace>` (reference
  scoped-запрос) и считает сервер здоровым только при литеральном boolean
  `true` в `healthy`, как reference `health().get("healthy") is True`
  (отсутствующее/небулево значение — не здоров; не-объект/невалидный JSON —
  `HealthError::Malformed`). `verify_workspace()` отправляет `GET /path`
  **без** `directory`: scoped `/path` лишь отражает workspace вызывающего,
  поэтому identity доказывается только собственным root сервера. Поле
  `directory` извлекается типизированно (отсутствие —
  `IdentityError::MissingDirectory`, не-строка — `Malformed`) и разрешается по
  семантике Python `Path.resolve(strict=False)`: существующие symlink-компоненты
  следуются (абсолютный target перезапускает разрешение от корня FS,
  относительный разрешается от родителя ссылки, `.`/`..` внутри target
  сворачиваются), а отсутствующий компонент присоединяется без остановки обхода,
  поэтому `..` после missing всё ещё сворачивается относительно
  symlink-разрешённого префикса. Лексического shortcut нет: reported принимается
  только когда его resolved-форма равна каноническому workspace, даже если он
  лишь лексически «складывается» в него. Несовпадение, отсутствующие/некорректные
  поля, невалидный JSON, встроенный NUL (Python `ValueError`), symlink loop и
  любая non-`NotFound` FS-ошибка (permission/not-a-directory/нечитаемая ссылка)
  дают fail-closed (`IdentityError::Mismatch`/`Malformed`/`MissingDirectory`).
  Транспортные
  сбои сохраняются (`HealthError::Transport`/`IdentityError::Transport`:
  timeout, unavailable, 401, 404, прочие HTTP, protocol). `OpenCodeClient` не
  рендерит workspace, `Health` — `version`, а новые ошибки — только static
  label и вложенный `TransportError`. Focused loopback tests покрывают успешные
  health/identity, игнорирование scoped echo, чужие root, malformed/missing
  поля, правила сравнения пути (trailing slash, `.`/`..`, symlink alias,
  regression symlink+missing+`../..` false-positive, `..` после symlink,
  escaped NUL), HTTP/auth failure, timeout/unavailable и redaction.
- Завершён этап **6.3. OpenAPI compatibility**. `OpenCodeClient::
  check_compatibility()` отправляет `GET /doc?directory=<workspace>` (reference
  `get_doc` использует `scoped=True`) и возвращает типизированный
  `DocCompatibility`; HTTP/auth/timeout/protocol категории сохраняются как
  `DocError::Transport`, невалидный JSON — `DocError::Malformed`, а валидный
  JSON-не-объект — несовместимость `DocumentNotObject`. Чистая
  `openapi_problems(&Value, require_prompt_model)` повторяет reference
  `opencode_client.py::openapi_problems`: обязательные routes/methods
  (`GET`/`POST /session`, `GET /session/status`, `GET /permission`, `POST
  /permission/{}/reply`, `GET /question`, health/path/session message/
  prompt_async), нормализация имён path-параметров (`{id}`/`{sessionID}` → `{}`),
  обязательные schema properties `Path`/`Session`/`AssistantMessage` и вложенный
  `AssistantMessage.time.completed`, JSON `prompt_async` body
  (`messageID`/`parts`, и `model` только при `require_prompt_model=true`),
  permission-reply body/поле `reply`/enum `once`/`always`/`reject`. Локальные
  `$ref` chains разрешаются с защитой от циклов; unresolvable ref (non-string,
  external, cyclic, broken) даёт пустой узел, поэтому sibling-properties не
  доказывают обязательную структуру, а non-object `time.properties` также fail
  closed. Конфликт двух spelling'ов, нормализующихся в один path, разрешается
  first-wins в порядке JSON insertion order (`serde_json` собран с feature
  `preserve_order`), как reference `setdefault`, поэтому конфликт не даёт
  ложного `compatible`. Malformed JSON/не-объект/неверные nested types/пустые
  paths/loops завершаются fail closed без panic и ложного `compatible`.
  `OpenCodeClient::from_project` выводит `require_prompt_model` из
  `ProjectEntry::opencode_model`, поэтому project без model совместим с
  документом без `model`, а project с model отвергает тот же документ; это
  проверка наличия поля `model`, а не конкретной provider/model на сервере.
  `CompatibilityProblem` несёт только стабильные категории и имена обязательных
  контрактных полей; произвольные path/schema/`$ref`/doc-значения, credentials,
  workspace и query не утекают в `Debug`/`Display`. Focused loopback tests
  покрывают reference fixtures, missing routes/wrong methods, path renaming,
  first-wins конфликт нормализованных paths в обеих очередностях raw JSON (pure
  и loopback), required properties/time.completed (включая non-object
  `properties`), inline/chained refs, cyclic/broken/external/non-string refs с
  sibling-properties (включая body/time/permission schemas), malformed nested
  containers, prompt body/conditional model, permission schema/enum, end-to-end
  `GET /doc` (точный scoped query), compatible/incompatible/malformed/non-object,
  HTTP/auth errors и redaction; тесты 6.1–6.2 не регрессировали.
- Завершён этап **6.4. Session create/list/get**. `OpenCodeClient` получил три
  типизированные session-операции reference `opencode_client.py`, все scoped с
  `directory`-query и через общий transport с Basic auth:
  `list_sessions()` → `GET /session` (список), `create_session(title)` →
  `POST /session` с компактным JSON `{"title": <title>}` (как httpx,
  UTF-8 без ASCII-escaping), `get_session(id)` → `GET /session/<id>`. Session id
  кодируется как один RFC 3986 path-сегмент (percent-encoding); для обычных
  alphanumeric `ses...` id это байт-в-байт no-op, но `%2F`/`%3F`/`%23`/пробел и
  любые другие байты (включая literal `%` и Unicode) больше не могут изменить
  request target или внедрить query. Transport принимает percent-encoded пути
  только как полный triplet `%` HEXDIG HEXDIG: bare/truncated/non-hex escapes
  (`%`, `%2`, `%GG`) отклоняются как `TransportError::InvalidRequest` до
  открытия сокета, а raw whitespace/`?`/`#`/control остаются запрещены. Пустой
  id и голые dot-сегменты `.`/`..` отклоняются fail closed как
  `SessionError::InvalidSessionId` до отправки — это единственное намеренное
  отклонение от reference, который подставляет id дословно. Типизированный
  `Session` отдаёт `id`/`title`/`directory` как `Option<&str>` (пермиссивно, как
  reference `session.get(...)`); невалидный JSON или неверный top-level shape
  (не объект для create/get, не массив для list) — `SessionError::Malformed`, а
  не-объектный элемент списка пропускается ровно как reference
  `isinstance(session, dict)`. Транспортные категории (timeout, unavailable, 401,
  404, прочие HTTP, protocol) сохраняются как `SessionError::Transport`.
  `Session`/`SessionError` редактированы: id/title/directory, workspace,
  credential и тела ответов не попадают в `Debug`/`Display`. Focused loopback
  tests используют mock server с управляемым lifecycle (RAII-хэндл
  останавливает accept-loop и join'ит поток при drop, без внешних сервисов) и
  покрывают успешные list/create/get с точным request (method/path/query/body/
  auth), percent-encoding id, literal `%`/Unicode id, verbatim plain id,
  fail-closed unusable id, приём валидных encoded путей и отклонение
  malformed percent escapes до отправки, transport errors, malformed
  list/create/get и redaction; тесты 6.1–6.3 не регрессировали.
- Завершён этап **6.5. Message list и parsing**. `OpenCodeClient` получил
  `list_messages(session_id)` → `GET /session/<id>/message`, scoped с
  `directory`-query и существующим Basic auth, как reference
  `opencode_client.py::list_messages`. Session id валидируется и
  percent-encode'ится общим с 6.4 механизмом: пустой id и голые `.`/`..`
  отклоняются до отправки как `MessageError::InvalidSessionId`, literal `%`,
  `/`, `?`, `#`, пробел и Unicode не меняют request target, а действующая
  percent-triplet validation transport не ослаблена. Типизированный `Message`
  (`MessageInfo` + `Vec<MessagePart>`) переносит parsing, который читают
  reference consumers (`worker.py`, `usage.py`), не реализуя worker:
  identity (`id`/`role`/`parentID`/`sessionID`), assistant lifecycle
  (`time.completed` присутствует и не `null`; `finish`; truthy `error`),
  provider/model (`model()` требует непустые `providerID`/`modelID`) и
  нормализованный token/cost `Usage` (`_number`: только JSON-числа,
  missing/bool/negative/non-finite → `0.0`). `MessagePart` отдаёт text
  (`type`/`text`/`ignored`) и tool lifecycle (`tool`, `state.status`,
  `state.error`, `metadata.providerExecuted`, `state.metadata.interrupted`);
  `Message::text()` повторяет reference `_text_of` (join непустых не-ignored
  text-частей через `\n` и Python `str.strip()` whitespace, т.е. Rust
  `is_whitespace` плюс U+001C..U+001F), `Message::has_pending_tool_parts()` —
  reference `_has_tool_parts`. Parsing пермиссивен как `message.get(...)` для
  скалярных полей (отсутствующее/неверно типизированное → `None`/`false`/zero)
  и для отсутствующих `info`/`parts` (reference defaults `{}`/`[]`), но
  present-но-неверно-типизированные lifecycle-контейнеры и элементы
  message/parts отклоняются fail-closed как `MessageError::Malformed`:
  не-объектный message, не-объектный `info`, не-массив `parts`, не-объектная
  часть, truthy не-объектные `time`/`state`/`metadata`/`state.metadata` и
  truthy не-string `text` text-части (reference упал бы с `AttributeError`/
  `TypeError`). Поэтому malformed структура не вызывает panic, не даёт ложного
  `completed`/успеха и не может спрятать более позднюю незавершённую запись за
  старой завершённой; невалидный JSON или не-массив top-level — тоже
  `MessageError::Malformed`. Транспортные категории
  (timeout, unavailable, 401, 404, прочие HTTP, protocol) сохраняются как
  `MessageError::Transport`. `Message`/`MessageInfo`/`MessagePart` рендерят
  только presence/lifecycle флаги и счётчики, `Usage` — accounting-числа,
  `MessageError` — static label; credential, workspace, id, text, error и тела
  ответов не утекают. Focused loopback tests (mock server с управляемым
  lifecycle) покрывают точный method/path/query/auth, reference fixture,
  percent-encoding и fail-closed id без запроса, transport errors, malformed
  body, fail-closed malformed message/part/lifecycle (в т.ч. завершённый
  assistant с повреждёнными parts и malformed trailing entry), lifecycle/
  completion/error, usage/model normalization, Python-whitespace text и
  tool-part lifecycle, redaction; тесты 6.1–6.4 не регрессировали.
- Завершён этап **6.6. Async prompt delivery**. `OpenCodeClient` получил
  `send_prompt_async(session_id, message_id, text)` →
  `POST /session/<id>/prompt_async`, scoped с `directory`-query и существующим
  Basic auth/transport, как reference
  `opencode_client.py::send_prompt_async`. Session id валидируется и
  percent-encode'ится общим с 6.4/6.5 механизмом: пустой id и голые `.`/`..`
  отклоняются до отправки как `PromptError::InvalidSessionId`, literal `%`,
  `/`, `?`, `#`, пробел и Unicode не меняют request target, а действующая
  percent-triplet validation transport не ослаблена. Тело запроса — ровно
  компактный UTF-8 JSON reference httpx (`ensure_ascii=False`,
  `separators=(",", ":")`):
  `{"messageID": <id>, "parts": [{"type": "text", "text": <text>}]}`, а
  `"model": {"providerID": ..., "modelID": ...}` добавляется последним полем
  только когда клиент построен из проекта с валидированным
  `ProjectEntry::opencode_model` (`from_project`); клиент без модели не
  отправляет `model` вовсе, как reference `config.opencode_model is None`.
   `messageID` и `text` едут внутри JSON и экранируются сериализатором
   (UTF-8 без ASCII-escaping), а не подставляются в path. Успех — любой `2xx`,
   включая bodyless `204`; транспорт читает и фреймит тело успешного ответа по
   HTTP framing (кроме bodyless `204`) прежде чем вернуть `HttpResponse` —
   ровно как reference `_request`, где httpx тоже читает тело, — но
   `send_prompt_async` не интерпретирует и не парсит его как JSON (reference
   `_request`, не `_json`), поэтому у `PromptError` нет `Malformed`-варианта.
   Reference `_request` принимает любой статус `< 400`, включая `3xx`, а
   существующий transport сохраняет политику только `2xx`, поэтому `3xx`
   намеренно возвращается как `TransportError::HttpStatus` (политика transport
   ради документации не ослабляется); транспортные категории (timeout,
   unavailable, 401, 404, прочие HTTP, protocol) сохраняются как
   `PromptError::Transport`. Успешный
  вызов означает только принятие POST и намеренно не утверждает завершение
  assistant-хода; автоматического retry неидемпотентной отправки нет, а
  транспортная ошибка после записи запроса оставляет исход доставки
  неопределённым, а не гарантирует отсутствие отправки. Redaction соблюдён:
  `OpenCodeClient` не рендерит workspace и model, `PromptError` — только static
  label и вложенный `TransportError`; session/message id, text, model, body,
  credential и `Authorization` не утекают в `Debug`/`Display`. `Cargo.toml`/
   `Cargo.lock`, schema и fixtures не менялись. Focused loopback tests (mock
   server с управляемым lifecycle `ServerCapture` stop/join, без внешней сети)
   покрывают точный method/path/query/auth/body без модели, добавление model из
   валидированного project entry последним полем, UTF-8 и JSON-escaping,
   bodyless `204`, игнорирование 2xx-тела, percent-encoding id, fail-closed
   unusable id и zero-timeout без отправки, transport errors 401/404/500, 3xx
   как `HttpStatus` (намеренное отклонение), timeout и unavailable, а также
   неопределённый исход после полной отправки (mock получает и фиксирует POST
   body, затем не отвечает до истечения client deadline: ожидается
   `TransportError::Timeout`, ровно один POST, без retry), redaction; тесты
   6.1–6.5 не регрессировали.
- Завершён этап **6.7. Permissions list/reply**. `OpenCodeClient` получил
  `list_permissions()` → `GET /permission` и
  `reply_permission(request_id, reply, message)` → `POST /permission/<id>/reply`,
  оба scoped с `directory`-query и существующим Basic auth, как reference
  `opencode_client.py::list_permissions`/`reply_permission`. `reply`
  типизирован enum `PermissionReply::{Once,Always,Reject}` (`once`/`always`/
  `reject`), поэтому unsupported reply неотправляем (reference бросает
  `OpenCodeError`). Тело reply — ровно компактный UTF-8 JSON reference httpx:
  `{"reply": <reply>}`, а `"message"` добавляется последним полем только когда
  message передан; отсутствующий message (`None`) не отправляет поле, а пустой
  (`Some("")`) отправляет `"message": ""`, сохраняя различие reference
  `message is not None`. Request id валидируется и percent-encode'ится как один
  RFC 3986 path-сегмент общим с 6.4–6.6 `encode_session_segment`/
  `encode_path_segment`, пустой id и голые `.`/`..` отклоняются до отправки как
  `PermissionError::InvalidRequestId`, literal `%`, `/`, `?`, `#`, пробел и
  Unicode не меняют request target, а percent-triplet validation transport не
  ослаблена (намеренное fail-closed hardening над reference, который
  подставляет id дословно). `reply_permission` следует reference `_request`
  (не `_json`): успех — любой `2xx`, включая bodyless `204`, тело успешного
  ответа фреймится транспортом, но не парсится как JSON, поэтому для reply нет
  `Malformed`; reference `_request` принимает `< 400` включая `3xx`, тогда как
  transport сохраняет политику только `2xx`, поэтому `3xx` — намеренно
  `TransportError::HttpStatus`. Автоматического retry нет, а транспортная ошибка
  после записи оставляет исход reply неопределённым. Типизированный
  `Permission` сохраняет реальные reference поля `PermissionRequest` (OpenCode
  SDK): `id`/`sessionID`/`permission` (`Option<String>`), `patterns`/`always`
  (`Vec<String>`), `metadata` (raw JSON только за явным
  `Permission::metadata()` accessor, `[redacted]` в rendering) и optional
  `tool: {messageID, callID}` (`PermissionTool`), которые читают будущие
  permission consumers, без реализации consumers. Parsing явно разграничивает
  required SDK-поля, absent/null optional defaults и malformed: `id`,
  `sessionID`, `permission` и `patterns` обязательны по SDK-shape, поэтому их
  отсутствие/`null`/неверный тип → `PermissionError::Malformed` (не `None` и не
  пустой список), чтобы повреждённый pending request нельзя было принять за
  отсутствующий (например, молча отфильтровать по `sessionID`) или разрешить;
  валидный пустой `patterns: []` остаётся допустимым и отличимым. Необязательные
  `always`/`metadata`/`tool` сохраняют reference defaults: absent/`null` →
  пусто/пустой объект/`None`. Present-но-неверно-типизированное значение
  (включая required identity/patterns), не-объектный элемент массива и
  не-массив top-level → `PermissionError::Malformed`, поэтому повреждённый
  request не пропускается молча и список не может выглядеть пустым или
  разрешённым. Транспортные
  категории (timeout, unavailable, 401, 404, прочие HTTP, protocol)
  сохраняются как `PermissionError::Transport`. Redaction соблюдён:
  `Permission`/`PermissionTool` рендерят только presence-флаги и счётчики,
  `metadata` — `[redacted]`, `PermissionError` — static label и вложенный
  `TransportError`; credential, `Authorization`, workspace, id, sessionID,
  permission name, patterns, commands, metadata, tool ids, message и тела
  ответов не утекают в `Debug`/`Display`. `Cargo.toml`/`Cargo.lock`, schema и
  fixtures не менялись. Focused loopback tests (mock server с управляемым
  lifecycle `ServerCapture` stop/join, без внешней сети) покрывают точный
  method/path/scoped query/auth для обоих endpoints, reference-compatible
  `PermissionRequest` fixture, валидный пустой `patterns: []` и optional
  defaults, отсутствие/`null` required identity (`id`/`sessionID`/`permission`)
  и `patterns`, malformed request между двумя валидными (вся операция
  `Malformed`), malformed top-level/element/полей, transport errors, каждый
  `once`/`always`/`reject`,
  optional message (отсутствующий vs пустой), compact UTF-8 body,
  percent-encoding и fail-closed unusable id без отправки, bodyless `204` и
  игнорирование 2xx-тела, `3xx` как `HttpStatus`, protocol error, а также
  неопределённый исход после полной отправки reply (mock получает и фиксирует
  POST body, затем не отвечает до истечения client deadline: Timeout, ровно
  один POST, без retry) и redaction; тесты 6.1–6.6 не регрессировали.
- **Потоки 3, 4 и 5 завершены.** Шаги **5.1–5.6**, **6.1**, **6.2**, **6.3**,
  **6.4**, **6.5**, **6.6** и **6.7** завершены, следующий незавершённый шаг —
  **6.8** (Questions и blockers).
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
