# agent-bridge (Rust)

Rust-переписывание `agent-bridge`: Cargo workspace с доменной моделью
(`bridge-domain`), загрузчиком конфигурации (`bridge-config`), read-only
инспекцией SQLite (`bridge-storage`) и CLI (`agent-bridge-cli`). Цель —
сохранить контракты CLI, MCP и SQLite, перейти к единому Rust-бинарнику и
добавить GUI на GPUI. Миграция идёт поэтапно, без одномоментной замены Python.

## Статус миграции

- Завершены этапы конфигурации **2.1–2.9** (базовый TOML loader, project ID и
  workspace validation, endpoint/port validation, уникальность
  workspace/endpoints/token files, max rounds/model/optional paths,
  auto-approve permissions, trusted external directories, credentials reader,
  project env reader).
- Завершён этап **3.1. Read-only schema inspection**: crate `bridge-storage`
  открывает существующую БД строго read-only (`mode=ro&immutable=1`),
  проверяет согласованную пару `PRAGMA user_version=6` и
  `meta.schema_version='6'` и сверяет tables/columns/indexes/foreign keys с
  schema v6, не создавая, не мигрируя и не изменяя БД.
- Завершён этап **3.2. WAL, foreign keys и busy timeout**: `bridge-storage`
  предоставляет runtime read-write подключение, которое создаёт отсутствующий
  parent directory, применяет и проверяет `PRAGMA journal_mode=WAL`,
  `PRAGMA foreign_keys=ON` и `PRAGMA busy_timeout=30000` (fail closed) и
  закрывается RAII, не создавая schema и не меняя `PRAGMA user_version`.
- Завершён этап **3.3. Task row mapping**: `bridge-storage` преобразует строку
  schema v6 таблицы `tasks` в типизированную модель `Task` (`TaskId`,
  `ProjectId`, `TaskStatus`, JSON-поля `allowed_paths`, `test_commands`,
  `snapshot`) и fail closed отвергает неизвестный статус, невалидный task
  UUID, повреждённый JSON, несоответствие SQLite-типа и отрицательный
  `revision_count`, не добавляя query/list/write API.
- Завершён этап **3.4. Round row mapping**: `bridge-storage` преобразует строку
  schema v6 таблицы `rounds` в полную типизированную storage-модель `RoundRow`
  (все 21 колонка; `TaskId`, `ProjectId`, `RoundKind`, `RoundStatus`,
  `VerifierState` и доменный `Verification`), проверяет диапазон
  `round_number` 1..=u32::MAX, строгий SQLite-`attempted` 0/1, форму
  `result_json`, декодирует `verifier_json` как `Verification` и отвергает
  несогласованную пару `verifier_state`/`verifier_json`, не добавляя
  query/list/write API и не меняя fixtures.
- Следующий этап — **3.5. Initialization совместимой пустой БД**.
- Полный план и очередь задач: [docs/implementation-plan.md](docs/implementation-plan.md).

## Документация

- [docs/README.md](docs/README.md) — обзор комплекта документов.
- [docs/implementation-plan.md](docs/implementation-plan.md) — план и статус миграции.
- [docs/architecture.md](docs/architecture.md) — целевая архитектура.
- [docs/existing-contract.md](docs/existing-contract.md) — существующий контракт.
- [docs/contract-manifest.json](docs/contract-manifest.json) — manifest контракта.
- [docs/testing-and-migration.md](docs/testing-and-migration.md) — тестирование и миграция.

## Fixtures и corpora

- Конфигурация: [docs/config-fixtures.md](docs/config-fixtures.md),
  [docs/fixtures/config-cases.json](docs/fixtures/config-cases.json)
- MCP: [docs/mcp-fixtures.md](docs/mcp-fixtures.md),
  [docs/fixtures/mcp-cases.json](docs/fixtures/mcp-cases.json)
- SQLite: [docs/sqlite-fixtures.md](docs/sqlite-fixtures.md),
  [docs/fixtures/sqlite/expected.json](docs/fixtures/sqlite/expected.json)
- Политика команд: [docs/command-policy-fixtures.md](docs/command-policy-fixtures.md),
  [docs/fixtures/command-policy-cases.json](docs/fixtures/command-policy-cases.json)
- Политика путей: [docs/path-policy-fixtures.md](docs/path-policy-fixtures.md),
  [docs/fixtures/path-policy-cases.json](docs/fixtures/path-policy-cases.json)
- Git snapshot: [docs/git-snapshot-fixtures.md](docs/git-snapshot-fixtures.md),
  [docs/fixtures/git-snapshot-cases.json](docs/fixtures/git-snapshot-cases.json)
- Мульти-репозиторная агрегация:
  [docs/multi-repository-fixtures.md](docs/multi-repository-fixtures.md),
  [docs/fixtures/multi-repository-cases.json](docs/fixtures/multi-repository-cases.json)

## Проверки

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
python3 docs/fixtures/sqlite/verify.py
git diff --check
```
