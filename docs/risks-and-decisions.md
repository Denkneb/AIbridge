# Риски и решения

## Принятые решения

### Поэтапная миграция

Big-bang rewrite слишком рискован для накопленной state/security логики.

### PTY вместо собственного chat protocol

Codex/OpenCode сохраняют streaming, permissions, tools и slash-команды.
Собственный chat client может стать отдельным будущим этапом.

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

### GPUI pre-1.0

Закрепить revision, обновлять отдельно и не пропускать GPUI types за границу UI.

### Embedded terminal

Terminal model не является готовым GPUI widget. Нужны rendering/input adapters,
ранний spike, compatibility fixtures и внешний fallback.

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
- `alacritty_terminal` или другой terminal engine;
- Wayland-only MVP или Wayland+X11;
- одна PTY session на окно или persistent tabs;
- поведение PTY при закрытии GUI;
- формат Linux packages.
