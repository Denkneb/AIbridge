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

View models тестируются без окна. GPUI tests покрывают focus, actions, filters
и layout states; smoke tests идут под Wayland compositor и Xvfb. Terminal
fixtures покрывают ANSI, alternate screen, cursor, Unicode, resize, paste,
mouse modes и scrollback.

## Migration gates

1. Contract fixtures зелёные на независимых копиях каждой реализации.
2. Security decisions не ослаблены.
3. Python runtime проекта остановлен; живые locks и process records отсутствуют.
4. WAL-aware копия Python state импортирована в отдельный Rust state; исходный
   Python state не изменён.
5. Rust state валидирован, ownership/format marker выставлен, schema v6.
6. Recovery проверен после принудительного завершения.
7. Один проект проходит soak на Rust runtime без Python fallback.
8. Только затем переключается следующий проект.

## Данные

Перед первым Rust write:

- остановить Python MCP/worker проекта;
- проверить отсутствие записи и живых locks/process records;
- снять WAL-aware backup SQLite (вместе с `-wal`/`-shm` или через backup API);
- сохранить schema version и checksum;
- импортировать копию в Rust state dir и выполнить Rust doctor в read-only
  режиме;
- выставить ownership/format marker и только затем разрешить Rust runtime.

Нельзя копировать только `state.sqlite`, игнорируя `-wal`/`-shm`, пока процесс
работает. Рабочий Python `state.sqlite` не открывается Rust-реализацией
напрямую и никогда не используется обеими реализациями.

## Rollback

- остановить Rust runtime и worker;
- Python state при импорте не изменялся, поэтому Python запускается после
  doctor без обратной миграции;
- Rust state остаётся отдельным и не переносится обратно в Python state;
- при смене schema внутри Rust state восстановить Rust backup;
- никогда не запускать две реализации параллельно и не открывать чужой state.

Первые production milestones выполняются без смены schema, чтобы rollback был
простым.

## Performance

Измеряются CLI/GUI startup, idle RSS, dashboard refresh на большой истории,
`task_status` wake-up latency, terminal rendering/input latency и UI stalls.
