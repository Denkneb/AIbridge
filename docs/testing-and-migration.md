# Тестирование и миграция

## Набор проверок

### Unit

Config parsing, state transitions, command/permission policy, path confinement,
dashboard state и terminal input mapping.

### Contract

Одни fixtures выполняются Python и Rust, но каждая реализация пишет только в
собственную временную копию БД; общая рабочая БД и общий state не используются.
Сравниваются семантические JSON, exit codes, категории ошибок, SQLite rows и
process decisions. Время и UUID нормализуются.

### Property

Shell tokenization, traversal, symlink layouts, allowed paths, transitions и
idempotency. Любая ошибка разбора означает отказ, не разрешение.

### Integration

- настоящий SQLite WAL и конкурентные транзакции;
- временные Git repos, index/HEAD и external repos;
- mock OpenCode HTTP/OpenAPI;
- MCP stdio/HTTP clients;
- worker subprocess и lock contention;
- process interruption и recovery.

### GUI и terminal

View models и React components тестируются без desktop окна с mock typed IPC.
UI tests покрывают focus, actions, filters и layout states; Rust tests проверяют
IPC input validation, project/session binding, безопасные DTO и lifecycle.
Tauri desktop smoke tests проверяют реальные WebView/IPC под поддерживаемыми
Wayland/X11 окружениями; конкретный harness фиксируется после prototype.
Headless CLI отдельно собирается и тестируется без WebView.

Первый desktop spike проверяет window + terminal/dashboard split + реальный PTY.
xterm.js/Rust PTY fixtures покрывают ANSI, alternate screen, cursor, Unicode,
resize/SIGWINCH, paste, mouse modes, scrollback, порядок output chunks,
bounded buffering/backpressure, process exit и cleanup при закрытии окна.
Codex/OpenCode запускаются в compatibility smoke; mock IPC или статический
terminal screenshot не считаются доказательством PTY compatibility.

## Migration gates

1. Contract fixtures зелёные на независимых копиях каждой реализации.
2. Security decisions не ослаблены.
3. Python runtime проекта остановлен; живые locks и process records отсутствуют.
4. Новый Rust state создан как собственная пустая БД schema v6 с отдельной
   историей; Python state не импортируется и остаётся отдельным.
5. Rust state валидирован, ownership/format marker выставлен, schema v6.
6. Recovery проверен после принудительного завершения.
7. Один проект проходит soak на Rust runtime без Python fallback.
8. Только затем переключается следующий проект.

## Данные

Перед первым Rust write:

- остановить Python MCP/worker проекта;
- проверить отсутствие записи и живых locks/process records;
- создать новый Rust state: собственную пустую БД schema v6 и отдельную историю
  задач (Python state/history не импортируются и не копируются);
- выполнить Rust doctor в read-only режиме;
- выставить ownership/format marker и только затем разрешить Rust runtime.

Рабочий Python `state.sqlite` не открывается Rust-реализацией напрямую, никогда
не используется обеими реализациями и не получает записей от Rust. Contract
fixtures по-прежнему выполняются на независимых временных копиях каждой
реализации.

## Rollback

- остановить Rust runtime и worker;
- Python state не изменялся и не получал записей от Rust, поэтому Python
  запускается после doctor без обратной миграции;
- Rust state остаётся отдельным и не переносится обратно в Python state;
- при смене schema внутри Rust state восстановить Rust backup;
- никогда не запускать две реализации параллельно и не открывать чужой state.

Первые production milestones выполняются без смены schema, чтобы rollback был
простым.

## Performance

Измеряются CLI/GUI startup, idle RSS, dashboard refresh на большой истории,
`task_status` wake-up latency, terminal rendering/input latency и UI stalls.
