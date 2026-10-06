# Тестирование и миграция

## Набор проверок

### Исполнимая матрица 15.4

На границе потока запускается полный Rust workspace и self-contained SQLite corpus:

```sh
python3 -B tools/check_matrix.py --fixtures-only --offline
```

Полная source-parity matrix требует чистый read-only checkout Python HEAD
`e52a46158cbeb4f3ae35063d395c05ea0ce144bc` и Python >=3.11 с зависимостями
frozen `pyproject.toml` (включая dev group):

```sh
python3 -B tools/check_matrix.py \
  --reference /home/denis/Python/agent_bridge --offline
```

Runner создаёт временные Git clones для v15 и v17, не переключает HEAD
источника, не открывает Python runtime SQLite/history и запрещает запись
Python bytecode. Если у источника есть `.venv`, используется её интерпретатор.
Все verifier-ы запускаются, даже если один suite не прошёл. Логи и
`summary.json` сохраняются в `target/check-matrix`; `--logs PATH` меняет место,
`--skip-rust` допускается, когда workspace уже проверен отдельно. Exit 2 —
infrastructure/config failure, exit 1 — failed selected suite. 17 известных
legacy-v6 MCP skips выводятся отдельно и не засчитываются как parity passes;
strict v15/v17 corpus не допускает skips.

CI всегда запускает Rust checks и self-contained corpus. Job всех source
verifier-ов включается repository variable `PYTHON_REFERENCE_REPOSITORY`;
для закрытого reference нужен secret `PYTHON_REFERENCE_TOKEN` с read-only
доступом. Checkout закреплён на v17 HEAD, fetch-depth=0 сохраняет v15 pin.
Job сохраняет логи как artifact. Без этой настройки CI не заявляет source
parity. Реальные model calls в CI не выполняются.

`bridge-automation/tests/proof.rs` покрывает production workflow
fix→consumer→final с crash/reopen и parallel worker overlap; локальные
HTTP/model doubles позволяют выполнять проверки без внешних аккаунтов.
Direct/worktree/linked runtime, manager lock и server reuse проверяются
workspace integration tests. CLI service tests добавляют main OpenCode +
настоящий Rust MCP lifecycle и rollback.

Optional реальный parallel smoke подготовлен отдельно:

```sh
python3 -B tools/live_parallel_smoke.py \
  --bridge "$PWD/target/debug/agent-bridge" \
  --opencode /absolute/path/to/opencode \
  --auth-source /absolute/path/to/auth.json \
  --output /tmp/parallel-evidence.json
```

Он использует два временных worktree, две небольшие модельные задачи,
loopback rendezvous и manual acceptance. Auth копируется в private временный
XDG root, после cleanup остаётся sanitized evidence. Запуск требует разрешённой
отправки тестовых prompts внешнему provider и оплачиваемой квоты; локальный
model-double proof не означает успешного live smoke.

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

## Bounded real-model automation proof

```sh
python3 tools/live_automation_smoke.py \
  --bridge /absolute/agent-bridge --opencode /absolute/opencode --codex /absolute/codex \
  --opencode-auth /absolute/opencode-auth.json --codex-auth /absolute/codex-auth.json \
  --output /tmp/live-automation.json
```

Runs a fixed two-step plan and final verifier with private copied credentials,
independent real Codex review, OpenCode worktrees and cumulative delivery.
Maximum workflow time is 600 seconds; only safe labels and booleans survive.
Uses provider tokens; crash/revision/control cases remain in offline proofs.
