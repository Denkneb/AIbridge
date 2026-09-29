# План реализации

## Правила декомпозиции

План выполняется короткими задачами. Одна задача:

- меняет один компонент или один сквозной контракт;
- обычно затрагивает не более 3–5 логически связанных файлов;
- имеет один основной критерий готовности;
- содержит собственный короткий контекст;
- завершается targeted tests, а не полной матрицей без необходимости;
- не смешивает рефакторинг, новую возможность и миграцию данных;
- не меняет schema, public API и security policy одновременно.

Если задача содержит два независимых результата, её следует разделить.

## Шаблон задачи

```text
Название:
Цель:
Контекст: только нужные документы и fixtures
Разрешённая область: точные каталоги/файлы
Не входит в задачу:
Критерии приёмки:
Targeted checks:
Зависит от:
Открывает следующие задачи:
```

Контрактные fixtures и короткие ADR должны быть самодостаточными. Следующая
задача читает результат предыдущей, а не историю её обсуждения.

## Поток 0. Контрактная база

### 0.1. Manifest внешнего контракта

Перечислить CLI-команды, MCP tools, статусы, SQLite version и runtime paths.

### 0.2. Fixtures конфигурации

Зафиксировать valid/invalid `projects.toml` и категории ошибок.

### 0.3. Fixtures MCP

Сохранить ответы tools для значимых статусов, включая compact/verbose status.

### 0.4. Fixtures SQLite

Подготовить небольшие БД для empty, active, review и terminal states.

### 0.5. Security corpus

Выделить command, path, symlink и Git policy cases с allow/deny результатом.

**Готовность потока:** fixtures воспроизводятся текущей Python suite.

## Поток 1. Rust foundation

### 1.1. Cargo workspace skeleton

Workspace, минимальные crates, toolchain и CI format/clippy/test.

### 1.2. Общие error types

Типизированные ошибки и безопасное пользовательское представление.

### 1.3. Идентификаторы и статусы

`ProjectId`, `TaskId`, `TaskStatus`, parsing и serialization.

### 1.4. Переходы Task

Table-driven реализация допустимых и запрещённых transitions без SQLite.

### 1.5. Round и Verification

Data model и serde compatibility без runtime logic.

## Поток 2. Конфигурация

### 2.1. Базовый TOML loader

### 2.2. Project ID и workspace validation

### 2.3. Endpoint и port validation

### 2.4. Уникальность workspace/endpoints/token files

### 2.5. Max rounds, model и optional paths

### 2.6. Auto-approve permissions

### 2.7. Trusted external directories

### 2.8. Credentials reader

### 2.9. Project env reader

Каждая задача переносит только указанную группу правил и её fixtures.

**Готовность потока:** весь config corpus совпадает с Python семантически.

## Поток 3. SQLite storage

### 3.1. Read-only schema inspection

### 3.2. WAL, foreign keys и busy timeout

### 3.3. Task row mapping

### 3.4. Round row mapping

### 3.5. Initialization совместимой пустой БД

### 3.6. Get/list/pagination queries

### 3.7. Atomic task creation

### 3.8. Request idempotency

### 3.9. Atomic round/verifier updates

- **3.9a. Atomic round/task lifecycle updates — завершено.** Типизированные
  lifecycle-API на `StorageConnection` (`create_revision_round`,
  `bind_round_session`, `prepare_round`, `mark_round_sent`,
  `mark_round_observing`, `mark_worker_started`, `finish_round`): каждая
  операция в одной `BEGIN IMMEDIATE` транзакции, проверка round-переходов по
  `ROUND_TRANSITIONS` и task-переходов через `TaskStatus::require_transition`.
- **3.9b. Verifier persist-once — завершено.** `begin_verifier` и
  `complete_verifier` атомарно ведут verifier lifecycle/result: `running` с
  идемпотентным обновлением и без отката `done`, однократная фиксация `done` +
  `verifier_json`, replay идентичного завершённого результата без записи
  (Python-семантика reuse) и fail-closed `VerifierResultConflict` для
  отличающегося результата. Проверяются task/round, project membership,
  current round и пара `verifier_state`/`verifier_json`; events не пишутся.
- **3.9c. reopen_failed_round — завершено.** Типизированный
  `StorageConnection::reopen_failed_round` и `ReopenFailedRoundOutcome`
  (`Reopened`/`NotEligible`). В одной `BEGIN IMMEDIATE` транзакции читаются task
  и его current round, обе строки маппятся через production-контракт, и
  восстановление выполняется только при `task.status=failed`, отсутствии
  `close_requested_at`, `round.status=failed` с
  `error_code=RECOVERABLE_FAILED_ERROR_CODE` (`assistant_error`) и
  согласованности round с task. Round атомарно переводится `failed->observing`,
  `error_code` очищается, task возвращается в `implementing` (implement) или
  `revising` (revise), и пишется ровно одно событие `reopened`
  (`failed assistant_error reopened for recovery`) с общим timestamp.
  Неизвестная/неподходящая задача и повторный вызов дают `NotEligible` без
  записей (Python no-op); другие error_code, pending cooperative close,
  не-current/несогласованный round и повреждённые строки не восстанавливаются,
  а некорректный persisted state отвергается fail closed без частичных записей.
- **3.9d. Cooperative close — завершено.** Типизированные
  `StorageConnection::request_task_close` (`RequestTaskCloseOutcome`:
  `Requested`/`AlreadyRequested`/`UnknownTask`/`Terminal`) и
  `StorageConnection::complete_requested_close`
  (`CompleteRequestedCloseOutcome`: `Closed`/`NoCloseRequest`/`Terminal`/
  `UnknownTask`), а `finish_round` учитывает pending close. В одной
  `BEGIN IMMEDIATE` транзакции `request_task_close` по Python-семантике даёт
  `UnknownTask` для отсутствующей задачи, `Terminal(status)` для
  `accepted`/`closed`, при первом запросе сохраняет `close_requested_at`,
  `close_reason` (до 300 Unicode-символов, как Python `reason[:300]`),
  `updated_at` и ровно одно событие `close_requested` (`round_number=NULL`,
  `task close requested`) с единым timestamp, а повтор возвращает
  `AlreadyRequested` без изменения timestamp/reason и без нового события.
  `complete_requested_close` переводит nonterminal-задачу с запросом в `closed`
  и пишет ровно одно событие `closed` (`round_number=NULL`,
  `task closed: <reason>`; пустой/отсутствующий reason использует точный
  fallback `requested while worker was running`), а unknown/terminal/без-запроса
  и повтор — типизированный no-op без записей. `finish_round` всегда пишет
  запрошенные round fields/status, но при pending close применяет фактический
  переход task в `closed` (валидируется через `TaskStatus::require_transition`)
  и пишет событие `closed` вместо round-finished, поэтому caller-supplied
  `task_status` не обходит pending close. Paired-записи и события атомарны и
  используют один timestamp, event failure откатывает всю транзакцию, а ошибки
  не раскрывают ids, reason, response, SQL, JSON и пути. Pending close
  по-прежнему запрещает `reopen_failed_round`. Schema/version/fixtures не
  менялись.
- **Этап 3.9 завершён.**

### 3.10. Storage isolation и создание нового Rust state

- **Завершено.** `bridge-storage` предоставляет типизированный storage-level
  контракт `RustStateLayout` для одного Rust-owned project state. Layout
  строится только из явно переданного Rust state root и `ProjectId` и выводит
  все runtime-пути внутри этого root: `<root>/<project_id>/state.sqlite`,
  project-scoped locks `mcp.lock`/`worker.lock` и root-scoped `runtime.lock`,
  PID/ownership records `<kind>.process.json`, логи `mcp.server.log`/
  `opencode.server.log`/`worker.log`, token-файлы и endpoint-файлы. `project_id`
  и artifact-имена валидируются как единый безопасный path component, поэтому
  пути не могут выйти за root; типизированный `StateLayoutError` не раскрывает
  id, имена и пути. `RustStateLayout::initialize` создаёт только собственную
  новую пустую БД schema v6 по derived Rust-пути и идемпотентно сохраняет
  существующие Rust-rows; API, читающего, копирующего или импортирующего Python
  SQLite/history, нет. Тесты на независимых временных Rust/Python roots
  доказывают, что все Rust runtime paths лежат внутри Rust root и не
  пересекаются с Python root, новая БД пуста (schema v6, без Python
  task/history rows), инициализация и Rust-записи не меняют байты Python state,
  а повторная инициализация сохраняет Rust-rows. Schema v6, version и fixtures
  не менялись.
- **Этап 3.10 завершён.**

### 3.11. Ownership/format marker и fail-closed guard

- **Завершено.** `bridge-storage` расширяет `RustStateLayout` ownership/format
  marker-ами поверх завершённого Rust state. Rust-owned state получает
  versioned sidecar marker `<project_dir>/.agent-bridge-state.json`
  (`implementation="rust"`, `format_version=1`, `project_id`, `state_root`) и
  additive-строку `meta.runtime_owner='rust'`; `PRAGMA user_version=6`,
  DDL/tables/columns/indexes/foreign keys, generic `inspect`/`initialize`
  compatibility и contract fixtures не меняются. Обязательный `state_root` —
  это namespace-ключ root: путь лексически нормализуется (`.` и повторные
  разделители убираются; `..` гасит только предшествующий обычный компонент, а
  несведённый ведущий `..` сохраняется, поэтому `../a` и `../../a` не
  сталкиваются с `a`; без обращения к FS и без следования symlink) и
  hex-кодируется из стабильных Unix OS-байтов (`OsStrExt::as_bytes`, явный
  контракт поддерживаемой платформы Unix/Linux; не unspecified
  `OsStr::as_encoded_bytes`), так что ключ стабилен как on-disk interchange,
  lossless для non-UTF-8 и не сталкивается у разных root. Оба маркера
  выставляются только
  `RustStateLayout::initialize` для собственного нового пустого Rust state;
  Python state/history не читается, не импортируется и не модифицируется.
  Новый production API `RustStateLayout::open` до выдачи writable
  `StorageConnection` обязан сверить sidecar marker, поддерживаемый
  `format_version`, implementation, project namespace, нормализованный
  `state_root`, `meta.runtime_owner` и существующий schema v6 contract.
  Отсутствующий, malformed (включая marker без `state_root`), unsupported,
  foreign или противоречивый marker, чужой `runtime_owner`, mismatch namespace
  или `state_root` и несовместимая schema завершаются fail closed без записи;
  при каждом отказе байты/содержимое чужого state и marker не меняются, а
  скопированный под другой Rust root state отклоняется. Порядок создания
  crash-safe: marker пишется первым в приватный уникальный временный файл
  (`create_new`, `sync_all`), публикуется no-clobber через hard link без
  перезаписи существующего маркера с синхронизацией каталога, затем создаётся
  schema v6 с `meta.runtime_owner` в одной `BEGIN IMMEDIATE` транзакции;
  прерванная инициализация (marker без DB) восстанавливается повторным
  `initialize`, а DB без marker или без `meta.runtime_owner` никогда не
  принимается как валидная и не перезаписывает чужой marker/state. Параллельные
  инициализаторы не делят temp-файл и сходятся к одному валидному state.
  Типизированный `RustStateError` (`MissingMarker`/`MalformedMarker`/
  `ForeignImplementation`/`UnsupportedFormatVersion`/`NamespaceMismatch`/
  `MissingDatabase`/`UnmarkedState`/`MissingRuntimeOwner`/`ForeignRuntimeOwner`/
  `IncompatibleSchema`/`UnsupportedSchemaVersion`/`Connect`/`MarkerIo`/
  `Database`) не раскрывает project id, namespace, marker contents, SQL, пути и
  secrets. Тесты покрывают fresh init и reopen, идемпотентный повтор,
  компонентно-осознанную нормализацию `state_root` (`.`/`a/../b`/`../a`/
  `../../a`, отсутствие коллизий относительных roots) со стабильным
  hex-контрактом, missing/malformed marker (включая отсутствие `state_root`),
  foreign
  implementation, unsupported `format_version`, namespace mismatch, чужой
  `state_root` и state, скопированный под другой root, missing/foreign
  `meta.runtime_owner`, sidecar/SQLite disagreement, несовместимую schema,
  параллельную инициализацию, неизменность чужого state/marker при каждом
  отказе, восстановление после marker-без-DB и неизменность Python
  state/history.

**Готовность потока:** семантика совместима без изменения schema version, а
Python- и Rust-state изолированы: Rust ведёт собственную пустую БД и отдельную
историю, общая рабочая БД и перенос Python history отсутствуют; writable Rust
open невозможен без полной согласованности sidecar + namespace +
`meta.runtime_owner` + schema v6.

**Потоки 3 и 4 завершены.** Шаги **4.7** (snapshot основного repository),
**4.8** (multi-repository snapshots) и **4.9** (snapshot comparison и
violations) завершены; шаг **5.1** (test command validation) завершён;
следующий незавершённый шаг — **5.2** (один command runner).

## Поток 4. Security и Git

### 4.1. Простые command tokens

- **Завершено.** Новый workspace-crate `bridge-command-policy` воспроизводит
  только базовую семантику токенизации простых команд и leading environment
  assignments из reference `command_policy.py` (`basename`, `is_assignment`,
  `split_command`, `leading_assignments`) и не принимает policy-решений.
  `split_command` — POSIX-токенизатор, эквивалентный Python
  `shlex.split(text, posix=True)` с `comments=False` и `whitespace_split=True`:
  кавычки снимаются, quoted whitespace сохраняется, backslash экранирует
  следующий символ вне одинарных кавычек, соседние quoted/unquoted фрагменты
  склеиваются, `#` — обычный символ, а `\r`/`\n`/`\t`/пробел разделяют токены.
  Crate `shlex` не используется, потому что он трактует `#` как комментарий и
  `\<newline>` как продолжение строки, тогда как Python — нет. Как fail-closed
  расширение NUL отклоняется (reference отклоняет NUL до токенизации), хотя
  Python `shlex` сохранил бы его. `basename` повторяет `token.rsplit("/", 1)[-1]`,
  `is_assignment` требует непустое имя, начинающееся с alphabetic или `_` и
  состоящее далее из alphanumeric или `_`, а `leading_assignments` отделяет
  только ведущие assignments от argv, сохраняя порядок первого появления имени
  и last-value-wins для повторяющихся имён. Узкий typed API представлен
  `TokenizeError` без payload (ошибка не содержит исходную команду/токены) и
  `LeadingAssignments` с доступами `variables`/`get`/`argv`. Unit-тесты
  покрывают простые команды, whitespace, одинарные/двойные кавычки, quoted
  spaces/backslash, path-prefixed executable, валидные и невалидные
  assignments, повторяющиеся assignments, assignment-only вход,
  empty/whitespace вход, NUL и unmatched quotes. Запрещённые Git writes,
  wrappers env/sudo/command/exec/nohup/nice/time, shell executables, raw shell
  metacharacters/globs и permission decision остаются задачами 4.2/4.3.

### 4.2. Запрещённые Git writes и wrappers

- **Завершено.** `bridge-command-policy` переносит token-level семантику
  reference `command_policy.py`: `git_invocation_problem`, `env_invocation_problem`
  и wrapper-aware `tokens_problem`. Запрещённые `git add`/`commit`/`push`
  распознаются после пропуска leading (и любых) `NAME=value` assignments,
  path-prefixed executable (`/usr/bin/git`, `./git`) и wrappers `env`, `sudo`,
  `command`, `exec`, `nohup`, `nice`, `time` (включая вложенные и
  path-prefixed wrappers). `git` global options с отдельным значением
  (`-C`/`-c`/`--git-dir`/`--work-tree`/`--namespace`/`--exec-path`/
  `--config-env`/`--super-prefix`/`--shallow-file`) и `--opt=value`
  пропускаются; glob в позиции подкоманды даёт `unprovable_git_glob`; basename
  подкоманды сравнивается регистронезависимо (`git ADD`).
  `env -S`/`--split-string` (включая `--split-string=`, `-Sxxx`) даёт
  `unprovable_wrapper_command`. Узкий typed API: `tokens_problem` и
  `policy_decision`, `PolicyReason` (`git_write_blocked`/`unprovable_git_glob`/
  `unprovable_wrapper_command`) и `PolicyDecision`; решения и причины не несут
  command text, argv, пути и secrets, а Display/Debug содержат только
  статические строки. Как задокументированное fail-closed расширение над
  reference (intentional difference) `env` command-splitting отклоняется также
  для combined short-option cluster с флагом `S` (`-iS`, `-0S`, `-vS`) и
  однозначных long-option аббревиатур `--split-string` (`--split`, `--spl`,
  `--s`), а неизвестные, неоднозначные и value-missing `env` options
  завершаются fail closed; reference пропускает такие формы
  (`env -iS 'git push'`, `env --split 'git push'` действительно выполняют
  split-команду). Table-driven тесты покрывают read-only и запрещённые git
  операции, path-prefixed и абсолютные пути, `git` global options, glob
  подкоманды, все поддерживаемые wrappers, вложенность, assignments,
  combined/abbreviated/malformed env options и representative in-scope corpus
  из `docs/fixtures/command-policy-cases.json` (42 case, семантически сверены с
  Python reference; metacharacters/globs/shell executables/`eval` — 4.3).
  Существующие тесты 4.1 продолжают проходить. Cargo.toml/Cargo.lock не
  менялись.

### 4.3. Fail-closed compound shell syntax

- **Завершено.** `bridge-command-policy` переносит raw-pattern семантику reference
  `command_policy.py`: новый `bash_pattern_problem` (и typed
  `bash_pattern_decision`) проверяет raw-строку до токенизации, поэтому
  shell-синтаксис, который нельзя статически разрешить, отклоняется fail closed
  как `unprovable_shell_syntax` даже внутри кавычек: separators (`;`/`&&`/`||`),
  pipes (`|`), background (`&`), redirects (`<`/`>`), command substitution
  (`$(`), backticks, subshells, brace/variable expansion (`$`, `${}`, `{}`) и
  newline/CR. Пустой/whitespace-only вход даёт `empty_bash_pattern`, NUL или
  неразбираемая строка — `unparsable_bash_pattern`. `tokens_problem` расширен до
  полной reference-семантики: general glob в обычном токене даёт
  `unprovable_glob_command`; shell executables (`sh`, `bash`, `zsh`, `dash`,
  `ksh`, `fish`, включая path-prefixed) либо перепроверяются через `-c`
  (вложенный `tokens_problem`), либо отклоняются как `unsafe_shell_invocation`
  (без `-c`, включая combined `-lc`) и `shell_command_missing` (`-c` без
  строки); `eval` склеивает аргументы и заново проверяет их через
  `bash_pattern_problem`. Узкий typed API: `bash_pattern_problem`,
  `bash_pattern_decision`, `tokens_problem`, `policy_decision`; новые
  `PolicyReason` (`empty_bash_pattern`/`unprovable_shell_syntax`/
  `unprovable_glob_command`/`unsafe_shell_invocation`/`shell_command_missing`/
  `unparsable_bash_pattern`) и `PolicyDecision` не несут command text, argv, пути
  и secrets, а Display/Debug содержат только статические строки. Семантика
  воспроизводит reference точно, включая две наблюдаемые детали: raw-скан идёт
  до токенизации (метасимвол/glob внутри кавычек всё равно виден) и shell без
  `-c` (например `bash --version`) отклоняется как `unsafe_shell_invocation`.
  Table-driven тесты покрывают empty/whitespace, NUL/untokenizable, все
  separators/pipes/background/redirects/substitution/backticks/subshell/
  expansion/newline, quoted metacharacters и globs, shell executables (`-c`,
  missing, nested, path-prefixed, combined options), `eval`, а также все
  относящиеся к 4.3 cases из `docs/fixtures/command-policy-cases.json`;
  representative in-scope corpus 4.1–4.2 продолжает проходить. Verifier-конверт
  (`missing_executable` для assignment-only `test_command`) остаётся задачей 5.1.
  Существующие тесты 4.1–4.2 не регрессировали. Cargo.toml/Cargo.lock не
  менялись.

### 4.4. Workspace-relative allowed paths

- **Завершено.** Новый workspace-crate `bridge-path-policy` реализует только
  workspace-relative лексическую валидацию и нормализацию `allowed_paths` из
  reference `git_snapshot.validate_allowed_paths`, не обращаясь к файловой
  системе. Пустой список разрешён и нормализуется в пустой scope. Для каждого
  entry сохраняется reference-порядок проверок: пустая строка/не-строка
  (`invalid_allowed_paths_entry`), backslash (`backslash_in_path`), компонент
  `..` до нормализации (`parent_traversal`, поэтому `a/../b` отвергается),
  сведение к empty/`.` включая голый корневой слэш (`empty_or_dot_path`,
  `./`, `/`, `//`) и абсолютный путь (`absolute_workspace_path`). Оставшиеся
  относительные file/directory scopes нормализуются детерминированно: `.` и
  пустые компоненты удаляются, trailing `/` сохраняет семантическое различие
  file и directory scope, а missing file/directory обрабатываются лексически
  без требования существования. Узкий typed API — `validate_allowed_paths`
  (строковые entries) и boundary-уровень `validate_allowed_path_entries` с
  `AllowedPathEntry::NonText` для non-string JSON-значения; типизированный
  `PathPolicyReason` (`as_str`/`Display`/`Error`) не несёт входной путь, имена
  и secrets. Table-driven тесты покрывают все относящиеся к 4.4 cases и
  variants из `docs/fixtures/path-policy-cases.json`, включая
  `invalid_allowed_paths_entry` для non-string на boundary API, а также
  нормализацию, детерминизм и отсутствие утечки путей в ошибках. Symlink
  confinement (4.5), trusted external directories/абсолютные внешние пути и Git
  repository discovery (4.6), scope/snapshot/сравнение (4.7–4.9) и MCP-envelope
  не входят. Существующие crates и их public API не менялись.

### 4.5. Symlink confinement

- **Завершено.** `bridge-path-policy` расширен filesystem-aware слоем поверх
  лексической семантики 4.4, не меняя существующий узкий API
  (`validate_allowed_paths`, `validate_allowed_path_entries`) и его поведение.
  Новые typed-функции `validate_workspace_allowed_paths` и
  `validate_workspace_allowed_path_entries` принимают workspace и
  workspace-relative `allowed_paths`: workspace канонизируется через
  `fs::canonicalize`, каждый entry сначала проходит reference-лексические
  проверки 4.4, а затем лексически нормализованный relative scope разрешается
  относительно canonical workspace по семантике Python
  `Path.resolve(strict=False)` — существующие symlink-компоненты следуются
  (абсолютный target перезапускает разрешение от корня FS, относительный
  разрешается от родителя ссылки, `.`/`..` внутри target сворачиваются),
  отсутствующий компонент завершает разрешение, а оставшийся хвост
  присоединяется лексически. Symlink в промежуточном компоненте обрабатывается
  так же, как конечный. Если resolved target не равен canonical workspace и не
  является его потомком, entry отклоняется как стабильная reason category
  `workspace_escape` (`PathPolicyReason::WorkspaceEscape`), покрывая и
  существующий, и dangling symlink escape; разрешённый entry сохраняет
  нормализованный исходный workspace-relative scope (не target path) с
  сохранением trailing-slash различия file/directory. Symlink loops и
  I/O/canonicalization failures (неканонизируемый workspace, non-`NotFound`
  metadata-ошибки, например не-каталог в компоненте, нечитаемая ссылка)
  завершаются fail closed типизированными `PathPolicyReason::SymlinkLoop`/
  `ResolutionFailure` — задокументированное fail-closed расширение над
  reference, который в non-strict режиме молча продолжает лексически. Все
  ошибки не несут payload и не раскрывают входные пути, workspace, symlink
  target, secrets или OS error text в `Display`/`Debug`. Table-driven тесты
  воспроизводят четыре относящиеся к 4.5 ветви
  `docs/fixtures/path-policy-cases.json`
  (`validate-relative-symlink-inside-scope`,
  `validate-relative-symlink-escape`, `validate-dangling-symlink-inside-scope`,
  `validate-relative-dangling-symlink-outside`), а также промежуточные
  symlink-компоненты, сворачивание `..` в target, symlink loop, отказ
  канонизации/не-каталога, сохранение лексических отказов 4.4 и отсутствие
  утечки путей/target/OS-text. Absolute external paths, trusted roots и Git
  repository discovery (4.6), scope/snapshot/сравнение (4.7–4.9) и MCP-envelope
  не входят. Cargo manifests/dependencies не менялись, тесты 4.4 не
  регрессировали.

### 4.6. External Git repository paths

- **Завершено.** `bridge-path-policy` расширен trusted-root слоем поверх
  лексической (4.4) и workspace symlink-confinement (4.5) семантики, не меняя
  существующий узкий API и его поведение. Новые typed-функции
  `validate_allowed_paths_with_trusted_roots` и
  `validate_allowed_path_entries_with_trusted_roots` принимают workspace,
  trusted external roots и `allowed_paths`: workspace и каждый trusted root
  канонизируются fail closed, относительные entries обрабатываются ровно как в
  4.5, а абсолютные разрешаются по семантике Python `Path.resolve(strict=False)`
  и проверяются в reference-порядке. Абсолютный путь, разрешающийся внутри
  canonical workspace, отклоняется как `absolute_workspace_path` **до**
  trusted-root lookup; затем путь вне всех trusted roots отклоняется как
  `outside_trusted_roots` (в том числе при escape через symlink). Для
  допустимого пути канонический root содержащего Git worktree находится через
  `git rev-parse --show-toplevel` (ближайший существующий предок для missing
  leaf, сам repo root и child поддержаны), probe ограничен таймаутом с
  принудительным kill/reap, timeout — fail-closed `path_resolution_failure`, а
  stdout читается как raw Unix path bytes без lossy/strict UTF-8 (non-UTF-8 repo
  root распознаётся), отсутствие репозитория даёт
  `not_git_repository`, а repo root вне trusted roots —
  `external_repo_root_outside_trusted`, поэтому вложенный trusted subdir внутри
  более высокого репозитория не расширяет доверие. Нормализованный validated
  scope типизирован как `ValidatedAllowedPath`: workspace-relative entries
  остаются относительными, external entries — каноническими абсолютными, а
  `PathScope` (`File`/`Directory`) сохраняет trailing-slash различие. Узкий typed
  API `group_allowed_paths_by_repo` группирует raw entries по каноническому
  repository root: relative entries без нормализации/валидации попадают в
  canonical main workspace, absolute — в найденный repository root, trusted roots
  не применяются; main-workspace bucket всегда присутствует и идёт первым,
  внешние bucket-ы детерминированно отсортированы. Ошибки представлены
  payload-free `PathPolicyReason` с новыми стабильными категориями
  `outside_trusted_roots`/`not_git_repository`/`external_repo_root_outside_trusted`
  и не раскрывают workspace, roots, входные пути, Git output, symlink target или
  OS error text в `Debug`/`Display`; symlink loop и I/O/canonicalization failures
  остаются fail closed (`symlink_loop`/`path_resolution_failure`). Table-driven
  тесты покрывают все относящиеся к 4.6 cases `validate_allowed_paths` и
  `group_allowed_paths_by_repo` из `docs/fixtures/path-policy-cases.json`
  (`validate-absolute-trusted-repo-file`, `validate-absolute-trusted-repo-directory`,
  `validate-absolute-outside-trusted`, `validate-absolute-repo-root-outside-trusted`,
  `validate-absolute-symlink-escape-outside-trusted`,
  `validate-absolute-trusted-not-git-repository`, `validate-absolute-workspace-file`,
  `validate-absolute-workspace-directory`, `group-empty-list`,
  `group-external-not-git-repository`, `group-external-repo-root-and-child`,
  `group-relative-and-external`, `group-two-external-repos`), а также repo
  root/child, missing leaf, trusted symlink escape, nested trusted root, missing
  trusted root, boundary non-string, сохранение raw entries, отсутствие утечки и
  regression 4.4–4.5. Cargo manifests/dependencies не менялись.

### 4.7. Snapshot основного repository

- **4.7a. Базовый снимок — завершено.** Новый workspace-crate
  `bridge-git` (без зависимостей от других bridge-crates) реализует
  изолированный read-only слой для базового состояния одного Git worktree.
  Проверка worktree эквивалентна `git rev-parse --is-inside-work-tree`:
  `false`/nonzero даёт typed `GitError::NotRepository`, а
  spawn/wait/timeout/I/O и malformed output завершаются fail closed
  инфраструктурной ошибкой. `head` вызывает `git rev-parse HEAD`: nonzero даёт
  `None` для валидного repository без commit, а успешный ответ принимается
  только как opaque ASCII commit id строгой формы (40 или 64 lowercase hex) без
  lossy decode. `status --porcelain=v1 -z` парсится собственным parser-ом:
  обычные entries и обе стороны rename/copy, пути как Unix `OsString` bytes без
  lossy `String`, malformed porcelain — fail closed, результат сортируется и
  дедуплицируется. `index_fingerprint` — SHA-256 от точных bytes
  `git ls-files --stage -z`, затем NUL separator, затем `git ls-files -v -z`
  (SHA-256 реализован внутри crate и сверен с FIPS 180-4 vectors, внешних
  зависимостей нет). `RepositorySnapshot` содержит HEAD, raw status/dirty
  paths, index fingerprint, а с 4.7b — ещё manifest и worktree fingerprint;
  фиктивные поля не добавлены. Git command runner ограничен: фиксированный
  executable `git`, stdin null, stderr отбрасывается (не попадает в ошибку),
  stdout читается как raw bytes отдельным потоком (нет pipe deadlock),
  wall-clock timeout с принудительным kill/reap; shell и Git write-команды
  отсутствуют. Узкий payload-free `GitError` не раскрывает workspace, argv,
  stdout/stderr, Git config, OS errors и secrets через `Display`/`Debug`. Тесты
  выполняются только во временных synthetic repositories: clean, no commit, dirty
  tracked/untracked, staged, intent-to-add, rename/copy parser, malformed
  porcelain, non-repository, timeout/kill/reap, non-UTF-8 Unix paths и
  стабильность index fingerprint. Manifest/хеширование файлов и worktree
  fingerprint реализованы в 4.7b; changed/committed paths, history ancestry,
  scope/policy violations, external/multi-repository snapshots и worker/MCP
  integration не входят. Существующие crates, их schema/public API и fixtures не
  менялись.

- **4.7b. Worktree manifest и `worktree_fingerprint` — завершено.** Тот же
  `bridge-git` (внешних зависимостей по-прежнему нет) добавляет детерминированный
  manifest рабочего дерева и стабильный SHA-256 fingerprint. `worktree_manifest`
  берёт ровно релевантные Git-ом файлы: tracked из `git ls-files -z` и
  non-ignored untracked из `git ls-files --others --exclude-standard -z` (ignored
  entries исключает сам Git), а отсутствующий в worktree tracked путь (deletion)
  не даёт entry. Пути — точные Unix `OsString` bytes без lossy UTF-8; entries
  упорядочены ровно как Python `sorted()` над surrogateescape-decoded именами
  (code point order, а не raw bytes: одиночный invalid byte `0xff` → `U+DCFF`
  идёт перед supplementary `U+1F600`) и дедуплицируются, поэтому manifest
  детерминирован и совпадает с reference для любого byte sequence.
  Digest entry фиксирует тип, executable bit и symlink identity: обычный файл —
  `sha256("file\0" || content)`, при любом executable bit — `sha256("exec\0" ||
  content)`, symlink — `sha256("link\0" || raw target bytes)` (target-файл не
  читается). `worktree_fingerprint` воспроизводит компактный digest reference
  `verifier.fingerprint`: `sha256` по каждой entry в отсортированном порядке
  (`path || 0x00 || lowercase hex digest || 0x00`), затем `b"status\0"` и raw
  status bytes; сверен с Python reference на фиксированном synthetic repository.
  Fail-closed расширение над reference: файловая ошибка даёт payload-free
  `GitError::ManifestIo`, а listed entry, не являющийся обычным файлом или
  symlink (directory, FIFO, socket, device), — `GitError::UnsupportedFileType`;
  malformed `-z` framing — `MalformedOutput`. `RepositorySnapshot` получил
  `manifest()`/`worktree_fingerprint()`, вычисляемые из тех же status bytes.
  Focused tests во временных synthetic repositories покрывают stable clean
  snapshot, tracked content change, staged/untracked, ignored exclusion,
  executable-bit-only change, symlink retarget/dangling/ignored target,
  non-UTF-8 paths, deterministic ordering (включая mixed supplementary-Unicode
  и invalid-byte имена, сверенные с Python reference), deletion, fail-closed
  directory/FIFO/unreadable cases и совпадение с Python reference. Multi-repository
  snapshots (4.8), comparison/violations (4.9) и worker/MCP integration не входят;
  Cargo manifests/dependencies не менялись.

### 4.8. Multi-repository snapshots

- **Завершено.** `bridge-git` получил новый модуль `multi_repo`, который
  оркестрирует read-only снимки основного workspace и всех затронутых внешних
  репозиториев поверх существующего `take_snapshot` (4.7) и типизированных
  repository bucket-ов `bridge-path-policy::group_allowed_paths_by_repo` (4.6).
  Crate получил локальную dependency `bridge-path-policy`; `bridge-path-policy`
  не изменялся. Узкий typed API: `take_multi_repository_snapshot(main_workspace,
  &[AllowedPathGroup])`, результат `MultiRepositorySnapshot` из
  `RepositoryGroupSnapshot` (`root()`, `allowed_paths()`, `snapshot()`), плюс
  payload-free `MultiRepoError`. Контракт: ровно один snapshot на каждый
  уникальный canonical repository root; main repository всегда присутствует и
  идёт первым даже при пустом `allowed_paths`; внешние репозитории следуют в
  детерминированном порядке по возрастанию canonical root; каждый entry
  сохраняет canonical root, исходные raw allowed entries своего bucket-а и
  соответствующий snapshot, поэтому одинаковые relative пути в разных
  репозиториях не смешиваются. Перед snapshot каждого repository bucket root
  сверяется с фактическим canonical Git worktree root
  (`git rev-parse --show-toplevel`, canonicalize, точное равенство). Fail closed
  завершаются: неканонизируемый main workspace
  (`workspace_resolution_failed`), неканонизируемый/исчезнувший bucket root
  (`repository_resolution_failed`), пустой список bucket-ов
  (`missing_main_repository`), первый bucket не main
  (`main_repository_mismatch`), duplicate/conflicting canonical root
  (`duplicate_repository_root`), не-Git-worktree (`not_a_git_repo`), подмена,
  вложенный или иначе не-canonical root (`repository_root_mismatch`) и любая Git
  infrastructure-ошибка probe/snapshot (`MultiRepoError::Git`). Ошибки не несут
  payload и не раскрывают roots, allowed paths, Git output, argv, OS error text и
  secrets в `Debug`/`Display`; shell и Git write-команды отсутствуют. Тесты во
  временных synthetic repositories покрывают main-only с пустыми allowed paths,
  main + один/два внешних репозитория с детерминированным порядком, repo
  root + child в одном snapshot, одинаковый relative filename в разных
  репозиториях без смешивания, независимые dirty/head/index/worktree состояния,
  duplicate/root-mismatch/disappeared/non-repository fail closed, payload-free
  errors и regression 4.7 (`take_snapshot` переиспользуется без изменений).
  Observable semantics сверены с Python reference multi-repository verifier flow
  (`git_snapshot.take_external_snapshots` + main snapshot в
  `mcp_server.submit_task_impl`) и релевантными fixtures
  `docs/fixtures/path-policy-cases.json` (group-* cases) и
  `docs/fixtures/git-snapshot-cases.json`; 4.9 comparison/violations,
  `changed_paths`/`committed_paths`, ancestry и worker/MCP integration не входят.
  `Cargo.toml`/`Cargo.lock` изменились только добавлением локальной dependency
  `bridge-path-policy` у `bridge-git`.

### 4.9. Snapshot comparison и violations

- **Завершено.** `bridge-git` получил typed API сравнения одного и нескольких
  repository baseline: `compare_repository_snapshot` и
  `compare_multi_repository_snapshot`, результаты `RepositoryComparison` /
  `MultiRepositoryComparison` и стабильный `GitPolicyViolation`. Сравнение
  вычисляет worktree `changed_paths` по manifest, `committed_paths` через
  read-only `git diff`/`diff-tree`, объединённые scope violations с точной
  file/directory семантикой и Git-policy violations в reference-порядке:
  `history_rewritten`, `head_changed`, `index_changed`; `allow_commit=true`
  подавляет только последние два. Missing main/external repository даёт
  соответственно `not_a_git_repo`/`external_repo_missing` без Git-команд над
  исчезнувшим root. Multi-repository сравнение сохраняет baseline-порядок,
  canonical root и независимые repository-relative результаты; external scope
  проверяется по абсолютному root-qualified candidate, но наружу возвращаются
  relative paths. Commit ids передаются Git как отдельные OS-arguments, shell и
  Git writes отсутствуют; команды ограничены прежним timeout/kill/reap runner.
  NUL path output парсится fail closed, Unix non-UTF-8 paths сохраняются без
  lossy decode и сортируются по Python surrogateescape-порядку. Ошибки остаются
  payload-free. Focused synthetic-repository tests покрывают clean/dirty,
  file/directory scope, commit, ancestry rewrite, `allow_commit`, missing
  external repository и независимое main/external сравнение; workspace format,
  clippy и tests проходят. Worker/MCP flat aggregation остаётся последующей
  интеграцией, а не частью security/Git primitive.

**Готовность потока:** решения совпадают с security corpus.

## Поток 5. Verifier

### 5.1. Test command validation

- **Завершено.** `bridge-command-policy` получает узкий verifier-level API поверх
  существующих policy primitives: `validate_test_commands(commands: &[&str]) ->
  Vec<TestCommandProblem>`, эквивалент Python
  `verifier.validate_test_commands` для строковых входов. Пустой список валиден и
  даёт пустой результат; каждая команда сначала проверяется существующей
  fail-closed семантикой `bash_pattern_problem`, а команда, у которой после
  ведущих `NAME=value` assignments остаётся пустой argv, отклоняется отдельной
  стабильной причиной `missing_executable`. Порядок результатов и индексы
  сохраняются как в Python `enumerate`. Типизированный `TestCommandReason`
  (`Policy(PolicyReason)` / `MissingExecutable`) и `TestCommandProblem`
  (`index`/`reason`) не несут command text, argv, пути и secrets;
  `Debug`/`Display` содержат только индекс и статическую причину.
  Не-строковые/не-list входы verifier относятся к configuration/transport и в
  crate не входят, как и раньше. Table-driven тесты покрывают
  все cases и variants `context=test_command` из
  `docs/fixtures/command-policy-cases.json` (safe commands, policy denials,
  assignment-only, пустой/whitespace вход), пустой список,
  сохранение порядка/индексов и отсутствие утечки входа в `Debug`/`Display`;
  существующие тесты 4.1–4.3 не регрессировали. Cargo.toml/Cargo.lock не
  менялись.

### 5.2. Один command runner

### 5.3. Последовательность команд

### 5.4. HEAD/workspace fingerprints

### 5.5. Side-effect detection

### 5.6. Persist-once semantics

**Готовность потока:** verifier fixtures эквивалентны Python.

## Поток 6. OpenCode adapter

### 6.1. HTTP transport и basic auth

### 6.2. Health и workspace identity

### 6.3. OpenAPI compatibility

### 6.4. Session create/list/get

### 6.5. Message list и parsing

### 6.6. Async prompt delivery

### 6.7. Permissions list/reply

### 6.8. Questions и blockers

**Готовность потока:** mock server проверяет каждый endpoint независимо.

## Поток 7. Worker

### 7.1. Worker argv и spawn

### 7.2. Lock и startup grace

### 7.3. Session resolution

### 7.4. Initial prompt happy path

### 7.5. Revision round

### 7.6. Permission blocker

### 7.7. Question blocker

### 7.8. Auto-approval integration

### 7.9. Failed/delivery_unknown

### 7.10. Continuation recovery

### 7.11. Verification integration

### 7.12. Cooperative close

**Готовность потока:** worker scenarios совпадают по БД и результату.

## Поток 8. MCP

### 8.1. `project_info`

### 8.2. `submit_task`

### 8.3. Compact `task_status`

### 8.4. Verbose `task_status`

### 8.5. Long wait и раннее пробуждение

### 8.6. `request_changes`

### 8.7. `accept_task`

### 8.8. `close_task`

### 8.9. stdio transport

### 8.10. Authenticated HTTP transport

### 8.11. Startup recovery

Одна tool-задача содержит только один handler и его contract fixtures.

## Поток 9. Runtime CLI

### 9.1. CLI parser и общие flags

### 9.2. `setup`

### 9.3. `doctor`

### 9.4. `serve-opencode`

### 9.5. `serve-mcp` и `mcp`

### 9.6. Process ownership и pidfd primitives

### 9.7. `start` одного проекта

### 9.8. Multi-project start и rollback

### 9.9. `status`

### 9.10. `stop`

### 9.11. `console` и `attach-opencode`

### 9.12. `launch-codex`

### 9.13. `launch-opencode`

### 9.14. Linked-project routing

**Готовность потока:** совместимы argv, exit codes и безопасные ошибки.

## Поток 10. GPUI foundation

### 10.1. Минимальное окно Wayland/X11

### 10.2. Application state

### 10.3. Resizable split

### 10.4. Background service bridge

### 10.5. Theme, fonts и accessibility baseline

## Поток 11. Настройки проектов

### 11.1. Read-only project list

### 11.2. Project edit form

### 11.3. Workspace picker

### 11.4. Endpoints и port validation

### 11.5. Permissions/trusted roots editor

### 11.6. Credential fields с redaction

### 11.7. Full-config validation

### 11.8. Change preview

### 11.9. Atomic save

### 11.10. Create/clone project

### 11.11. Setup/Doctor actions

### 11.12. Start/Stop actions

**Готовность потока:** проект настраивается без ручного редактирования TOML.

## Поток 12. Dashboard

### 12.1. Read-only task query service

### 12.2. Таблица одного проекта

### 12.3. Aggregation связанных проектов

### 12.4. Active/All filter

### 12.5. Search и project/status filters

### 12.6. Virtualized rows

### 12.7. Selection persistence по task ID

### 12.8. Task details

### 12.9. Verification/repository details

### 12.10. Periodic refresh

### 12.11. WAL notification optimization

### 12.12. Open/attach session action

**Готовность потока:** паритет с curses dashboard, UI read-only.

## Поток 13. Embedded terminal

### 13.1. Выбор terminal engine

### 13.2. PTY spawn и exit

### 13.3. PTY resize/SIGWINCH

### 13.4. Базовый cell rendering

### 13.5. ANSI colors и cursor

### 13.6. Unicode, wide и combining glyphs

### 13.7. Keyboard input

### 13.8. Scrollback

### 13.9. Selection и clipboard

### 13.10. Bracketed paste

### 13.11. Mouse reporting

### 13.12. Alternate screen

### 13.13. Codex launch profile

### 13.14. OpenCode launch profile

### 13.15. External-terminal fallback

### 13.16. Project switch/window close policy

**Готовность потока:** Codex/OpenCode compatibility suite проходит в GPUI.

## Поток 14. Миграция

### 14.1. Подготовка переключения и остановка Python runtime

Python runtime останавливается; проверяется отсутствие живых locks и
PID/process records. Python state при этом не копируется, не читается и не
импортируется.

### 14.2. Создание нового Rust state

Rust инициализирует собственную пустую БД schema v6 и отдельную историю задач;
Python state не импортируется, не копируется и не переносится.

### 14.3. Проверка изоляции и ownership marker

Раздельные state root, SQLite, locks, PID/ownership records, token-файлы, логи и
endpoints; schema v6, `meta.runtime_owner='rust'` и sidecar marker; fail-closed
при чужом state.

### 14.4. Backup и независимый rollback rehearsal

Откат возвращает к неизменённому Python state; Rust state не переносится
обратно, а Python state не получает записей от Rust.

### 14.5. Один тестовый проект на Rust runtime

### 14.6. Crash/recovery drill

### 14.7. Soak period и метрики

### 14.8. Последовательный перевод проектов

### 14.9. Решение об архивировании Python

## Экономия контекста

- В задачу включаются только нужные fixtures и раздел документа.
- Отчёт содержит результат, файлы, checks и следующий dependency без пересказа
  всей архитектуры.
- Полная test matrix запускается на границе потока или в CI; внутри задачи —
  targeted tests и необходимые regressions.
- Рефакторинг общего слоя выполняется отдельной задачей до потребителей.
- Независимые потоки параллельны только после стабилизации общего контракта.
- Решение, влияющее на будущие задачи, фиксируется коротким ADR.

## Общий Definition of Done

- старые config и SQLite работают без конвертации;
- MCP и CLI контрактно совместимы;
- crash recovery покрывает промежуточные статусы;
- security/verifier suites проходят;
- GUI настраивает проекты без раскрытия секретов;
- Codex/OpenCode работают во встроенном PTY;
- dashboard отзывчив и соответствует storage;
- установка и rollback документированы.
