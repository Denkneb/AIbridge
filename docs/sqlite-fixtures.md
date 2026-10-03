# Fixtures SQLite `agent_bridge` (schema v6)

Машиночитаемый manifest: [fixtures/sqlite/expected.json](fixtures/sqlite/expected.json).
Это контрактные fixtures для будущих Rust-задач потока 3 (SQLite storage) и
задачи 3.10 (storage isolation и создание нового Rust state); они описывают
наблюдаемую schema и состояния существующего Python-кода и остаются immutable
эталоном семантики, а не целевой реализацией. Rust здесь не реализуется.

Fixtures намеренно детерминированы: без timestamps текущего времени, секретов,
реальных tokens/passwords и абсолютных machine-specific путей внутри БД. JSON
отсортирован по ключам, `databases` — по имени файла, строки — по
`task_id`/`round_number`. Один fixture-файл описывает одно состояние; полная
Python test suite не копируется.

## Источники

Fixtures построены по фактическому коду Python-репозитория
`/home/denis/Python/agent_bridge` (только чтение), в первую очередь:

- `src/agent_bridge/storage.py` — `SCHEMA_VERSION`, schema v6, индексы,
  invariant-ы, статусы task/round, `verifier_state`;
- `src/agent_bridge/worker.py` — форма persisted `result_json`, переходы раунда;
- `src/agent_bridge/verifier.py` — компактная форма `verifier_json`;
- `src/agent_bridge/mcp_server.py` — какие поля `result_json` читает
  `task_status`;
- `tests/test_storage.py`, `tests/test_mcp.py` — подтверждение наблюдаемых
  значений и ветвей.

Версия-источник зафиксирована в поле `source.commit`. Python-репозиторий не
изменялся.

## Файлы

| Файл | Назначение |
| --- | --- |
| `empty-v6.sqlite` | инициализированная schema v6 без tasks/rounds/events |
| `active-v6.sqlite` | одна `implementing` task и открытый initial round (`observing`) |
| `awaiting-review-v6.sqlite` | одна `awaiting_review` task, завершённый round, компактный `result_json` и `verifier_state='done'` |
| `terminal-v6.sqlite` | минимальные `accepted` и `closed` tasks с завершёнными rounds/events для проверки связей |
| `expected.json` | source commit, schema version, ожидаемые таблицы/indexes/invariants и нормализованные rows каждой БД |
| `generate.py` | автономный детерминированный regenerator (только stdlib) |
| `verify.py` | автономный read-only verifier (только stdlib `sqlite3`/`json`) |

## Schema fixtures

Все БД инициализированы до `PRAGMA user_version=6` и содержат
`meta.schema_version='6'`. Состав schema совпадает с `Storage.initialize`:

- `meta(key, value)`;
- `tasks(...)` с partial unique index `ux_tasks_active` — не более одной
  незавершённой task на проект;
- `rounds(...)` с `PRIMARY KEY (task_id, round_number)`, FK
  `task_id -> tasks(task_id)` и unique index `ux_rounds_request`;
- `events(...)` с `ix_events_task(task_id, id)`.

Ожидаемые таблицы, колонки (name/type/notnull/pk), indexes (columns/unique/
partial) и foreign keys перечислены в `expected.json.schema`; текстовые
invariant-ы — в `expected.json.invariants`.

Состояния:

| БД | tasks | rounds | events |
| --- | --- | --- | --- |
| `empty-v6.sqlite` | — | — | — |
| `active-v6.sqlite` | `task-1 implementing` | `1 observing, attempted=1` | `created` |
| `awaiting-review-v6.sqlite` | `task-1 awaiting_review` | `1 complete`, `result_json`, `verifier_state=done` | `created`, `complete` |
| `terminal-v6.sqlite` | `task-1 accepted`, `task-2 closed` | по одному `complete` round на task | `created`, `complete`, затем `accepted`/`closed` |

`terminal-v6.sqlite` намеренно минимален: round и event добавлены только там,
где они нужны для проверки FK `rounds -> tasks` и упорядоченных событий task.
Recovery-промежутки (`needs_user`, `delivery_unknown`, `failed`) и migration
fixtures (v0–v5) в набор не входят.

## Synthetic conventions

Полный словарь — в `expected.json.synthetic_conventions`. Ключевые:

- `project_id` = `proj`, `workspace` = `/fixture/workspace`;
- `task_id` = `task-<n>`, `session_id` = `ses-<n>`, `message_id` = `msg-<n>`,
  `request_id` = `req-<n>`, `payload_hash` = `hash-<request_id>`;
- timestamps — фиксированные UTC ISO-8601 с миллисекундами
  (`2026-01-01T00:00:0n.000+00:00`), без текущего времени;
- Git `head` — 40-символьный hex, fingerprints — 64-символьный hex
  (синтетические, не от реального репозитория);
- `result_json` и `verifier_json` — компактные синтетические объекты с теми же
  полями, что пишет Python (`changed_paths`, `head_before/after`,
  `repositories`, `usage`; `status`, `commands`, `before/after`, `log`).

В БД не попадают реальные данные, секреты, tokens и machine-specific пути.

## expected.json

Верхний уровень:

| Поле | Назначение |
| --- | --- |
| `fixture_version` | версия формата fixtures |
| `source` | commit, модули и tests Python-репозитория |
| `schema_version` | ожидаемый `PRAGMA user_version` |
| `schema` | ожидаемые tables/columns, indexes и foreign keys |
| `invariants` | текстовые invariant-ы schema v6 |
| `synthetic_conventions` | словарь синтетических placeholder-ов |
| `databases` | ожидаемые `user_version`, `meta`, нормализованные `tasks`/`rounds`/`events` по каждому файлу |

Нормализация rows: `tasks.allowed_paths`/`test_commands` — JSON-массивы,
`tasks.snapshot` и `rounds.result_json`/`verifier_json` — JSON-объекты или
`null`; `rounds.attempted` — boolean. `events.id` не является контрактом
(auto-increment), поэтому в `expected.json` событие сравнивается без `id`, но с
сохранением порядка вставки.

## verify.py

`verify.py` ничего не пишет: каждая БД открывается как
`file:<name>?mode=ro&immutable=1`, а при наличии `-wal`/`-shm` sidecar проверка
сразу падает. Проверяются:

1. отсутствие sidecar-файлов и совпадение набора `*.sqlite` с `databases`;
2. `PRAGMA integrity_check` и `quick_check` = `ok`;
3. `PRAGMA user_version` = 6;
4. `PRAGMA foreign_key_check` не возвращает строк;
5. таблицы, колонки (name/type/notnull/pk), indexes (columns/unique/partial) и
   foreign keys совпадают с `expected.json.schema`;
6. `meta`, `tasks`, `rounds`, `events` совпадают с нормализованными
   ожидаемыми rows;
7. semantic invariant-ы: не более одной активной task на проект, известные
   task/round статусы, `verifier_state in (null,'running','done')` и наличие
   `verifier_json` при `done`.

Код возврата: `0` — успех, `1` — есть расхождения, `2` — непригоден
`expected.json`.

## Regeneration

```sh
python3 docs/fixtures/sqlite/generate.py   # пересоздаёт БД и expected.json
python3 docs/fixtures/sqlite/verify.py     # read-only проверка
```

`generate.py` читает только собственные константы, работает на stdlib,
пересоздаёт файлы с нуля, выполняет `PRAGMA wal_checkpoint(TRUNCATE)` и
закрывает БД так, что sidecar-файлов не остаётся. Повторный запуск
воспроизводит то же семантическое содержимое. Byte-identical раскладка SQLite
(страницы, freelist, whitespace в `sqlite_master`) контрактом не является —
контракт задают `expected.json` и `verify.py`.

## Использование в дифференциальных Python/Rust tests

Fixtures immutable и описывают наблюдаемое Python-поведение. Их нельзя
изменять и нельзя использовать как общую рабочую БД двух реализаций.

1. Читать `databases` из `expected.json`.
2. Для каждой реализации скопировать нужный `*.sqlite` в её собственный
   изолированный временный state-каталог и открыть **только эту копию**
   read-write (Python `Storage.connect` включает WAL и создаёт sidecar-файлы;
   сами fixtures остаются чистыми). Python и Rust никогда не открывают один и
   тот же файл.
3. Выполнить mapping/queries и write-сценарии в Python-реализации на её копии и
   независимо в Rust-реализации на её копии.
4. Сравнить нормализованные rows и статусы каждой копии с `expected.json`;
   write-результаты не переносятся между реализациями.
5. Проверять `invariants` (уникальность активной task, FK, идемпотентность
   `request_id`, `verifier_state`).

Rust начинает с собственной пустой БД и отдельной истории; Python state/history
в Rust не импортируется, не копируется и не переносится, а Python state не
получает записей от Rust.

Такой harness воспроизводит fixtures на текущей Python suite и позже
становится differential-раннером на независимых копиях Python и Rust.

## Ограничения

- только schema v6 и четыре состояния; migrations и recovery-промежутки не
  покрываются;
- нет секретов, credentials, tokens и реальных OpenCode/Git данных;
- `result_json`/`verifier_json` — компактные синтетические примеры, а не
  полные verification-логи и output;
- `events.id` и физическая раскладка SQLite не фиксируются;
- fixtures не заменяют полную Python test suite и targeted unit-тесты.

## Delta v16/v17 (0B.3 завершён)

Отдельный набор — [fixtures/sqlite/delta/expected.json](fixtures/sqlite/delta/expected.json),
reference HEAD `e52a46158cbeb4f3ae35063d395c05ea0ce144bc` (schema17).
Исторические четыре v6 и empty-v15 fixtures и их verifiers сохранены без изменений.
Новый generator использует frozen v15 DDL и независимо заданные additive DDL v16/v17;
он не импортирует Python reference и пишет только в `delta/`.

| Fixture | Назначение |
| --- | --- |
| `delta/fresh-v17.sqlite` | fresh17, пустая история, без Rust ownership |
| `delta/owned-v15.sqlite` | синтетический Rust-owned v15: direct/worktree tasks, rounds/events, writer reservation, worktree/quarantine |
| `delta/owned-v16.sqlite` | intermediate16 с теми же историческими полями; delivery `manual` и явный `on_accept` |
| `delta/owned-v17.sqlite` | такой же populated v17 и пустой `automation_runs` |

Во всех owned fixtures `meta.runtime_owner='rust'` и дополнительная строка
`fixture_metadata='preserve-me'`. Sidecar marker задаётся template в manifest и
создаётся только рядом с временной копией, с фактическим временным state root.
Это не копии пользовательского Rust/Python state.

Контракт v16 — `tasks.delivery_mode TEXT NOT NULL DEFAULT 'manual'`.
При upgrade legacy v15 **обе** задачи, включая worktree, получают `manual`.
Существующие v16/v17 значения сохраняются: fixture с явным `on_accept` — отдельный
persisted-state сценарий, а не результат автоматического backfill legacy task.
Контракт v17 — таблица `automation_runs(run_id,status,control,document,created_at,updated_at)`;
`control` default `'run'`; partial unique expression index:

```sql
CREATE UNIQUE INDEX ux_automation_unfinished
ON automation_runs((1)) WHERE status NOT IN ('completed','ready','stopped');
```

`delta/verify.py` открывает БД с `mode=ro&immutable=1`, проверяет точные
колонки/defaults/PK/FK actions, indexes (включая expression и полный predicate),
все raw rows с event ids и оба schema markers. Проверяет integrity/quick/FK,
отсутствие WAL/SHM/journal и SHA-256 до/после чтения. Колонки сравниваются по
именам: физический ordinal при fresh/additive layout не является контрактом.

`delta/verify_parity.py` импортирует **actual pinned** `Storage` с отключённым
bytecode и проверяет HEAD/clean tree до/после. Все writable SQLite connections
reference разрешены только внутри собственного temporary directory; сеть
запрещена. Используется минимальный config double (`db_path`, `project_id`,
`allow_parallel_writers`), без чтения конфигурации/паролей/runtime state.
Проверки без skips:

- actual fresh `Storage.initialize` совпадает с independently generated fresh17;
- actual `initialize`/`_migrate` для копий owned15 и owned16 даёт exact17 schema/indexes;
- все исторические поля/строки/ids и extra meta сохраняются; v15 backfill manual,
  v16 explicit on_accept сохранён; повторная инициализация v17 идемпотентна;
- marker sidecar не изменяется; default control, PK/NOT NULL и partial uniqueness
  проверены insert/update операциями только на временных копиях;
- completed/ready/stopped допускают несколько runs; running/paused/failed/unknown
  занимают один слот; переход в ready освобождает слот;
- SQLite authorizer отказывает при создании `ux_automation_unfinished`, после
  добавления delivery column/automation table: для v15 и v16 весь upgrade
  откатывается, включая DDL/history/meta/version; повтор после отказа успешен;
- future schema18 отвергается без изменения логического содержимого;
- байты всех девяти исторических и delta fixtures не меняются.

Текущий Python всегда мигрирует прямо в17. Intermediate16 — независимо созданный
контракт для будущих Rust-owned v15→v16→v17 migrations, а не заявленный запуск
старого Python бинарника. **Rust target остаётся v15:** его ownership/schema guards
не изменены; поддержка и enforcement v16/v17, включая missing/foreign/malformed
markers и транзакционный повтор guard, остаются задачами 3.13. Python не проверяет
Rust ownership: этот harness доказывает сохранение synthetic marker/meta, а не
Rust guard acceptance. Fixtures не разрешают импорт Python history в Rust.

```sh
python3 docs/fixtures/sqlite/delta/verify.py
python3 docs/fixtures/sqlite/delta/verify_parity.py
# При намеренной регенерации только нового delta corpus:
python3 docs/fixtures/sqlite/delta/generate.py
```

Оба verifiers прошли. Повторная delta regeneration дала byte-identical SQLite и
JSON. Четыре negative harness проверки на временных копиях отвергли stale source
pin, изменённую history row, неверный partial predicate (даже с согласованным
изменением expectation) и наличие WAL sidecar. Исторический verifier (5 fixtures)
и AST manifest verifier также прошли. Rust-код в 0B.3 не изменялся.
