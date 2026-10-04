# Риски и решения

## Принятые решения

### Поэтапная миграция

Big-bang rewrite слишком рискован для накопленной state/security логики.

### PTY вместо собственного chat protocol

Codex/OpenCode сохраняют streaming, permissions, tools и slash-команды.
Собственный chat client может стать отдельным будущим этапом.

### Десктопный UI: Tauri 2, React, TypeScript, Vite

**Принято 2026-10-04.** Целевой GUI использует React + TypeScript, frontend
собирается Vite и работает в Tauri 2 WebView. Предыдущий выбор GPUI заменён
до начала реализации GUI; существующий Rust backend сохраняется.
Dashboard и settings относятся к React, terminal renderer — xterm.js,
PTY/SQLite/Git/processes/credentials/security policy — к Rust services.

Основание: формы и dashboard удобно разрабатывать в web UI, а xterm.js даёт
готовый terminal renderer. Tauri adapter связывает UI с Rust без зависимости
domain/services от UI framework. React + Vite поддерживаются
[Tauri frontend configuration](https://v2.tauri.app/start/frontend/);
роль terminal renderer описана в [xterm.js](https://xtermjs.org/).

Следствия: добавляются frontend toolchain/lockfile, типизированный IPC boundary
и системные WebView зависимости. Headless CLI остаётся отдельным target.
Первый UI spike — window + terminal/dashboard split + настоящая PTY session,
с проверкой input/resize/Unicode/large output/process cleanup. Полные settings
и dashboard идут после него; backend-очередь продолжается с 7.14.

### Read-only dashboard

Мутации задач требуют отдельной модели review и authorization; случайные GUI
кнопки не должны обходить существующий workflow.

### Раздельные state Python и Rust

Реализации не делят рабочую БД или runtime state. У Python и Rust свои state
root, SQLite, locks, PID/ownership records, token-файлы, логи и endpoints. Rust
всегда создаёт собственную пустую БД и отдельную историю задач; импорт,
копирование или перенос Python state/history в Rust не поддерживается и не
планируется, а Python state не изменяется и служит точкой отката.

### projects.toml — источник истины

CLI и GUI не должны расходиться или использовать разные базы настроек.
`projects.toml` остаётся каноническим и общим для чтения; запись допускается
только от активной реализации при остановленных runtime, поэтому конкурентной
записи не возникает.

## Риски

### WebView и frontend toolchain

Закрепить Cargo и frontend lockfiles; обновления Tauri/frontend выполнять
отдельно. Проверить WebKitGTK, Wayland/X11, fonts/HiDPI и packaging на целевых
Linux-системах. Tauri/React types остаются в adapters, вне domain/services.

### Embedded terminal

xterm.js отображает терминал, Rust adapter управляет настоящим PTY. Нужны
IPC input/output bridge с bounded queues/backpressure, корректные resize/exit,
ранний spike, Codex/OpenCode compatibility fixtures и внешний fallback.

### Frontend/backend IPC

Минимальные capabilities/CSP и узкие типизированные команды задаются явно.
Backend проверяет project/session binding и policy; credential contents и
универсальный shell executor не предоставляются frontend. Terminal data
обрабатывается как недоверенный поток. Rust IPC adapter не заменяет
существующие ownership/authorization guards.

### Python/Rust divergence

Строгая типизация не гарантирует одинаковое поведение. Нужны дифференциальные
fixtures, одинаковая mock history и сравнение SQLite/security decisions.

### SQLite dual writers

Даже при поэтапной миграции две реализации не должны открывать одну рабочую БД.
Изоляция гарантируется раздельными state root и ownership/format marker:
одновременный запуск Python и Rust для проекта запрещён, а общая запись
отсутствует by design. Rust ведёт собственную пустую БД и отдельную историю;
перенос Python state/history в Rust не выполняется, а Python state не получает
записей от Rust.

### libgit2 против Git CLI

Ignore, worktrees, submodules и index могут отличаться. Системный Git следует
сохранить там, где его поведение является контрактом.

### Async/UI boundaries

SQLite, Git и process waits выполняются в background executor с cancellation и
bounded queues; mutex нельзя удерживать во время rendering.

### Секреты

Используются opaque secret references, redaction by type, запрет `Debug` для
secret values и тесты argv/log/error output.

## Открытые вопросы для spikes

- `rusqlite` или `sqlx`;
- MCP SDK или собственный transport adapter;
- Rust PTY crate, chunking и backpressure для Tauri channels;
- Wayland-only MVP или Wayland+X11;
- одна PTY session на окно или persistent tabs;
- поведение PTY при закрытии GUI;
- формат Linux packages.
