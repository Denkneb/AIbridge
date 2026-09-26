# Тестирование и миграция

## Набор проверок

### Unit

Config parsing, state transitions, command/permission policy, path confinement,
dashboard state и terminal input mapping.

### Contract

Одни fixtures выполняются Python и Rust. Сравниваются семантические JSON,
exit codes, категории ошибок, SQLite rows и process decisions. Время и UUID
нормализуются.

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

1. Contract fixtures зелёные.
2. Security decisions не ослаблены.
3. Старая БД прочитана и обновлена на копии.
4. Recovery проверен после принудительного завершения.
5. Один проект проходит soak без Python fallback.
6. Только затем переключается следующий проект.

## Данные

Перед первым Rust write:

- остановить Python MCP/worker проекта;
- проверить отсутствие записи;
- сделать WAL-aware backup SQLite;
- сохранить schema version и checksum;
- выполнить Rust doctor в read-only режиме;
- затем разрешить Rust runtime.

Нельзя копировать только `state.sqlite`, игнорируя `-wal`/`-shm`, пока процесс
работает.

## Rollback

- остановить Rust runtime и worker;
- при неизменной schema запустить Python после doctor;
- при новой schema восстановить backup или применить backward migration;
- никогда не запускать две реализации параллельно.

Первые production milestones выполняются без смены schema, чтобы rollback был
простым.

## Performance

Измеряются CLI/GUI startup, idle RSS, dashboard refresh на большой истории,
`task_status` wake-up latency, terminal rendering/input latency и UI stalls.
