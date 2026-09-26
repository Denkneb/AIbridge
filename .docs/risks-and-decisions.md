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

### projects.toml — источник истины

CLI и GUI не должны расходиться или использовать разные базы настроек.

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

Две реализации могут нарушить invariant одной активной задачи. Нужны process
locks, read-only migration phase и запрет одновременного запуска.

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
