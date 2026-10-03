# Существующий внешний контракт `agent_bridge`

Этот документ фиксирует внешний контракт уже реализованного Python-проекта
`/home/denis/Python/agent_bridge` как базу для Rust-миграции. Он не описывает
целевую архитектуру и не является планом: единственный машинно-читаемый
источник — [contract-manifest.json](contract-manifest.json).

Актуальная привязка источника: репозиторий `/home/denis/Python/agent_bridge`,
exact Git revision `e52a46158cbeb4f3ae35063d395c05ea0ce144bc`, **Python schema
v17** (`PRAGMA user_version=17`). Manifest описывает reference Python; Rust
storage остаётся v15, historical foundations 0–6/7.1–7.6 — v6. Fixtures 0A
сохраняют source pin `86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a` и schema v15.
Config/permission delta 0B.2 и SQLite delta 0B.3 проверены отдельно;
MCP и runtime/automation delta 0B.4/0B.5 также завершены; refresh 0B завершён. Rust-код/runtime/schema и существующие
fixtures в 0B.1 не меняются.

## Назначение

Контракт нужен, чтобы будущие Rust-задачи опирались на фактическое поведение
Python-кода, а не на README или догадки. Manifest намеренно детерминирован:
без timestamps, секретов, реальных tokens/passwords и нестабильных
task/session/message ID. JSON-ключи отсортированы; массивы отсортированы там,
где порядок не является частью контракта.

## Структура manifest

| Раздел | Содержимое |
| --- | --- |
| `manifest_version`, `source` | версия формата manifest, версия пакета, repository и exact revision |
| `contract_baselines` | current reference v17, historical fixtures 0A/v15 и текущий Rust storage target v15 |
| `automation` | approved plan, durable run/control/intents, inherited checkout, read-only Codex schemas, exact verifier/review gate и final delivery |
| `scope` | что зафиксировано как совместимый контракт и что намеренно не зафиксировано |
| `cli` | имя программы, точка входа, общие options, команды, exit codes |
| `mcp` | имя сервера, ровно шесть tools с параметрами, transports, коды ошибок, internal-only детали |
| `domain` | task/round статусы, доказуемые переходы, recoverable failure, dependency gate/activation, worker error codes |
| `config` | расположение `projects.toml`, ключи проектов, дефолты и правила валидации |
| `checkpoint` | versioned per-round checkpoint (`rounds.checkpoint_json`): shape, canonical topology, bounds, predecessor rule, privacy |
| `diagnostics` | `status --json` envelope `schema_version=1`, readiness/server/process-record/worktree поля и exit codes |
| `hook` | read-only Codex `UserPromptSubmit` hook: output wrapper, context cap 1200, fail-open и privacy |
| `profiles` | builtin/custom профили, snapshot-контракт, limits и коды ошибок |
| `storage` | `PRAGMA user_version`, migration map, tables, columns, indexes, invariants |
| `worktree` | execution root, checkout/runtime пути, статусы и ограничения |
| `delivery` | artifact/journal пути, modes, states, refusal codes и ограничения |
| `paths` | config/state/runtime/worktree/delivery/quarantine пути |
| `runtime` | entry points, locks и process ownership records |
| `security` | suspected-secret categories, worktree path policy |
| `opencode_api` | HTTP API OpenCode, от которого зависит adapter |
| `environment` | имена service env vars и список tuning overrides (не контракт) |

## Источники

Manifest построен по фактическому коду и тестам Python-репозитория, в первую
очередь:

- `src/agent_bridge/cli.py` — CLI-команды, exit codes, wiring Codex/OpenCode;
- `src/agent_bridge/add_project.py` — `add-project` dry-run/apply;
- `src/agent_bridge/prune.py`, `quarantine.py` — история, quarantine/purge;
- `src/agent_bridge/delivery.py` — artifact/journal и `deliver-task`;
- `src/agent_bridge/mcp_server.py` — шесть MCP tools, параметры и коды ошибок;
- `src/agent_bridge/mcp_http.py` — authenticated Streamable HTTP transport;
- `src/agent_bridge/storage.py` — schema v17, additive migration map v6→v17, tables,
  indexes, invariants, статусы и атомарные переходы;
- `src/agent_bridge/worker.py` — task/round transitions, locks, worker error codes;
- `src/agent_bridge/automation.py`, `automation_checkout.py`, `codex_client.py`
  — approved plan, run lifecycle, inherited artifact и independent review;
- `src/agent_bridge/config.py` — `projects.toml` keys, `delivery_mode`,
  `auto_approve_state_directory`, `execution_mode`,
  `max_active_tasks`, `allow_parallel_writers`, `default_profile`;
- `src/agent_bridge/profiles.py` — builtin/custom profiles и snapshot;
- `src/agent_bridge/secret_scanner.py` — suspected-secret categories;
- `src/agent_bridge/git_worktree.py`, `worktree_runtime.py` — task-scoped checkout
  и server;
- `src/agent_bridge/runtime.py` — start/status/stop, locks, process records;
- `src/agent_bridge/diagnostics.py`, `hook_status.py` — readiness snapshot и hook;
- `src/agent_bridge/opencode_client.py` — обязательные операции OpenCode API;
- `tests/` — подтверждение наблюдаемых кодов ошибок и переходов.

Поле `evidence` у каждого перехода ссылается на модуль и функцию, а не на номер
строки, чтобы ссылка не устаревала при рефакторинге.

## Публичные границы контракта

Совместимыми считаются:

- имена и опции CLI-команд (`add-project`, `prune` включая `--quarantine`,
  `deliver-task`, `status --json`, `hook-status`, `smoke-opencode`, worker и
  task-scoped `serve-opencode-worktree`), а также exit codes;
- имена, параметры и коды ошибок ровно шести MCP tools
  (`project_info`, `submit_task`, `task_status`, `request_changes`,
  `accept_task`, `close_task`), включая `budget`, `workflow_id`, `depends_on`,
  `profile`, `allow_suspected_secrets`, `structured_findings`,
  `allow_budget_override`;
- stdio и authenticated HTTP transports и runtime entry points;
- статусы task/round, dependency waiting gate и доказуемые переходы; активация
  `waiting_dependencies` происходит только через явный `task_status`, даёт
  `implementing` и не имеет scheduler/startup auto-activation. Её gates: принятые
  зависимости, admission-лок, свободный worker-слот (либо scope-disjoint
  parallel), отсутствие close request. Direct-mode baseline пересчитывается и
  разрешает грязный baseline только при собственном `allow_dirty=true` задачи
  (чужой policy не наследуется); dirty вне scope и недоступный workspace
  блокируют. Worktree-mode использует персистентный `base_head` своего чистого
  checkout, main-workspace не перебазируется;
- `execution_mode` (`direct`/`worktree`), `max_active_tasks`,
  `allow_parallel_writers` и профили (`default_profile`, custom profiles,
  immutable snapshot);
- SQLite v17: расположение, `PRAGMA user_version`, tables, columns, indexes и
  invariants, включая финальный partial unique `ux_active_writers_single` и
  отсутствие исторических `ux_tasks_active`/`ux_active_writers_project`;
- worktree execution root, task-scoped checkout/server и delivery
  artifact/journal;
- versioned per-round checkpoint (`rounds.checkpoint_json`, version 1): fixed
  fail-closed shape, canonical `workspace`/`external N` topology, bounds
  (repositories 64, entries 200, path 300, digest 128, exact state 2000,
  count 1000000), отсутствие truncation exact predecessor state, delta только к
  непосредственно предыдущему round, content/path privacy;
- `status --json`: envelope `{schema_version, projects}`, readiness/server/
  process-record/snapshot/worktree поля, exit 0 при полной готовности и 1 иначе;
- read-only Codex `UserPromptSubmit` hook: обёртка
  `hookSpecificOutput.hookEventName`, только `additionalContext`, без blocking
  decision, cap 1200 символов, fail-open на idle/ошибках и запрет на утечку
  task text/result/findings/session id/приватных путей/credentials;
- `projects.toml`: расположение, ключи проектов и правила валидации;
- config/state/runtime/worktree/delivery/quarantine пути;
- suspected-secret categories и политика `allowed_paths` для worktree;
- поверхность OpenCode HTTP API, от которой зависит adapter.

## Намеренно не зафиксировано

Следующие детали считаются implementation details и могут отличаться в Rust:

- человекочитаемые тексты сообщений, ошибок и логов;
- внутренняя раскладка Python-модулей, имена классов и функций;
- физический порядок columns и внутренние имена indexes за пределами
  задокументированной уникальности;
- содержимое полных verification-логов и bytes вывода команд;
- конкретные значения `task_id`, `session_id`, `message_id` и timestamps;
- внутренние JSON-структуры сообщений/parts OpenCode за пределами
  задокументированных полей;
- дефолты worker polling/timeout и переменные окружения `AB_*` (они помечены в
  manifest как `implementation_overrides_not_frozen`);
- internal-only флаги (`needs_user_recovery`, `allow_waiting_activation` и
  подобные) — они не являются аргументами публичных MCP tools.

## Границы и ограничения

- Document не включает секреты, реальные tokens/passwords и timestamps.
- Manifest описывает reference Python v17 и **не** заявляет Rust parity:
  текущий Rust storage v15; historical foundations v6 сохраняют свой scope.
  Manifest-обновление не меняет Rust code/runtime/schema/fixtures.
- Python и Rust владеют отдельными runtime state/SQLite/history; canonical
  `projects.toml` общий только для чтения, запись — только активной реализацией
  при остановленном runtime. Доступ к Python runtime DB не подразумевается.
- Переходы сверяются с текущим source: atomic needs_user recovery теперь
  сохраняет observing и spawn lease перед запуском worker. wait=0 допускает
  один needs_user recovery, но не failed recovery. Automation provenance
  остаётся внутренним input, не расширяет публичные MCP signatures.
- Явно вынесенные limitations (не implemented): worktree external directories/
  submodules/LFS/sparse/nested repos/environment setup; полный patch archival;
  A2A/auto merge/commit/push/deploy. Approved plan coordinator и on_accept
  delivery реализованы opt-in. Resumable delivery может оставить частично
  материализованное состояние при crash без automatic rollback; parallel
  startup сериализуется через runtime.lock с bounded wait 60s, ordinary
  start/status/stop остаются nonblocking. Smoke доказывает concurrent execution
  через model rendezvous и staggered startup, а не simultaneous spawn.
- `docs/fixtures/sqlite/verify.py` проверяет fresh v15 и отдельные legacy v6
  fixtures. Source parity и Rust-owned additive upgrade проверяет
  `docs/fixtures/sqlite/parity/verify_parity.py`; это не импорт Python state.

## Delta 0B.1 и проверка manifest

`launch-codex --auto --plan` и automation-status/pause/resume/stop работают с
approved JSON plan; automation-worker — private coordinator entry point.
`delivery_mode=manual|on_accept` фиксируется на task, on_accept требует
worktree. Project config mode и frozen task mode — разные наблюдаемые значения.
CLI dry-run/build/apply, task manual/on_accept и plan apply/manual — отдельные
контракты. Повтор accept undelivered on_accept возобновляет journal; manual и
уже delivered ответы сохраняют идемпотентность. accepted не означает delivered.

State-directory opt-in добавляет только permission root: external Git boundaries
и linked projects не расширяются. Controller сохраняет edit/task deny и bash
ask. Automation model calls read-only; публичный accept managed task требует
passed current verifier и saved positive review того же round/fingerprint.
plan delivery=manual завершается ready; completed требует durable delivered.

Проверка: `python3 docs/verify_contract_manifest.py` (можно передать `--source`
или `--manifest`). Проверяет точный HEAD/clean tree, полный CLI options set,
шесть MCP signatures/defaults и internal-only inputs, config defaults, literal
DDL tables/columns/defaults/keys/index predicates, Codex output schemas и
runtime bounds. Использует только AST source и SQLite `:memory:`: не импортирует
Python приложение, не запускает его migrations/models и не открывает runtime
SQLite/history. Это source-contract проверка, не запуск Python suite и не
доказательство runtime/model parity. Historical fixture verifier-ы используют
свои pins; config/permission, SQLite, MCP и runtime/automation delta 0B.2–0B.5 выполнены; refresh завершён.
