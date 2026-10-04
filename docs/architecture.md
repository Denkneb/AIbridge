# Архитектура

## Принципы

1. Доменная логика не зависит от UI, CLI и MCP transport.
2. CLI, MCP и Tauri desktop используют один Rust сервисный слой.
3. Привязка процесса к project, workspace и endpoints неизменяема.
4. Worker остаётся отдельным процессом для изоляции и recovery.
5. Переходы состояния проверяются моделью и выполняются транзакционно.
6. Security-critical код сохраняет fail-closed семантику Python-версии.

## Общая схема

```text
React + TypeScript (Vite, Tauri WebView)
        │ typed IPC / streamed PTY output
Tauri Rust adapter ─┐
CLI ────────────────┼── application services ── domain
MCP stdio/HTTP ─────┘            │                 │
                                ├── SQLite        │
                                ├── OpenCode HTTP │
                                ├── Git/processes │
                                ├── PTY           │
                                └── worker ───────┘
```

## Cargo workspace

```text
crates/
  bridge-domain/       Task, Round, статусы и переходы
  bridge-config/       projects.toml, credentials, project env
  bridge-storage/      SQLite schema, migrations, repositories
  bridge-opencode/     HTTP client и OpenAPI compatibility
  bridge-git/          snapshots, allowed paths, repository discovery
  bridge-policy/       shell и permission policy
  bridge-verifier/     commands, fingerprints, side effects
  bridge-worker/       implementation, revision, recovery
  bridge-mcp/          stdio и authenticated HTTP MCP
  bridge-runtime/      locks, pidfd, start/status/stop
  bridge-terminal/     PTY, input/output, resize и process lifecycle
  bridge-desktop/      Tauri entry point, IPC adapter и window lifecycle
  agent-bridge-cli/    итоговый бинарник
frontend/             React + TypeScript, Vite, dashboard и xterm.js
```

Это границы ответственности, а не обязательное число crates: пакеты можно
объединить, если разделение не улучшает независимое тестирование.

## Граница desktop frontend/backend

React владеет отображением, формами и состоянием навигации; xterm.js —
отображением терминала и terminal input. Tauri adapter предоставляет
типизированные команды и потоки данных к существующим Rust services.
Rust владеет SQLite, Git, HTTP, credentials, policy checks, PTY и subprocesses.
Domain/services не зависят от React или Tauri; headless CLI собирается и
работает отдельно от desktop/WebView.

Frontend получает безопасные DTO и opaque session references. Он не получает
credential contents, прямой SQL-доступ или универсальную команду запуска shell.
Rust проверяет project/session binding и входные данные каждого IPC-вызова;
Tauri capabilities и CSP задаются явно, с минимально необходимыми разрешениями.
Terminal output считается недоверенными данными и передаётся в xterm.js.

PTY output передаётся порциями через Tauri channels с bounded buffering и
backpressure. Terminal byte stream не хранится в React state; component lifecycle
не должен пересоздавать работающий terminal или завершать backend session.
Resize/input/exit/cancellation и поведение при закрытии окна имеют отдельный
контракт. Detached runtime services сохраняют существующую ownership модель.

## Доменная модель

Статусы задаются закрытым `enum`:

```text
implementing -> awaiting_review -> revising -> awaiting_review -> accepted
       |               |              |
       +----------> needs_user <-------+
       +----------> failed
       +----------> delivery_unknown
```

`Task`, `Round`, `Verification`, `RepositorySnapshot` и `UserAction` получают
типизированные поля и версионируемую сериализацию. Недопустимый переход должен
отклоняться до записи в SQLite.

## Хранилище

- Python и Rust никогда не используют общую рабочую БД или общий runtime state.
- Каждая реализация владеет отдельным state root, `state.sqlite`, WAL/SHM
  sidecar-файлами, locks, PID/ownership records, token-файлами и логами.
- Rust всегда инициализирует собственную пустую БД schema v6 и ведёт отдельную
  историю задач; Python state/history в Rust не импортируется и не копируется.
- Внутри собственного state каждой реализации сохраняются WAL, foreign keys,
  busy timeout и `BEGIN IMMEDIATE`.
- `PRAGMA user_version` меняется только отдельной миграцией внутри владельца
  state.
- Идемпотентность обеспечивается `request_id` и хешем payload.
- GUI читает только Rust-owned SQLite через `bridge-storage`, а не через MCP.
- Один проект имеет не более одной незавершённой задачи.

## Изоляция Python и Rust

У каждой реализации отдельный namespace по умолчанию:

| Артефакт | Python (существующий) | Rust (изолированный) |
| --- | --- | --- |
| config | `$XDG_CONFIG_HOME/agent-bridge/projects.toml` | тот же файл, read-only |
| secrets/token | `$XDG_CONFIG_HOME/agent-bridge/secrets/` | `$XDG_CONFIG_HOME/agent-bridge-rs/secrets/` |
| state root | `$XDG_STATE_HOME/agent-bridge` | `$XDG_STATE_HOME/agent-bridge-rs` |
| project state | `.../agent-bridge/<project_id>/` | `.../agent-bridge-rs/<project_id>/` |
| database | `<state_dir>/state.sqlite` | `<state_dir>/state.sqlite` в Rust root |
| locks | `mcp.lock`, `worker.lock`, `runtime.lock` | те же имена в Rust root |
| PID/ownership | `<state_dir>/<kind>.process.json` | в Rust state dir |
| logs | `<state_dir>/*.log` | в Rust state dir |
| endpoints/ports | порты проекта Python | отдельный Rust namespace портов |

`projects.toml` остаётся единственным источником конфигурации. Rust читает его
read-only в период сосуществования и не пишет, пока существует Python runtime;
запись допускается только от активной реализации при остановленных runtime
проекта, поэтому конкурентной записи не возникает.

Активным владельцем проекта одновременно является ровно одна реализация. При
переключении проекта его endpoints, порты и token-файлы переводятся в namespace
нового владельца в рамках переключения, когда Python runtime уже остановлен, а
Rust state проверен; одновременного использования одного endpoint/token двумя
реализациями не происходит.

## Ownership и fail-closed

Rust state помечается ownership/format marker:

- sidecar `<state_dir>/.agent-bridge-state.json` с `implementation="rust"`,
  `format_version`, `project_id`, `state_root`;
- additive строка `meta.runtime_owner='rust'` в БД (schema v6 и
  `meta.schema_version` не меняются).

Rust fail-closed отказывается открывать или инициализировать state, если:

- каталог лежит вне настроенного Rust state root или совпадает с Python state
  root;
- sidecar marker отсутствует или `implementation != "rust"`;
- `meta.runtime_owner` присутствует и не равен `rust`;
- живые locks или ownership records другой реализации относятся к проекту.

Эквивалентный guard на стороне Python оформляется отдельной задачей; до неё
изоляция обеспечивается раздельными namespace, запретом одновременного запуска
и Rust-side проверкой.

## Собственная история Rust

Rust всегда создаёт и использует собственную пустую БД и отдельную историю
задач. Импорт, копирование или перенос Python state/history в Rust не
поддерживается и не планируется: Python history в Rust не появляется, а Python
state не получает записей от Rust. Исходный Python state остаётся отдельным и
служит точкой отката.

## Процессы

Worker запускается подпроцессом того же бинарника:

```text
agent-bridge worker --project ID --task TASK_ID --round N
```

`start/status/stop` сохраняют process ownership records, Linux start time,
pidfd и locks. Остановка не затрагивает вручную запущенные процессы.

## OpenCode и MCP

OpenCode adapter изолирует health/OpenAPI checks, sessions, `prompt_async`,
messages, permissions, questions и one-time replies.

Сохраняются MCP tools:

- `project_info`;
- `submit_task`;
- `task_status`;
- `request_changes`;
- `accept_task`;
- `close_task`.

Поддерживаются stdio и authenticated Streamable HTTP на localhost. Compact и
`verbose=true` ответы, `wait_seconds <= 300`, recovery side effects и ранний
возврат при смене статуса должны быть совместимы.

## Security boundaries

- canonical paths и symlink confinement проверяются перед доступом;
- абсолютные allowed paths допустимы только в доверенных внешних Git repos;
- недоказуемо безопасный shell-синтаксис отклоняется;
- `git add`, `git commit`, `git push` не разрешаются автоматически;
- секреты не попадают в argv, UI, SQLite, логи и ошибки;
- project/workspace/endpoint нельзя изменить MCP-аргументом.
