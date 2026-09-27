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

### 3.10. Storage isolation и односторонний импорт копии

Проверить, что Python- и Rust-реализации используют раздельные state root,
SQLite-файлы, locks, PID/ownership records, token-файлы, логи и endpoints;
write tests выполняются только на независимых временных копиях каждой
реализации. Импорт — односторонний: WAL-aware копия Python state при
остановленном Python runtime, исходный Python state не изменяется. Contract
fixtures Python остаются эталоном семантики; запись обеими реализациями в одну
рабочую БД не предполагается.

### 3.11. Ownership/format marker и fail-closed guard

Зафиксировать sidecar marker (`implementation="rust"`, `format_version`) и
additive `meta.runtime_owner='rust'`; Rust отказывается открывать state другой
реализации или чужого namespace. Schema v6 и contract fixtures не меняются.

**Готовность потока:** семантика совместима без изменения schema version, а
Python- и Rust-state изолированы и связаны только односторонним импортом копии.

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

### 14.1. Read-only запуск на изолированной копии state

### 14.2. Односторонний импорт WAL-aware копии Python state в Rust state

Python runtime остановлен; исходный Python state не изменяется и сохраняется
для отката.

### 14.3. Проверка Rust state и ownership marker

Schema v6, `meta.runtime_owner='rust'` и sidecar marker; fail-closed при чужом
state.

### 14.4. Backup и независимый rollback rehearsal

Откат возвращает к неизменённому Python state; Rust state не переносится
обратно.

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
