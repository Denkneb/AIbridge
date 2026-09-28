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
- **Следующий незавершённый шаг потока 3 — 3.11** (ownership/format marker и
  fail-closed guard).

### 3.11. Ownership/format marker и fail-closed guard

Зависит от **3.10**: маркеры выставляются только на новом изолированном Rust
state. Зафиксировать sidecar marker (`implementation="rust"`, `format_version`)
и additive `meta.runtime_owner='rust'`; Rust отказывается открывать state другой
реализации или чужого namespace. Schema v6 и contract fixtures не меняются.

**Готовность потока:** семантика совместима без изменения schema version, а
Python- и Rust-state изолированы: Rust ведёт собственную пустую БД и отдельную
историю, общая рабочая БД и перенос Python history отсутствуют.

## Поток 4. Security и Git

### 4.1. Простые command tokens

### 4.2. Запрещённые Git writes и wrappers

### 4.3. Fail-closed compound shell syntax

### 4.4. Workspace-relative allowed paths

### 4.5. Symlink confinement

### 4.6. External Git repository paths

### 4.7. Snapshot основного repository

### 4.8. Multi-repository snapshots

### 4.9. Snapshot comparison и violations

**Готовность потока:** решения совпадают с security corpus.

## Поток 5. Verifier

### 5.1. Test command validation

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
