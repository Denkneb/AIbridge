# План реализации

## Правила декомпозиции

План выполняется короткими задачами. Одна задача:

- меняет один компонент или один сквозной контракт;
- обычно затрагивает не более 3–5 логически связанных файлов;
- имеет один основной критерий готовности;
- содержит собственный короткий контекст;
- завершается targeted tests, а не полной матрицей без необходимости;
- не смешивает рефакторинг, новую возможность и миграцию данных;
- не меняет schema, public API и security policy одновременно.

Если задача содержит два независимых результата, её следует разделить.

## Шаблон задачи

```text
Название:
Цель:
Контекст: только нужные документы и fixtures
Разрешённая область: точные каталоги/файлы
Не входит в задачу:
Критерии приёмки:
Targeted checks:
Зависит от:
Открывает следующие задачи:
```

Контрактные fixtures и короткие ADR должны быть самодостаточными. Следующая
задача читает результат предыдущей, а не историю её обсуждения.

## Источник истины и расхождение версий

- Проверенный текущий источник (READ-ONLY): `/home/denis/Python/agent_bridge`,
  HEAD `e52a46158cbeb4f3ae35063d395c05ea0ce144bc`
  (`feat: add autonomous plan execution with Codex review`), рабочее дерево
  чистое при сверке 2026-10-03. Текущая Python schema — **v17**
  (`storage.py:41`); v16 добавляет frozen `tasks.delivery_mode`, v17 —
  `automation_runs`. Изменения сверены по исходникам и Git diff, Python suite
  в этой сверке не запускалась. Runtime SQLite/history не читались,
  не импортировались и не изменялись.
- **Зафиксированный контракт 0A остаётся v15**, HEAD
  `86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a`.
  `docs/contract-manifest.json` обновлён до v17 в 0B.1; fixtures 0A сохраняют
  свой v15 pin. Config/permission delta 0B.2 и SQLite delta 0B.3 проверены отдельно;
  MCP и runtime/automation delta 0B.4/0B.5 также завершены. Завершённые результаты 0A/1.6–1.7/2.10–2.12/3.12 относятся
  именно к этому baseline; новые source line references ниже помечены v17,
  старые line references читаются на frozen v15 commit.
- Rust создаёт собственный **v17** state после 3.13c; поддержанные Rust-owned
  v6/v11/v14/v15/v16 обновляются транзакционно. Read-only v15 не мигрируется.
  Schema guards v16/v17 реализованы в 3.13a/c. Python migrations (0..16) не становятся Rust allowlist; upgrades
  допускаются только для явно поддержанных Rust-owned contracts.
- Завершённый Rust foundation (этапы 0–6 и 7.1–7.6) опирается на **старый
  контракт schema v6**: исторический manifest до refresh,
  `docs/fixtures/sqlite/*-v6.sqlite` и `docs/fixtures/sqlite/expected.json`.
  Завершённый 0A manifest описывал v15; текущий manifest описывает v17,
  historical fixture pins не переписаны.
  Эти этапы остаются честно завершённым **foundation v6**, а не паритетом с
  современным Python; их исторические описания сохраняются без переписывания.
- Современный Python/Rust parity целиком не заявлен. Standalone MCP поддерживает
  structured findings, budgets, workflow/dependencies, checkpoints, worktree
  execution, executor profiles, recovery и frozen on_accept delivery. Local
  controller и реальный provider/worktree/verifier/delivery smoke проверены
  2026-10-05; [отчёт](live-smoke.md). Parallel services покрыты fixtures,
  live parallel matrix 15.3 остаётся открытой. Diagnostics/hook, config migration,
  automatic plan coordinator и GUI остаются отдельными незавершёнными потоками.
- **Поток 0A завершён:** manifest и config/MCP/SQLite/security/runtime corpus
  зафиксированы от Python v15. **1.6 и 1.7 завершены** (pure domain),
  **2.10–2.12 завершены** (config execution_mode/admission settings/profiles).
  **3.12a завершён** (fresh v15 и guarded additive v6→v15).
  **3.12b завершён** (historical writer indexes и v11/v14 upgrade).
  **3.12c завершён** (explicit activation и fenced baseline refresh).
  **3.12d завершён** (writer reservations/scope admission/reconcile).
  **3.12e завершён** (worktrees/quarantine lifecycle).
  **3.12f завершён** (budget persistence/parse).
  **0B.1 завершён** (manifest и source verification v17).
  **0B.2 завершён** (config/permission delta fixtures, 87 source-parity cases).
  **0B.3 завершён** (4 SQLite delta fixtures, actual v17 migration parity).
  **0B.4 завершён** (47 MCP/claim source-parity scenarios, без skips).
  **0B.5 завершён** (77 runtime/automation cases, без skips).
  **Delta fixtures 0B завершены.** **7.13 завершён** (structured findings validation/persistence/dispatch).
  Согласованный блок 7.14 → 7.15 → 8.18 → 7.18 завершён.
  **7.16 завершён** как исполнимый блок application services (checkout, task runtime,
  revision/cwd, close/recovery/quarantine); manager lock **9.7a завершён**.
  **7.17a завершён** (B1 admission/activation services).
  **7.17b services завершены**, live-model smoke остаётся в 15.3.
  **7.17c service завершён**.
  **Delivery gate 7.17d и 9.18a–e завершены**; CLI и MCP on_accept подключены.
  **1.8, 2.13, 2.14, 3.15, question blocker 7.7 и recovery services 7.10a/b завершены**;
  schema target v17; 3.13a–c и automation run storage 3.14 завершены; полный worker FSM,
  CLI/MCP adapters и новые v16/v17 задачи идут по зависимостям.
  Исторический 7.7 сохраняет foundation v6 scope; изменённые recovery и
  permission контракты используют delta fixtures 0B.

## Изменения reference после завершённого 0A (2026-10-03)

Сверка `86c65b5..e52a461`: 3 коммита, 29 изменённых файлов. Это новый
контракт v16/v17, а не только уточнение line numbers.

| Коммит | Изменение | Задачи Rust |
| --- | --- | --- |
| `d298478` | on-accept delivery, state-directory permissions, atomic needs_user recovery | 1.8, 2.13–2.14, 3.13a–b, 3.15, 7.8, 7.10, 8.19–8.20, 9.18e |
| `f7cae1a` | bounded manager-lock wait для worktree startup | 9.7a, 7.16b, 15.3 |
| `e52a461` | approved plan, detached coordinator, Codex review, inherited checkout, schema17 | 3.13c, 3.14, поток 16, 15.5 |

Delta реализация остаётся открытой; **0B.1–0B.5 завершены**: manifest v17,
AST/source verification, config/permission, SQLite, MCP и runtime/automation delta corpora.
Fixtures 0A и Rust implementation сохраняют v15 baseline. Contract delta
refresh завершён; 7.13 завершён; 7.14 завершён; 7.15, 8.18 и 7.18 завершены; следующие consumers — worktree execution/MCP/runtime.

## Поток 0. Контрактная база

### 0.1. Manifest внешнего контракта

Перечислить CLI-команды, MCP tools, статусы, SQLite version и runtime paths.

### 0.2. Fixtures конфигурации

Зафиксировать valid/invalid `projects.toml` и категории ошибок.

### 0.3. Fixtures MCP

Сохранить ответы tools для значимых статусов, включая compact/verbose status.

### 0.4. Fixtures SQLite

Подготовить небольшие БД для empty, active, review и terminal states.

### 0.5. Security corpus

Выделить command, path, symlink и Git policy cases с allow/deny результатом.

**Готовность потока:** fixtures воспроизводятся текущей Python suite.

## Поток 0A. Refresh контракта до schema v15 (завершён)

**Зачем.** Python уже schema v15 и добавил public/config/storage/runtime/
security/UI возможности, которых нет в Rust-плане. Поток 0A фиксирует источник
v15 и обновляет согласованные contract manifest и fixtures **до** 7.7.

**Результат 0A.1–0A.6:** manifest v15; config corpus (121 cases), MCP corpus
(68 strict target-v15 cases), fresh SQLite v15 и отдельные legacy v6 fixtures
(97 source-parity checks), security corpus (51 cases), runtime corpus (29
cases). Config/security/runtime/SQLite проходят без пропусков; MCP legacy-v6
проверяется отдельно и имеет 17 явно отмеченных skips, которые не считаются
доказательством parity. Rust runtime/schema остаются foundation v6.

Новые corpus и команды проверки описаны в [security fixtures](security-fixtures.md)
и [runtime fixtures](runtime-fixtures.md). Source HEAD совпал с зафиксированным;
Python runtime state/history не читались и не изменялись.

**Изменения Python относительно v6 (additive, `storage.py:1908-2091`):**
v7 `rounds.structured_findings`; v8 `tasks.budget_json`; v9
`tasks.workflow_id`/`depends_on`; v10 `tasks.execution_mode` +
`worktrees`/`worktree_quarantine`; v11 `waiting_dependencies` в промежуточном
partial unique `ux_tasks_active` (исторический, удаляется на v15); v12
`rounds.checkpoint_json`; v13 `tasks.profile`/`profile_json`/
`profile_hash`/`profile_source`; v14 `active_writers` + промежуточный
`ux_active_writers_project` (исторический, удаляется на v15); v15
`active_writers.parallel` + partial unique
`ux_active_writers_single WHERE parallel=0`. Финальный fresh v15 **не**
содержит writer-status UNIQUE `tasks(project_id)`. Python-миграции v6→v15
**additive и не означают чтение/миграцию Python runtime state**: Rust, как и
сейчас, начинает отдельную пустую БД и обновляет только Rust-owned state;
ownership guard/isolation (3.10/3.11) сохраняются. Поэтому «fresh schema
target» (новая пустая v15) и «Rust-owned additive upgrade/legacy test
fixtures» (только Rust-owned legacy v6 для проверки аддитивной миграции)
разграничиваются и не смешиваются.

### 0A.1. Contract manifest v15 (завершено)

- **Цель:** обновить `docs/contract-manifest.json` до `schema_version=15` и
  зафиксировать новые CLI/MCP/config/runtime-поверхности.
- **Source evidence:** `storage.py:41,1689-2091`; `cli.py:360-639`;
  `mcp_server.py:2560-2709`; `config.py:44-56,249-466`.
- **Содержание:** CLI-команды `add-project`, `prune` (history + `--quarantine`),
  `deliver-task`, `status --json`, `hook-status`, `smoke-opencode`
  (`--worktree`/`--parallel-worktrees`), `serve-opencode-worktree`, `worker`,
  `console`/`attach-opencode --task`; MCP-инструменты
  `project_info`/`submit_task`/`task_status`/`request_changes`/`accept_task`/
  `close_task` с новыми параметрами (`budget`, `workflow_id`, `depends_on`,
  `profile`, `allow_suspected_secrets`, `allow_budget_override`); runtime-пути
  `worktrees/`, `admission.lock`, `workers/<task_id>.lock`.
- **Не входит:** Rust-код и runtime state.
- **Критерии приёмки:** manifest воспроизводит v15-поверхность и согласован с
  Python-источником; `schema_version=15`.
- **Targeted checks:** `python3 docs/fixtures/sqlite/verify.py`;
  `git diff --check`.
- **Зависит от:** фиксации источника v15.
- **Открывает:** 0A.2–0A.6 и schema/domain/config foundations.

### 0A.2. Config fixtures v15 (завершено)

- **Цель:** добавить valid/invalid cases для `execution_mode`,
  `max_active_tasks`, `allow_parallel_writers`, `default_profile`/custom
  profiles.
- **Source evidence:** `config.py:44-56,249-466`;
  `tests/test_config.py:286-390,800-866`.
- **Содержание:** `execution_mode` absent→`direct`, `worktree`, invalid;
  `max_active_tasks` positive int (bool/0/negative rejected);
  `allow_parallel_writers=true` только при `execution_mode="worktree"`;
  default/custom profile resolution.
- **Не входит:** реализация config в Rust.
- **Критерии приёмки:** config corpus семантически совпадает с Python v15.
- **Targeted checks:** config-cases parity; `git diff --check`.
- **Зависит от:** 0A.1.
- **Открывает:** config foundation потребителей.

### 0A.3. MCP fixtures v15 (завершено)

- **Цель:** обновить ответы tools для новых статусов/полей.
- **Source evidence:** `mcp_server.py:174-351,354-688,938-948,1124-1131,
  1973-2163,2192-2859`; `tests/test_mcp.py` (structured findings 1963-2291,
  budget 571-971, workflow 1030-1740, secrets 345-539, parallel 6748-6793).
- **Содержание:** structured findings на `request_changes`, budget/warning/
  exhausted, workflow/dependency gate, `suspected_secrets` categories-only,
  `execution_mode`, `project_info` active set, ID-less `task_status` →
  `ambiguous_task`, compact `phase`/`verification_progress`.
- **Не входит:** Rust-код и runtime state.
- **Критерии приёмки:** MCP corpus эквивалентен Python v15.
- **Targeted checks:** mcp-cases parity; `git diff --check`.
- **Зависит от:** 0A.1.
- **Открывает:** MCP foundation потребителей.

### 0A.4. SQLite fixtures v15: fresh target и Rust-owned upgrade/legacy (завершено)

- **Цель:** подготовить **fresh schema target** (пустая v15) и отдельно
  **Rust-owned additive upgrade/legacy** fixtures (v6, только Rust-owned), не
  читая и не мигрируя Python runtime state.
- **Source evidence:** `storage.py:41-72,1689-2091`;
  `tests/test_storage.py:221,234,277,318,397,634,652,685,808,1145,1210,1832,
  2701,3094,3176`.
- **Содержание:** fresh v15 DDL/columns/indexes; additive v6→v15 migration
  (structured_findings, budget_json, workflow/depends_on, execution_mode +
  worktrees/worktree_quarantine, waiting status, checkpoint_json, profile*,
  active_writers, parallel). **Целевые индексы v15:** partial unique
  `ux_active_writers_single` (`active_writers(project_id) WHERE parallel=0`) +
  nonunique lookup `ix_tasks_project_status`/`ix_active_writers_project`;
  промежуточные `ux_tasks_active` (v11/v14) и `ux_active_writers_project` (v14)
  **DROP**-аются на v15 (`storage.py:2049-2086`). Writer-status UNIQUE
  `tasks(project_id)` в v15 отсутствует, иначе B2 parallel writers запрещены.
  `meta.schema_version='15'`, `PRAGMA user_version=15`.
- **Разграничение:** fresh target — новая пустая Rust-owned v15; legacy v6 —
  только для Rust-owned upgrade/legacy-read тестов; Python state не участвует.
- **Не входит:** чтение Python runtime SQLite и изменение Rust-кода.
- **Критерии приёмки:** fresh v15 и legacy v6 различаются явно; fresh v15
  допускает несколько disjoint parallel writers и не содержит
  `ux_tasks_active`; ownership guard/isolation не ослаблены.
- **Targeted checks:** `python3 docs/fixtures/sqlite/verify.py`;
  `git diff --check`.
- **Зависит от:** 0A.1.
- **Открывает:** schema/domain/config foundations (domain 1.6/1.7, config
  2.10–2.12, storage 3.12).

### 0A.5. Security corpus v15 (завершено)

- **Цель:** обновить security corpus новыми политиками.
- **Source evidence:** `secret_scanner.py:29-134`; `mcp_server.py:764-780,
  2297-2299,2735-2740`; `mcp_server.py:2259-2271`;
  `git_worktree.py:455-497`; `storage.py:282-419`.
- **Содержание:** suspected-secret categories (`aws_access_key`,
  `github_token`, `openai_api_key`, `anthropic_api_key`, `google_api_key`,
  `slack_token`, `stripe_token`, `jwt`, `pem_private_key`) и
  `allow_suspected_secrets`; worktree path policy (absolute external
  `allowed_paths` запрещены, submodules/LFS/sparse fail closed); canonical
  symlink-resolved file/directory scope overlap; corrupt scope fail closed.
- **Не входит:** изменение schema/public API и Rust security implementation.
- **Критерии приёмки:** security corpus совпадает с Python v15.
- **Targeted checks:** security-cases parity; `git diff --check`.
- **Зависит от:** 0A.1.
- **Открывает:** security consumers.

### 0A.6. Runtime/readiness fixtures v15 (завершено)

- **Цель:** зафиксировать `status --json`, diagnostics snapshot, verifier
  phase/progress и hook-status.
- **Source evidence:** `diagnostics.py:41-523`; `runtime.py:515-587`;
  `storage.py:32-39,673,3067`; `hook_status.py:19-132`;
  `cli.py:369-376,1402-1433`.
- **Содержание:** `DIAGNOSTICS_SCHEMA_VERSION=1`, per-project readiness +
  `worktree_summary` + `active_writers`; safe phase `agent|verifying` и
  `verification_progress` (`state`/`command_index`/`command_count`); read-only
  fail-open Codex hook без raw prompt text.
- **Не входит:** реализация runtime services в Rust.
- **Критерии приёмки:** runtime corpus совпадает с Python v15.
- **Targeted checks:** diagnostics parity; `git diff --check`.
- **Зависит от:** 0A.1.
- **Открывает:** runtime/diagnostics consumers.

**Готовность потока 0A:** manifest и все группы fixtures зафиксированы от
источника v15; fresh target и Rust-owned upgrade разграничены; schema/public
API/security policy не объединены в одну задачу. Только после этого
начинаются schema/domain/config foundations, затем потребители.

## Поток 0B. Refresh текущего reference до schema v17 (завершён)

Это delta к завершённому 0A, с сохранением v6/v11/v14/v15 исторических
fixtures и результатов. Python source доступен read-only; все тестовые БД,
worktrees, configs и subprocess doubles создаются в synthetic Rust fixtures.

### 0B.1. Manifest и source pin v17 (завершено)

- **Цель:** зафиксировать HEAD `e52a46158cbeb4f3ae35063d395c05ea0ce144bc`,
  CLI/config/MCP/storage delta и новые automation paths.
- **Source:** `storage.py:41,190,1748,2114-2131`, `cli.build_parser`,
  `config.ProjectConfig`, `automation.py`, `codex_client.py` (current v17).
- **Приёмка:** schema17 target явно отделён от текущего Rust v15; delivery
  config/task modes отделены от plan `delivery=apply|manual`; automation
  inheritance/run id — внутренние inputs, не новые public MCP arguments.
- **Результат:** manifest v17, `contract_baselines` явно отделяет Rust v15
  и historical fixtures v15; 25 CLI commands, 6 public MCP tools, 8 tables и
  6 indexes; frozen delivery policy, permission opt-in, lease recovery,
  automation plan/run/provenance/review/delivery и private runtime paths.
- **Проверки:** `python3 docs/verify_contract_manifest.py`: pinned clean source,
  AST CLI/options/MCP signatures/defaults, config defaults, literal DDL в RAM
  SQLite, exact columns/defaults/PK/FK/index predicates, Codex output schema и
  runtime bounds. Negative checks: stale pin, missing CLI, missing delivery
  column, wrong automation predicate, public automation input — reject.
  Python app/suite/models/runtime state не запускались; historical SQLite
  fixture verifier и diff check прошли. Открывает 0B.2–0B.5.

### 0B.2. Config и permission fixtures (завершено)

- **Цель:** defaults/invalid values для `delivery_mode` и
  `auto_approve_state_directory`; controller и worker policy cases.
- **Source:** `tests/test_config.py`, `tests/test_launchers.py`,
  `tests/test_worker.py` (v17).
- **Приёмка:** on_accept только worktree; state opt-in не расширяет trusted
  external Git/linked-project roots; `/`, `*`, `?`, symlink/traversal/glob
  escape отвергаются в соответствующих config/policy boundaries.
- **Результат:** `docs/fixtures/config-permission-v17.json`, отдельный verifier
  `docs/fixtures/config/verify_v17.py`; historical v15 corpora/pins сохранены.
  87 cases: config35 + immutable1 + no-state-creation1 + controller6 +
  worker42 + boundaries2. Реальные load_project/build_controller_config/
  _permission_decision/load_linked_projects/validate_allowed_paths вызываются
  только на synthetic temporary dirs/repos; SQLite/network запрещены,
  bytecode выключен, source HEAD/import path/clean tree проверены.
- **Проверки:** **87/87 source-parity cases, no skips**; negative harness
  cases stale pin/duplicate ids/false escape allow/numeric bool reject,
  manifest source verifier, historical SQLite verifier и diff check.
  Rust config/security implementation не меняется. Depends on 0B.1;
  открывает 2.13/2.14/7.8a/b. SQLite delta 0B.3 также завершён.

### 0B.3. SQLite v16/v17 fixtures (завершено)

- **Цель:** fresh17, additive Rust-owned v15→v16→v17 и intermediate16;
  описание delivery default и automation partial unique index.
- **Source:** `Storage.initialize`, `Storage._migrate`,
  `tests/test_storage.py`, `tests/test_automation.py::test_schema_seventeen_upgrades_v16_additively`.
- **Приёмка:** v15 fixtures не переписываются; legacy task backfill manual;
  `automation_runs` и exact index predicate проверяются; ownership guards,
  rollback и read-only inspection не ослабевают.
- **Проверки:** SQLite schema/fixture verifier. Зависит от 0B.1.
- **Результат:** отдельные `docs/fixtures/sqlite/delta/{fresh-v17,owned-v15,owned-v16,owned-v17}.sqlite`;
  independent generator/expectations, immutable inspection и actual pinned
  `Storage.initialize`/`_migrate` parity на temporary copies. Полная история,
  event ids, extra meta и synthetic Rust ownership сохраняются; legacy manual,
  explicit on_accept, exact automation expression/predicate/default проверены.
  Failure injection после additive DDL доказывает rollback v15/v16 и успешный
  retry; reinitialize идемпотентен, future18 отвергается, сети/runtime state нет.
  Intermediate16 независимо построен: текущий Python мигрирует прямо в17.
  Rust v16/v17 guards/migrations не реализованы и остаются задачами 3.13.
- **Верификация:** оба delta verifiers прошли без skips; byte-identical повторная
  generation; 4 negative harness cases (stale pin/history/predicate/sidecar)
  отвергнуты; historical SQLite (5 fixtures), manifest и diff check прошли.
  Подробнее: `docs/sqlite-fixtures.md`. MCP delta 0B.4 также завершён.

### 0B.4. MCP delivery/recovery fixtures (завершено)

- **Цель:** first/repeat accept, отказ/partial apply/current state,
  zero-wait recovery, spawn lease и automation-managed actions.
- **Source:** `tests/test_delivery_on_accept.py`, `tests/test_mcp.py`,
  `tests/test_storage.py` (v17).
- **Приёмка:** manual ответы сохраняются; accepted ≠ delivered; wait=0 может
  восстановить needs_user, но не failed; concurrent claims дают один spawn;
  unknown/state_unavailable и private-path redaction представлены явно.
- **Проверки:** MCP source-parity corpus. Зависит от 0B.1.
- **Результат:** отдельный `docs/fixtures/mcp-delivery-recovery-v17.json` и
  `docs/fixtures/mcp/verify_v17.py`: 47/47 cases без skips (25 delivery,
  15 MCP recovery, 5 storage claim/release, 2 managed-action guards).
  Independent response projections/exact manual objects, exception types и spawn
  counts сверены поверх actual pinned source integration scenarios; исходные
  assertions дополнительно проверяют filesystem/history/lock effects.
  Real temporary Git/SQLite, fake HTTP/model/spawn, запрет socket connections,
  SQLite/Git confinement, bytecode/cache/plugins disabled, HEAD/clean-tree и
  fixture hash guards; runtime state не читается. Три failed-terminal scenarios
  используют явный healthy-server double, чтобы исключить live localhost probe.
- **Верификация:** 47 passed; 5 negative corpus harness cases отвергнуты
  (stale pin, duplicate, missing scenario, wrong spawn count, bool/int confusion).
  JSON deterministic, manifest/SQLite/diff checks прошли; historical v15 MCP
  corpus/verifier и Rust target v15 сохранены. Подробнее: `docs/mcp-fixtures.md`.
  Runtime/automation delta 0B.5 также завершён; полный automation workflow
  с обеими реальными моделями не заявлен.

### 0B.5. Runtime/automation fixtures и границы доказательства (завершено)

- **Цель:** bounded startup wait, strict plan/review schema, inherited
  baseline, durable intents/control, review fingerprint и итоговая доставка.
- **Source:** `tests/test_runtime.py`, `tests/test_worktree_runtime.py`,
  `tests/test_automation.py`, `tests/test_codex_client.py` (v17).
- **Приёмка:** runtime corpus отделяет startup serialization от concurrent
  execution; automation fixtures используют model doubles и crash injection.
  Source `docs/automatic-mode-plan.md:67–73` сообщает отдельный live Codex
  prepare и OpenCode smoke (README ссылается на документ),
  но полного real-model workflow на пользовательском проекте не доказывает.
- **Проверки:** runtime/automation corpus; без запуска реальных моделей или
  обращения к Python runtime state. Зависит от 0B.1.
- **Результат:** `docs/fixtures/runtime-automation-v17.json` и
  `docs/fixtures/runtime/verify_v17.py`: **77/77 без skips** (30 plan, 16 answer,
  1 runtime bounds, 1 fake-child launch lease, 1 durable blocked run, 28 source
  integration scenarios). Independent expectations проверяют plan/review gates,
  persisted run/task/round/delivery inventories, engine/child/start counts.
  Source asserts проверяют inherited baseline/scope, durable intents/crash
  resume, review fingerprint, controls и final delivery. Runtime startup
  serialization отделена от concurrent lifetime stand-in children.
  Adapter использует temporary fake executable, timeout/cancel/exit checks;
  настоящие модели/OpenCode/coordinator child не запускаются.
- **Верификация:** 6 negative harness cases отвергнуты (pin/id/default/bound/
  invalid accept/manual ready). SQLite/Git confinement, network connect guard,
  tracked-child cleanup, fixture hashes/HEAD/clean-tree, bytecode/cache/plugins
  disabled; manifest/historical SQLite/diff checks прошли. Restricted sandbox
  сначала запретил socket port probes для 3 worktree scenarios; разрешённый
  запуск вне sandbox прошёл все cases без подмены allocation/skip.
  Подробнее: `docs/runtime-fixtures.md`. Rust-код/target v15 не меняются.
  **0B завершён; 7.13–7.16 и согласованный профильный блок завершены на уровне services; следующий крупный блок — 7.17.**

## Поток 1. Rust foundation

### 1.1. Cargo workspace skeleton

Workspace, минимальные crates, toolchain и CI format/clippy/test.

### 1.2. Общие error types

Типизированные ошибки и безопасное пользовательское представление.

### 1.3. Идентификаторы и статусы

`ProjectId`, `TaskId`, `TaskStatus`, parsing и serialization.

### 1.4. Переходы Task

Table-driven реализация допустимых и запрещённых transitions без SQLite.

### 1.5. Round и Verification

Data model и serde compatibility без runtime logic.

### 1.6. Dependency/waiting domain transitions (v11, завершено)

- **Цель:** доменные `waiting_dependencies` статус и переходы без SQLite.
- **Source evidence:** `storage.py:103,118,133,2807-2878`
  (`activate_waiting_dependencies` всегда `UPDATE tasks SET
  status='implementing'`); `tests/test_storage.py:1145`.
- **Содержание:** `waiting_dependencies` в `TaskStatus`; explicit-активация
  `waiting_dependencies -> implementing` (event `dependencies_satisfied`);
  запрет неявной активации. `waiting_dependencies -> revising/failed` в
  источнике отсутствуют; `waiting -> closed` идёт только generic close-caller'ом
  (`close_task_impl` -> `update_task_status('closed')`,
  `mcp_server.py:2927-3041`, `storage.py:2756`), подтверждён
  `tests/test_mcp.py:1720`.
- **Критерии приёмки:** transitions table-driven (pure domain); dependency gate,
  linked DB graph и идемпотентность старого submit hash — приёмка
  storage/MCP-задач (3.12c и MCP), не домена.
- **Targeted checks:** domain transition tests.
- **Зависит от:** 1.4. **Открывает:** 3.12c, 7.17a, 8.14.
- **Результат:** `TaskStatus::WaitingDependencies` с parsing/serde и active
  classification; generic transition table разрешает только `waiting -> closed`.
  Отдельная `TASK_EVENT_TRANSITIONS` и `transition_on(DependenciesSatisfied)`
  разрешают `waiting -> implementing`; обычный `require_transition` отвергает
  активацию. Event spelling — `dependencies_satisfied`. Повтор события из
  `implementing` запрещён. Dependency graph, atomic activation, writer
  reservations и idempotency остаются в 3.12c/MCP; runtime/schema остаются v6.
  Табличные тесты покрывают все status pairs и событие из каждого статуса,
  strict parsing/serde, close и безопасные ошибки. Storage query regression
  ожидает семь active statuses; production storage/schema не менялись.
- **Проверено:** 56 domain, 193 storage и 143 worker tests; targeted clippy
  (`bridge-domain`, `bridge-storage`, `bridge-worker`, all targets), format и
  `git diff --check`. Worker tests запускались вне песочницы для loopback HTTP.

### 1.7. Typed contracts новых persisted полей (v7–v13, завершено)

- **Цель:** serde-модели новых persisted полей без runtime logic.
- **Source evidence:** `storage.py:705-1055,1705-1805`; `git_snapshot.py:29-49`;
  `usage.py:156-244`; `profiles.py:37-487`; `mcp_server.py:172-334`;
  `worker.py:1400-1530,1549-1654`. HEAD проверен: `86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a`.
- **Содержание:** `StructuredFinding` (severity/path/line/code/message),
  `Budget` (limits input/output/reasoning/cache_read/cache_write/cost +
  `warning_threshold`), `RoundCheckpoint` (fingerprints + diff-stat),
  `ProfileSnapshot` (`profile_json`/`hash`/`source`), `execution_mode`, workflow
  metadata (`workflow_id`/`depends_on`).
- **Критерии приёмки:** round-trip serde; canonical hash; corrupt fail closed.
- **Targeted checks:** domain model serde tests.
- **Зависит от:** 1.3, 1.5. **Открывает:** 3.12a, 7.13–7.18.
- **Результат:** модуль `persisted` в `bridge-domain` (типы re-exported из crate root)
  содержит `StructuredFinding`/bounded `StructuredFindings`, `Budget` с typed
  usage fields и positive finite JSON numbers, `RoundCheckpoint` со всеми
  repository/state/diff-stat моделями, `ProfileSnapshot`, `ExecutionMode` и
  `WorkflowMetadata`/`DependencyReference`. Deserialize проверяет весь payload,
  а не сохраняет уцелевший поднабор; invalid data даёт безопасную ошибку.
  Checkpoint refs нормализуются в lowercase; state/topology/mirrored workspace
  fingerprints и bounds проверяются. Runtime/schema остаются foundation v6.
- **Контракт для потребителей:** поле `ProfileSnapshot.source` — `builtin|config`,
  а persisted `tasks.profile_source` — origin (`argument|project_default|builtin_default`).
  `validate_identity` сравнивает row id/hash/origin с snapshot. Snapshot SHA-256
  использует sorted compact UTF-8 JSON; definition SHA-256 — sorted UTF-8 JSON
  с Python-default пробелами после separators и собственную модель definition,
  до наследования project model. `sha2` добавлена как dependency; `serde_json`
  теперь production dependency домена. Golden cases вычислены Python-кодом
  зафиксированного HEAD, без чтения runtime state.
- **Границы:** публично собранные/изменённые модели требуют `validate` перед
  использованием. Findings проверяют shape и lexical запреты; scope/symlink/
  trusted-root authorization и secret scanning остаются у потребителей.
  Dependency task IDs — safe tokens до 128 символов, не UUID `TaskId`; strict
  duplicate-edge rejection не заменяет linked-project/cycle checks. Budget
  сохраняет integer/float JSON encoding, отсутствие budget/profile/checkpoint
  представляет внешний `Option<T>`, не повреждённая модель по умолчанию.
  Checkpoint игнорирует неизвестные поля как reference reader, но неверный тип
  известного поля отвергается даже у unavailable repository; пустой repository
  list отвергается. Builtins/config resolution, migrations и runtime consumers
  не входят в задачу.
- **Проверено:** 74 domain tests (18 новых), включая round-trip, Python golden
  hashes, Unicode, inherited model, tampering, optional/legacy shapes, bounds,
  missing/unknown/malformed fields, duplicate dependencies и redaction;
  workspace/all-targets clippy, format и `git diff --check`.

### 1.8. DeliveryMode (v16, завершено)

- **Цель:** typed `manual|on_accept`, historical default manual, раздельный
  config parse и persisted normalization.
- **Source:** `config._parse_delivery_mode`, `storage._normalize_delivery_mode` (v17).
- **Приёмка:** config unknown/non-string/whitespace fail closed; persisted
  absent/corrupt value деградирует в manual, как Python, и никогда не включает
  автоматическую доставку. Это отличается от strict budget parsing.
- **Проверки:** table-driven enum/normalization tests. Зависит от 0B.1.
- **Результат:** `bridge-domain::DeliveryMode` с exact serde/TryFrom,
  default Manual и отдельным defensive `normalize_persisted`: только точное
  `on_accept` сохраняет opt-in, отсутствующие/неизвестные/нестроковые значения
  дают Manual. Domain table-driven tests; storage/config wiring — 2.14/3.13b.


## Поток 2. Конфигурация

### 2.1. Базовый TOML loader

### 2.2. Project ID и workspace validation

### 2.3. Endpoint и port validation

### 2.4. Уникальность workspace/endpoints/token files

### 2.5. Max rounds, model и optional paths

### 2.6. Auto-approve permissions

### 2.7. Trusted external directories

### 2.8. Credentials reader

### 2.9. Project env reader

### 2.10. execution_mode validation (v10, завершено)

- **Цель:** config validation `execution_mode=direct|worktree`.
- **Source evidence:** `config.py:249-280,443-466,626`;
  `tests/test_config.py:286-390`. Source HEAD проверен: `86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a`.
- **Содержание:** absent→`direct`; `worktree`; surrounding whitespace/invalid
  fail closed; `allow_parallel_writers=true` только при `worktree`.
- **Критерии приёмки:** default `direct` не меняется; fail-closed типизирован.
- **Targeted checks:** config execution_mode tests.
- **Зависит от:** 2.1, 0A.2. **Открывает:** 7.16a, 8.17.
- **Результат:** `ProjectEntry::execution_mode() -> bridge_domain::ExecutionMode`;
  absent → `Direct`, exact `direct|worktree`, no trim/coercion. Loader также
  проверяет тип `allow_parallel_writers` (только TOML boolean) и worktree-only
  gate для `true`. Errors — `DomainError`/`InvalidInput`, статические безопасные
  сообщения с разделением type/whitespace/value/gate. Validation действует
  отдельно для каждого проекта; ошибка любого проекта отвергает весь config.
  Сырая TOML-таблица сохраняется без вставки defaults и нормализации.
- **Границы:** typed admission settings (`max_active_tasks`,
  `allow_parallel_writers` getter/defaults) остаются задачей 2.11; mode gate
  уже проверен в 2.10. Worktree execution/storage/runtime wiring не входят;
  конфигурация `worktree` сама по себе не запускает worktree executor.
- **Проверено:** 159 config tests (8 новых), включая все 16 targeted v15
  corpus cases без skips, mode/type/whitespace/case/boolean matrix,
  per-project isolation, raw-value preservation и redaction; workspace
  all-targets clippy, format, `git diff --check`. `serde_json` добавлен только
  как config dev-dependency для чтения frozen corpus; fixtures не менялись.

### 2.11. max_active_tasks / allow_parallel_writers defaults (v14 B1 / v15 B2, завершено)

- **Цель:** B1/B2 config defaults и gate.
- **Source evidence:** `config.py:51-56,416-466`;
  `tests/test_config.py:338-392`. HEAD проверен: `86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a`.
- **Содержание:** `max_active_tasks=1` default, positive int (bool/0/negative
  rejected); `allow_parallel_writers=false` default, true только worktree.
- **Критерии приёмки:** historical defaults не ослаблены; worktree-only gate.
- **Targeted checks:** config admission-defaults tests.
- **Зависит от:** 2.10, 0A.2. **Открывает:** 3.12d, 7.17a, 7.17b.
- **Результат:** `ProjectEntry::max_active_tasks() -> u64` с default `1` и
  `ProjectEntry::allow_parallel_writers() -> bool` с default `false`.
  Лимит принимается только как положительный TOML integer (без bool/string/float
  coercion); хранится без сужения в `u64`, проверена граница `i64::MAX` TOML.
  Общий с 2.10 parser возвращает проверенный boolean и сохраняет worktree-only
  gate для `true`. Настройки независимы: большой task bound не включает parallel
  writers, а parallel opt-in разрешён в worktree и при task bound `1`.
  Defaults не вставляются в raw TOML; getters одинаково работают после clone.
  Invalid values дают безопасный `DomainError`/`InvalidInput`, без input/path/id.
- **Границы:** config только хранит admission settings; counting/reservations,
  writer ledger, dependency activation, SQLite/schema и runtime concurrency
  остаются у storage/worker/MCP consumers. Ни один default не ослаблен.
- **Проверено:** 162 config tests (3 новых, расширены mode/gate, per-project и
  redaction regressions), все 23 targeted v15 config cases без skips; native
  TOML integer forms/boundary, nonpositive/wrong-type values в обоих режимах,
  independent-settings matrix, raw-value preservation и typed per-project
  defaults; workspace all-targets clippy, format и `git diff --check`.

### 2.12. Profile definitions/default/effective snapshot model (v13, завершено)

- **Цель:** config profile definitions и resolution.
- **Source evidence:** `config.py:45-61,344-413`; `profiles.py:37-416`;
  `tests/test_config.py`; `tests/test_profiles.py`.
- **Содержание:** builtins + custom `[projects.<id>.profiles.<id>]`,
  `default_profile` validation; merged `profile_definitions`; effective model
  (`snapshot_model_source`).
- **Критерии приёмки:** unknown `default_profile` fail closed; historical
  implementer backward compatible.
- **Targeted checks:** config/profile resolution tests.
- **Зависит от:** 2.1, 0A.2. **Открывает:** 7.18, 8.18.
- **Результат:** immutable definitions объединяют четыре точных Python builtins
  и пользовательские profiles; строгие ID, purpose/instructions/model checks,
  запрет дополнительных полей и неизвестного default. Приоритет выбора:
  непустой argument → project default → builtin implementer. Source definition
  отделён от selection origin; effective model берётся из profile либо project,
  а snapshot фиксирует значения и canonical definition/snapshot hashes через
  domain 1.7. Переопределённый implementer не считается historical builtin.
  Defaults не вставляются в raw TOML; ошибки и Debug не раскрывают inputs.
- **Security:** private instruction gate проверяет девять категорий секретов
  по reference patterns и frozen fixtures; добавлены production `regex` и
  `base64`, `serde_json` перенесён в production dependencies.
- **Границы:** этот foundation содержит config definitions/resolution/snapshots.
  Persistence/submit selection и prompt wiring закрыты 8.18/7.18;
  UI wiring остаётся у 12.13. Python runtime
  state не читался и не изменялся; reference HEAD подтверждён по manifest.
- **Проверено:** 173 config tests, все 33 profile corpus cases без skips,
  четыре независимых Python canonical snapshot/hash goldens, effective-model
  и origin/source matrix, bounds/Unicode/controls, frozen secret categories,
  redaction и per-project isolation; все 949 workspace tests, workspace
  all-targets clippy, format и `git diff --check`.

Каждая задача переносит только указанную группу правил и её fixtures.

**Готовность потока:** весь config corpus совпадает с Python семантически;
новые persisted-настройки (execution_mode, admission defaults, profiles) имеют
собственные узкие задачи до потребителей.

### 2.13. State-directory approval opt-in (завершено)

- **Цель:** boolean `auto_approve_state_directory`, default false.
- **Source:** `config._parse_auto_approve_state_directory`,
  `state_directory_permission_pattern` (v17).
- **Приёмка:** real bool only; opt-in требует narrowly scoped state root,
  запрещает `/` и wildcard characters `*|?`; trusted external Git roots не
  меняются. Здесь только config validation; worker/controller — 7.8a/b.
- **Проверки:** config fixtures. Зависит от 0B.2. Открывает 7.8a/b.
- **Результат:** real-bool `ProjectEntry::auto_approve_state_directory`, false
  default; `load_config_with_state_root` проверяет явный Rust namespace без
  чтения/создания state. Opt-in через обычный `load_config` без root запрещён:
  Rust не угадывает Python/default state directory. Pattern builder отвергает
  relative/root/wildcard/non-UTF8 roots; lexical parent normalization не даёт
  замаскировать `/`. Trusted external Git roots не расширяются.
  35 config delta cases проходят (включая delivery), targeted root checks,
  полный config suite (177) и clippy; permission consumers — 7.8a/b.


### 2.14. delivery_mode config (v16, завершено)

- **Цель:** manual default; on_accept разрешён только с execution_mode=worktree.
- **Source:** `config._parse_delivery_mode`, `load_all_projects` (v17).
- **Приёмка:** strict type/enum/whitespace и cross-field checks; config не
  меняет policy уже созданной задачи.
- **Проверки:** delivery-mode config fixtures. Зависит от 1.8, 0B.2.
- **Результат:** validated `ProjectEntry::delivery_mode`, default Manual,
  strict string/enum/whitespace parsing и worktree-only OnAccept gate.
  19 delivery cases из frozen v17 delta corpus; полный config suite (175)
  и all-targets clippy. Task policy persistence остаётся в 3.13b.


## Поток 3. SQLite storage

### 3.1. Read-only schema inspection

### 3.2. WAL, foreign keys и busy timeout

### 3.3. Task row mapping

### 3.4. Round row mapping

### 3.5. Initialization совместимой пустой БД

### 3.6. Get/list/pagination queries

### 3.7. Atomic task creation

### 3.8. Request idempotency

### 3.9. Atomic round/verifier updates

- **3.9a. Atomic round/task lifecycle updates — завершено.** Типизированные
  lifecycle-API на `StorageConnection` (`create_revision_round`,
  `bind_round_session`, `prepare_round`, `mark_round_sent`,
  `mark_round_observing`, `mark_worker_started`, `finish_round`): каждая
  операция в одной `BEGIN IMMEDIATE` транзакции, проверка round-переходов по
  `ROUND_TRANSITIONS` и task-переходов через `TaskStatus::require_transition`.
- **3.9b. Verifier persist-once — завершено.** `begin_verifier` и
  `complete_verifier` атомарно ведут verifier lifecycle/result: `running` с
  идемпотентным обновлением и без отката `done`, однократная фиксация `done` +
  `verifier_json`, replay идентичного завершённого результата без записи
  (Python-семантика reuse) и fail-closed `VerifierResultConflict` для
  отличающегося результата. Проверяются task/round, project membership,
  current round и пара `verifier_state`/`verifier_json`; events не пишутся.
- **3.9c. reopen_failed_round — завершено.** Типизированный
  `StorageConnection::reopen_failed_round` и `ReopenFailedRoundOutcome`
  (`Reopened`/`NotEligible`). В одной `BEGIN IMMEDIATE` транзакции читаются task
  и его current round, обе строки маппятся через production-контракт, и
  восстановление выполняется только при `task.status=failed`, отсутствии
  `close_requested_at`, `round.status=failed` с
  `error_code=RECOVERABLE_FAILED_ERROR_CODE` (`assistant_error`) и
  согласованности round с task. Round атомарно переводится `failed->observing`,
  `error_code` очищается, task возвращается в `implementing` (implement) или
  `revising` (revise), и пишется ровно одно событие `reopened`
  (`failed assistant_error reopened for recovery`) с общим timestamp.
  Неизвестная/неподходящая задача и повторный вызов дают `NotEligible` без
  записей (Python no-op); другие error_code, pending cooperative close,
  не-current/несогласованный round и повреждённые строки не восстанавливаются,
  а некорректный persisted state отвергается fail closed без частичных записей.
- **3.9d. Cooperative close — завершено.** Типизированные
  `StorageConnection::request_task_close` (`RequestTaskCloseOutcome`:
  `Requested`/`AlreadyRequested`/`UnknownTask`/`Terminal`) и
  `StorageConnection::complete_requested_close`
  (`CompleteRequestedCloseOutcome`: `Closed`/`NoCloseRequest`/`Terminal`/
  `UnknownTask`), а `finish_round` учитывает pending close. В одной
  `BEGIN IMMEDIATE` транзакции `request_task_close` по Python-семантике даёт
  `UnknownTask` для отсутствующей задачи, `Terminal(status)` для
  `accepted`/`closed`, при первом запросе сохраняет `close_requested_at`,
  `close_reason` (до 300 Unicode-символов, как Python `reason[:300]`),
  `updated_at` и ровно одно событие `close_requested` (`round_number=NULL`,
  `task close requested`) с единым timestamp, а повтор возвращает
  `AlreadyRequested` без изменения timestamp/reason и без нового события.
  `complete_requested_close` переводит nonterminal-задачу с запросом в `closed`
  и пишет ровно одно событие `closed` (`round_number=NULL`,
  `task closed: <reason>`; пустой/отсутствующий reason использует точный
  fallback `requested while worker was running`), а unknown/terminal/без-запроса
  и повтор — типизированный no-op без записей. `finish_round` всегда пишет
  запрошенные round fields/status, но при pending close применяет фактический
  переход task в `closed` (валидируется через `TaskStatus::require_transition`)
  и пишет событие `closed` вместо round-finished, поэтому caller-supplied
  `task_status` не обходит pending close. Paired-записи и события атомарны и
  используют один timestamp, event failure откатывает всю транзакцию, а ошибки
  не раскрывают ids, reason, response, SQL, JSON и пути. Pending close
  по-прежнему запрещает `reopen_failed_round`. Schema/version/fixtures не
  менялись.
- **Этап 3.9 завершён.**

### 3.10. Storage isolation и создание нового Rust state

- **Завершено.** `bridge-storage` предоставляет типизированный storage-level
  контракт `RustStateLayout` для одного Rust-owned project state. Layout
  строится только из явно переданного Rust state root и `ProjectId` и выводит
  все runtime-пути внутри этого root: `<root>/<project_id>/state.sqlite`,
  project-scoped locks `mcp.lock`/`worker.lock` и root-scoped `runtime.lock`,
  PID/ownership records `<kind>.process.json`, логи `mcp.server.log`/
  `opencode.server.log`/`worker.log`, token-файлы и endpoint-файлы. `project_id`
  и artifact-имена валидируются как единый безопасный path component, поэтому
  пути не могут выйти за root; типизированный `StateLayoutError` не раскрывает
  id, имена и пути. `RustStateLayout::initialize` создаёт только собственную
  новую пустую БД schema v6 по derived Rust-пути и идемпотентно сохраняет
  существующие Rust-rows; API, читающего, копирующего или импортирующего Python
  SQLite/history, нет. Тесты на независимых временных Rust/Python roots
  доказывают, что все Rust runtime paths лежат внутри Rust root и не
  пересекаются с Python root, новая БД пуста (schema v6, без Python
  task/history rows), инициализация и Rust-записи не меняют байты Python state,
  а повторная инициализация сохраняет Rust-rows. Schema v6, version и fixtures
  не менялись.
- **Этап 3.10 завершён.** Это исторический **foundation v6**: современный
  target (3.12) поднимает fresh Rust-owned БД до schema v15, сохраняя ту же
  изоляцию/ownership, поэтому завершённое описание выше не переписывается.

### 3.11. Ownership/format marker и fail-closed guard

- **Завершено.** `bridge-storage` расширяет `RustStateLayout` ownership/format
  marker-ами поверх завершённого Rust state. Rust-owned state получает
  versioned sidecar marker `<project_dir>/.agent-bridge-state.json`
  (`implementation="rust"`, `format_version=1`, `project_id`, `state_root`) и
  additive-строку `meta.runtime_owner='rust'`; `PRAGMA user_version=6`,
  DDL/tables/columns/indexes/foreign keys, generic `inspect`/`initialize`
  compatibility и contract fixtures не меняются. Обязательный `state_root` —
  это namespace-ключ root: путь лексически нормализуется (`.` и повторные
  разделители убираются; `..` гасит только предшествующий обычный компонент, а
  несведённый ведущий `..` сохраняется, поэтому `../a` и `../../a` не
  сталкиваются с `a`; без обращения к FS и без следования symlink) и
  hex-кодируется из стабильных Unix OS-байтов (`OsStrExt::as_bytes`, явный
  контракт поддерживаемой платформы Unix/Linux; не unspecified
  `OsStr::as_encoded_bytes`), так что ключ стабилен как on-disk interchange,
  lossless для non-UTF-8 и не сталкивается у разных root. Оба маркера
  выставляются только
  `RustStateLayout::initialize` для собственного нового пустого Rust state;
  Python state/history не читается, не импортируется и не модифицируется.
  Новый production API `RustStateLayout::open` до выдачи writable
  `StorageConnection` обязан сверить sidecar marker, поддерживаемый
  `format_version`, implementation, project namespace, нормализованный
  `state_root`, `meta.runtime_owner` и существующий schema v6 contract.
  Отсутствующий, malformed (включая marker без `state_root`), unsupported,
  foreign или противоречивый marker, чужой `runtime_owner`, mismatch namespace
  или `state_root` и несовместимая schema завершаются fail closed без записи;
  при каждом отказе байты/содержимое чужого state и marker не меняются, а
  скопированный под другой Rust root state отклоняется. Порядок создания
  crash-safe: marker пишется первым в приватный уникальный временный файл
  (`create_new`, `sync_all`), публикуется no-clobber через hard link без
  перезаписи существующего маркера с синхронизацией каталога, затем создаётся
  schema v6 с `meta.runtime_owner` в одной `BEGIN IMMEDIATE` транзакции;
  прерванная инициализация (marker без DB) восстанавливается повторным
  `initialize`, а DB без marker или без `meta.runtime_owner` никогда не
  принимается как валидная и не перезаписывает чужой marker/state. Параллельные
  инициализаторы не делят temp-файл и сходятся к одному валидному state.
  Типизированный `RustStateError` (`MissingMarker`/`MalformedMarker`/
  `ForeignImplementation`/`UnsupportedFormatVersion`/`NamespaceMismatch`/
  `MissingDatabase`/`UnmarkedState`/`MissingRuntimeOwner`/`ForeignRuntimeOwner`/
  `IncompatibleSchema`/`UnsupportedSchemaVersion`/`Connect`/`MarkerIo`/
  `Database`) не раскрывает project id, namespace, marker contents, SQL, пути и
  secrets. Тесты покрывают fresh init и reopen, идемпотентный повтор,
  компонентно-осознанную нормализацию `state_root` (`.`/`a/../b`/`../a`/
  `../../a`, отсутствие коллизий относительных roots) со стабильным
  hex-контрактом, missing/malformed marker (включая отсутствие `state_root`),
  foreign
  implementation, unsupported `format_version`, namespace mismatch, чужой
  `state_root` и state, скопированный под другой root, missing/foreign
  `meta.runtime_owner`, sidecar/SQLite disagreement, несовместимую schema,
  параллельную инициализацию, неизменность чужого state/marker при каждом
  отказе, восстановление после marker-без-DB и неизменность Python
   state/history. **Историческая пометка:** marker/guard реализованы для
   schema v6 (foundation v6); будущий fresh v15 target (3.12) обязан сохранить
   тот же ownership/format guard и расширить проверку на поддерживаемый
   schema v15, не ослабляя изоляцию.

### 3.12. Schema v15 storage target (после 0A, завершено)

**3.12a–3.12f завершены.** Зависит от refresh 0A.4 и
typed-контрактов 1.7 (для 3.12c — также 1.6).
Каждая подзадача меняет один storage-контракт; schema, public API и security
policy не объединяются. Rust по-прежнему ведёт только собственную пустую БД
(fresh v15) и Rust-owned additive upgrade/legacy fixtures (v6/v11/v14);
Python state не читается и не мигрируется.

- **3.12a. Additive columns/table DDL (завершено).** `rounds.structured_findings` (v7),
  `tasks.budget_json` (v8), `tasks.workflow_id`/`depends_on` (v9),
  `tasks.execution_mode` + `worktrees`/`worktree_quarantine` (v10),
  `rounds.checkpoint_json` (v12), `tasks.profile*` (v13), `active_writers`
  (v14), `active_writers.parallel` (v15). Source: `storage.py:1705-1805,
  1908-2091`. Acceptance: fresh v15 DDL и additive v6→v15 на Rust-owned
  legacy fixture; `meta.schema_version='15'`. Depends on 1.7. Check: storage
  tests + `verify.py`.
  **Результат:** `RustStateLayout::initialize` создаёт fresh v15 либо обновляет
  строго Rust-owned v6 после read-only sidecar/schema/runtime-owner guard и
  повторной проверки внутри `BEGIN IMMEDIATE`. DDL и оба version markers
  атомарны; все legacy task/round/event columns сохраняются без backfill.
  Новые optional поля NULL, execution mode `direct`, writer parallel default `0`.
  `inspect` и ownership guard принимают frozen v6/v15; v15 проверяет также
  defaults и single-writer index predicate. Fresh и upgraded DDL совпадают с
  independent `expected-v15.json` по tables/columns/defaults/indexes/FKs;
  физический порядок добавленных columns не является контрактом.
  Final v15 indexes создаются как часть структурного target (старый
  `ux_tasks_active` удалён); historical v11/v14 upgrades и writer semantic
  matrix остаются в 3.12b. До 3.12d `create_task` сохраняет прежний single-task
  bound через проверку внутри writer transaction, после request replay.
  **Границы:** generic `initialize` сохраняет исторический v6 fixture contract;
  runtime Rust state использует guarded layout initializer. Python state не
  читается и не мигрируется. В 3.12a ledger оставался пустым; 3.12d добавляет
  backfill/reconcile/reservations. Typed row mapping новых полей и runtime
  consumers — следующие задачи.
  **Проверено:** 200 storage tests (7 новых), четыре Rust-owned legacy fixture
  upgrades с сохранением всех прежних columns, defaults/NULL semantics,
  idempotency, fresh/concurrent upgrade, transaction rollback и retry,
  single-task admission/replay regression, wrong defaults/predicate rejection;
  SQLite `verify.py` проверил 5 fixtures read-only; workspace clippy, format,
  `git diff --check` и все 956 workspace tests.
- **3.12b. Writer-status indexes: historical v11/v14 vs final v15 (завершено).**
  Промежуточные `ux_tasks_active` (`tasks(project_id)` WHERE writer statuses;
  `storage.py:140-145`) и `ux_active_writers_project`
  (`active_writers(project_id)`; `storage.py:2037`) создаются только миграциями
  до v14 и **DROP**-аются на v15 (`storage.py:2049-2086`). Fresh v15 их НЕ
  содержит: остаётся partial unique `ux_active_writers_single`
  (`active_writers(project_id) WHERE parallel=0`) плюс nonunique lookup
  `ix_tasks_project_status`/`ix_active_writers_project`. Single-writer bound
  обеспечивают partial unique + atomic admission ledger/real-status, а не
  writer-status UNIQUE `tasks(project_id)` (он запрещал бы B2 parallel writers).
  Source: `storage.py:135-155,1977-1990,2024-2086`. Acceptance: fresh v15
  допускает несколько disjoint parallel writers; legacy v6/v11/v14 rows
  сохраняют смысл; `ux_tasks_active` отсутствует в v15. Check: storage index
  tests + `verify.py`.
- **Результат 3.12b:** строгие отдельные structural contracts для v11 и v14;
  v11 `ux_tasks_active` включает waiting status, v14 исключает его и имеет
  `ux_active_writers_project`. Guarded Rust initializer обновляет v11/v14 до
  v15 в одной транзакции, удаляет исторические UNIQUE indexes, добавляет
  отсутствующие поля и final v15 lookup/partial indexes. Все прежние columns
  и reservations сохраняются; v14 reservations получают `parallel=0`.
  Guard проверяет весь predicate, defaults и согласованную version pair;
  read-only ownership validation читает одну транзакционную schema snapshot,
  чтобы не смешивать версии при concurrent upgrade. Foreign/unmarked state
  не принимается, Python state не читается и не мигрируется.
  **Проверено:** 207 storage tests (7 новых), independent intermediate fixtures
  из копии frozen v15 (без production migration DDL), v11/v14 status matrix,
  fresh/v6/v11/v14→v15 singleton/parallel index matrix, project/task identity,
  сохранение всех legacy columns/reservations, idempotency, wrong predicates,
  ownership/version mismatch, rollback/retry и concurrent upgrade;
  SQLite `verify.py` (5 fixtures read-only), все 963 workspace tests,
  workspace all-targets clippy, format и `git diff --check`.
  **Границы:** индекс допускает несколько parallel reservations, но проверку
  disjoint scopes реализован в 3.12d. Reconcile/admission не входили в 3.12b;
  runtime parallelism остаётся у consumers, defaults не ослаблены.
- **3.12c. Waiting-dependency activation (завершено).** `waiting_dependencies` статус,
  `activate_waiting_dependencies` (atomic single winner, conditional UPDATE
  rowcount==1), `refresh_task_baseline`, `dependencies_satisfied` event.
  Source: `storage.py:103,2807-2878`. Acceptance: explicit activation only, no
  startup auto-activation; idempotent. Depends on 1.6. Check: test_storage
  `test_activate_waiting_dependencies_is_atomic_and_idempotent:1145`.
  **Результат:** `activate_waiting_dependencies(task_id, project_id)` — explicit
  API для schema v15: caller устанавливает accepted dependency readiness;
  scoped waiting-row lookup, domain `DependenciesSatisfied` transition,
  conditional UPDATE (`rowcount==1`) и точное round-1 event
  `dependencies_satisfied` / `accepted dependencies unlocked the task` находятся
  в одном `BEGIN IMMEDIATE`. Winner возвращает true, repeat/missing/foreign/
  nonwaiting/busy — false без writes. Task timestamp совпадает с event timestamp.
  `refresh_task_baseline` принимает object snapshot и optional base head,
  меняет только baseline/timestamp через project/status-fenced UPDATE;
  activated/closed task не получает stale baseline. Без event/round writes.
  Typed `DependencyUpdateError` не раскрывает inputs/SQL/trigger text в
  Display/Debug; underlying error доступен только через `source`.
  **Границы:** открытие/инициализация не активируют задачи; workers/prompts,
  создание/attempt round, accepted-dependency resolution и combined refresh/
  activation orchestration не входят. До 3.12d сохраняется single-writer guard
  по actual writer statuses + ledger чужих задач; own stale reservation
  исключается и сохраняется. Scope overlap/normalization, reservation insert/
  release/reconcile добавлены в 3.12d; modern Python runtime parity ещё не
  заявлен. Python runtime state не читался и не изменялся.
  **Проверено:** 216 storage tests (9 новых): explicit-only startup/reopen,
  exact event/timestamp, immutable rounds/attempted, refresh pin/fence,
  missing/foreign/all-status no-op matrix, writer/ledger blocking и project
  isolation, 8 concurrent callers с одним winner, competing waiting tasks,
  refresh-vs-activation race, event/baseline failure rollback/retry и redaction,
  invalid snapshots/corrupt rows fail closed; workspace all-targets clippy,
  SQLite verifier (5 fixtures read-only), format и `git diff --check`.
- **3.12d. active_writers reservation + admission (завершено).** Reservation insert/release
  внутри `create_task`/`update_task_status`, `get_active_writers`,
  `writer_activity_present` (ledger + real writer statuses),
  `_check_writer_scope_admission_in_conn`, `_reconcile_active_writers_in_conn`,
  canonical symlink-resolved scope overlap (`_scope_canonical_identity`,
  `_canonical_scopes_overlap`), `ScopeOverlap`/`ScopeDataError` fail closed.
  Source: `storage.py:307-419,2182-2291,2431-2487,2516-2641,2775-2776`.
  Acceptance: `max_active_tasks=1` default; `allow_parallel_writers=false`
  default; true только для worktree; corrupt scope fail closed. Depends on
  1.6, 2.11, 3.12a.
  **Результат:** private-field `AdmissionSettings` validates positive task bound
  и worktree-only parallel opt-in; defaults `1/false/direct`. Новый
  `create_task_with_admission` сохраняет request replay перед admission,
  считает все unfinished statuses, допускает initial writer/waiting status,
  сохраняет execution mode и атомарно пишет ready-writer reservation вместе с
  task/initial round/event. Waiting submit не резервирует слот; parallel waiting
  submit проверяет overlap. Default `create_task` сохраняет historical v6 API
  и на v15 использует новый ledger; opt-in settings предназначены для v15.
  `activate_waiting_dependencies_with_admission` проверяет worktree mode,
  canonical scope admission и резервирует slot в status/event transaction;
  повторный/blocked вызов не пишет. Совпадающий own crash-window reservation
  переиспользуется, чужая identity/scopes/flag fail closed.
  `update_task_status`, `finish_round` (включая pending-close override) и
  `complete_requested_close` освобождают slot только при terminal status,
  атомарно с остальными writes. Generic waiting→writer transition запрещён.
  **Scope contract:** normalized relative/absolute scopes, trailing `/` = dir;
  malformed JSON/non-list/non-string/empty/ambiguous paths fail closed.
  Component-aware overlap использует symlink-resolved longest existing prefix
  и missing suffix; siblings не конфликтуют, directory descendants и aliases
  конфликтуют. Broken/cyclic/unresolvable paths не сравниваются лексически.
  Parallel admission проверяет incoming canonical identities даже без peers.
  Scope authorization остаётся у path-policy consumer, workspace — trusted
  project config input; permission/scope allowlists не расширяются.
  **Reads/recovery:** strict typed `get_active_writers` (timestamps/task-id order),
  `writer_activity_present` по ledger + реальным writer statuses, независимо от
  config и потерянной reservation. Explicit `reconcile_active_writers` удаляет
  orphan/terminal rows и backfills writers, сохраняет существующие scopes/flags
  при config downgrade. Corrupt/conflicting repair откатывается целиком, а не
  скрывает constraint errors. Rust-owned v6/v11/v14 upgrade выполняет default
  single-writer backfill до commit; v15 init остаётся read-only/idempotent,
  configured startup recovery вызывает explicit reconcile API.
  **Границы:** storage only; config/CLI/MCP/worker wiring, dependency resolver,
  per-task locks и runtime parallel workers остаются у consumers. Python state
  не читался и не мигрировался; reference HEAD подтверждён по manifest.
  **Проверено:** 233 storage tests (17 новых, обновлены staged ledger assertions),
  all-status count/bounds и replay-at-capacity, B1 queues/B2 disjoint writers,
  filesystem alias/directory/missing-leaf/absolute-path matrix, broken/looped
  aliases и corrupt ledger/real scopes fail closed, lost-ledger/config downgrade,
  terminal release и transactional rollback/retry/redaction, idempotent repair,
  concurrent submit task bound и overlapping/disjoint submit/activation;
  все 989 workspace tests, all-targets clippy, SQLite verifier (5 fixtures
  read-only), format и `git diff --check`.
- **3.12e. worktrees/worktree_quarantine lifecycle storage (завершено).**
  `register_worktree`, `update_worktree_status`, `update_worktree_server`,
  `update_worktree_baseline`, `set_worktree_delivery`; quarantine registry
  (`register_worktree_quarantine`, `transition_worktree_quarantine`,
  `list_worktree_quarantine_readonly`, `has_worktree_quarantine_table`).
  Source: `storage.py:226-249,3319-3929`. Acceptance: fail-closed transition
  maps; read-only quarantine listing. Depends on 3.12a.
  Реализовано в `bridge-storage/worktrees.rs`: typed statuses и строгий
  mapping всех полей, project/task/worktree-mode fence, BEGIN IMMEDIATE для
  переходов и metadata updates. Status/delivery events записываются атомарно;
  повтор worktree/delivery status не меняет metadata, quarantine same-status
  может уточнять путь, как Python implementation. Quarantine expected status
  и original path сравниваются под write lock; drift не меняет реестр.
  Baseline/process/path metadata остаются opaque, server port — 1..65535.
  Read-only listing/table probe и strict worktree read используют mode=ro и
  один snapshot: missing DB не создаётся, supported schema без quarantine
  table даёт empty/false, incompatible/corrupt state возвращает safe error.
  Читается текущий WAL без WAL flip/migration; SQLite может затронуть свои
  transient WAL/SHM sidecars. Filesystem create/move/remove, Git worktrees,
  server startup и применение delivery остаются задачами consumers/runtime.
  Проверено: **246 storage tests**, exhaustive persisted transition matrices,
  atomic rollback/redaction, metadata/idempotence, readonly legacy/live-WAL,
  drift и concurrent quarantine transitions; **1002 workspace tests**, all-targets
  clippy, SQLite verifier (5 fixtures read-only), format и diff check.
- **3.12f. Budget persistence/parse (завершено).** `tasks.budget_json` persist + strict
  reader (`validate_budget`/`normalize_persisted_budget` live in
  `usage.py:156-244`). Source: `usage.py`; `mcp_server.py:2289-2291`.
  Acceptance: NULL = no budget; corrupt budget fail closed. Depends on 3.12a.
  `bridge-storage/budgets.rs`: immutable `TaskBudget`, public validation,
  guarded persisted normalization и snapshot read-only budget reader. Limits
  input/output/reasoning/cache_read/cache_write/cost — positive finite numbers,
  bool и unknown fields запрещены; warning_threshold ∈ (0,1], default 0.8.
  Public JSON null означает optional absence, но persisted JSON null — corrupt;
  SQL NULL и historical v6 без колонки означают no budget. Modern task reads
  включают budget_json и возвращают typed `InvalidBudget` при повреждении.
  `create_task_with_budget` сохраняет бюджет в одной транзакции с task/round/
  event/reservation; default APIs пишут SQL NULL. Replay сохраняет исходный
  бюджет, trusted consumer включает budget в payload_hash; legacy v6 не
  принимает новый непустой бюджет. Read-only API не создаёт state, не мигрирует
  и не меняет journal mode, различает missing task/DB и task без бюджета.
  Проверено: **254 storage tests**, validation/defaults/numeric boundaries,
  strict corrupt reads/replay/mutations, atomic rollback/redaction, readonly
  legacy/live-WAL и default creation; **1010 workspace tests**, all-targets clippy,
  SQLite verifier (5 fixtures read-only), format и diff check. Usage aggregation,
  warning/exhaustion state, override/gates и MCP wiring остаются в 7.14.

**Готовность потока (после 0A/3.12):** target — **fresh Rust-owned schema v15**
(пустая БД), additive upgrade — только Rust-owned legacy v6; Python- и
Rust-state изолированы: отдельная БД и отдельная история, общая рабочая БД и
перенос Python history/runtime state отсутствуют; writable Rust open
по-прежнему невозможен без полной согласованности sidecar + namespace +
`meta.runtime_owner` + поддерживаемого schema contract (v15 для нового state,
ownership/isolation из 3.10/3.11 не ослабляются). Формулировка «без изменения
schema version» относится только к историческому завершённому 3.10/3.11
(foundation v6) и не является финальным контрактом.

**Потоки 3, 4 и 5 завершены.** Шаги **4.7** (snapshot основного repository),
**4.8** (multi-repository snapshots) и **4.9** (snapshot comparison и
violations) завершены; шаг **5.1** (test command validation), шаг **5.2**
(один command runner), шаг **5.3** (последовательность команд), шаг **5.4**
(HEAD/workspace fingerprints), шаг **5.5** (side-effect detection) и шаг
**5.6** (persist-once semantics) завершены. Шаг **6.1** (HTTP transport и
basic auth), шаг **6.2** (Health и workspace identity), шаг **6.3** (OpenAPI
compatibility), шаг **6.4** (Session create/list/get), шаг **6.5** (Message
list и parsing), шаг **6.6** (Async prompt delivery), шаг **6.7**
  (Permissions list/reply) и шаг **6.8** (Questions и blockers) завершены.
  Шаг **7.1** (Worker argv и spawn), шаг **7.2** (Lock и startup grace), шаг
  **7.3** (Session resolution), шаг **7.4** (Initial prompt happy path), шаг
  **7.5** (Revision round) и шаг **7.6** (Permission blocker) завершены.
  Завершённые этапы 0–6 и 7.1–7.6 — foundation старого контракта schema v6.
  **Поток 0A, domain 1.6/1.7 и config 2.10–2.12 завершены**;
  **3.12a завершён** (fresh v15 и guarded additive v6→v15);
  **3.12b завершён** (historical writer indexes и v11/v14 upgrade);
  **3.12c завершён** (explicit activation и fenced baseline refresh);
  **3.12d завершён** (writer reservations/scope admission/reconcile);
  **3.12e завершён** (worktrees/quarantine lifecycle);
  **3.12f завершён** (budget persistence/parse);
  **0B.1–0B.5 завершены** (v17 manifest и delta corpora);
  **7.13 завершён**; согласованный блок 7.14/7.15/8.18/7.18 завершён;
  следующий незавершённый исторический шаг — **7.8** (Auto-approval integration);
  новые возможности v7–v17 в Rust не завершены.

### 3.13. Schema v16/v17 extension (завершено)

Завершённый 3.12 остаётся foundation v15. Каждый шаг здесь сохраняет sidecar,
namespace, meta/runtime_owner и Rust/Python isolation guards; поддержка новых
версий появляется только с проверенным полным schema contract.

- **3.13a. Additive schema16 (завершено).** `tasks.delivery_mode TEXT NOT NULL DEFAULT
  'manual'`; fresh target16 и Rust-owned v15→16, historical supported upgrades
  доводятся до нового target без потери строк. Read-only v15 не мигрируется.
  Source: `Storage._migrate` (v17). Checks: defaults/row preservation,
  exact contract, ownership и transactional failure. Depends on 0B.3.
  Реализованы fresh16 и additive Rust-owned v6/v11/v14/v15→16 в одном
  BEGIN IMMEDIATE с guards и финальной проверкой схемы. Старые строки
  получают manual; read-only inspection v15 не пишет и не мигрирует.
  274 storage tests: независимый delta schema/default contract, сохранение
  всех таблиц v15, idempotent initialize, foreign owner/default refusal и
  rollback DDL/user_version/meta при trigger failure.
- **3.13b. Frozen delivery policy persistence (завершено).** Mapping/create/replay/read-only
  читают эффективный task mode; existing rows manual, new rows получают
  submit-time config. Live config не влияет на repeat accept. Source:
  `Storage.create_task_with_round`, `_row_to_task`, `_row_to_task_readonly`,
  `_normalize_delivery_mode`. Checks: legacy/default/frozen/replay/corrupt
  normalization. Depends on 3.13a, 1.8. Opens 8.19a.
  Task mapping читает delivery_mode из сохранённой строки; отсутствующий
  столбец, неизвестная строка и неверный SQLite type дают Manual.
  AdmissionSettings принимает worktree-only OnAccept; создание сохраняет
  политику в одной transaction с task/round/profile/reservation/checkout.
  Submit service передаёт config mode; replay возвращает старую policy до
  применения текущих settings. Request hash сохраняет historical semantics:
  смена config не конфликтует с повтором запроса. Проверены оба направления
  config switch, readonly mapping, corrupt/legacy fallback и rollback;
  автоматическая доставка при accept остаётся consumer 8.19a.
- **3.13c. Additive schema17 (завершено).** `automation_runs(run_id,status,control,document,
  created_at,updated_at)`, control default run; unique expression index
  `ux_automation_unfinished ON automation_runs((1)) WHERE status NOT IN
  ('completed','ready','stopped')`. Fresh17 и Rust-owned16→17; paused/blocked
  удерживают unfinished slot. Checks: exact DDL/default/predicate, guard,
  migration rollback, preserved task/delivery rows. Depends on 3.13a, 0B.3.
  Fresh target17 и upgrades всех явно поддержанных Rust-owned versions
  выполняются в одной transaction. Schema guard проверяет полный contract,
  default control=run, expression (1) и точный unfinished predicate.
  Независимый frozen17 manifest, сохранение v16 строк/delivery policy, rollback
  DDL/index/version markers, terminal/paused/blocked slot и corrupt index/default
  refusal проверены тестами; историческая read-only v16 поддержка сохранена.

### 3.14. Automation run storage (v17, завершено)

- **Цель:** create/load/latest/save/control, typed run identity/status/control
  и persisted document, один unfinished run на project DB. Run statuses:
  running/paused/blocked/completed/ready/stopped; control: run/pause/stop.
- **Source:** `automation.RunStore:158-230` (v17).
- **Приёмка:** read-only load не создаёт state; UUID/document identity
  проверяются; unsupported/corrupt state возвращает error; terminal controls
  отвергаются; repeated/concurrent create не обходят unique fence.
- **Проверки:** storage CRUD/readonly/rollback/race fixtures.
- **Зависит от:** 3.13c, 0B.5. Открывает поток 16.
- **Результат:** `AutomationRunStore` привязан к явному Rust-owned layout;
  create валидирует canonical UUID/document до initialization и сохраняет
  running/run в BEGIN IMMEDIATE. Load по id/latest использует read-only
  transaction с sidecar/namespace/owner/full schema17 guards, missing state
  не создаётся. Save атомарно сохраняет document/status и текущий control;
  control проверяет terminal status под той же write transaction. Завершённый
  run нельзя вернуть в незавершённый status; новая работа требует нового run id.
  Typed UUID/status/control и redacted outcomes/errors не раскрывают document.
  Семь тестов покрывают CRUD/latest, persisted identity и corrupt state,
  read-only/foreign/old-schema guards, terminal fence, rollback, concurrent
  create/control/save и WAL conversion contention. Дополнительно 30 повторов
  fresh concurrent-create: один winner, второй UnfinishedRun. Для первого
  переключения WAL добавлен bounded 30s retry исключительно SQLITE_BUSY/LOCKED;
  обычный busy_timeout=30000 восстанавливается после операции.
  Coordinator FSM, plan validation и Codex calls остаются в потоке 16.

### 3.15. Atomic needs_user recovery claim/release (завершено)

- **Цель:** сохранить текущий round, взять spawn claim и снять его при
  failed spawn только по совпадающей lease.
- **Source:** `Storage.claim_needs_user_recovery:2994-3080`,
  `release_needs_user_recovery:3079-3124` (v17).
- **Приёмка:** ровно один caller переводит task в implementing/revising;
  parked needs_user round → observing, pending round остаётся pending;
  worker_started_at/event атомарны. Close-request gate; no new round,
  revision_count/outbound message не меняются. Lease overwritten by started
  worker не откатывается. Это API поверх существующих полей, без новой DDL.
- **Проверки:** claim/release/concurrency/rollback/no-resend storage tests.
- **Зависит от:** 0B.4, 3.9, 3.12d. Открывает 7.10a/b, 8.20.
- **Результат:** `claim_needs_user_recovery` и typed opaque `RecoveryClaim`;
  current-round/status/project/close guards, одно atomic status/lease/event
  изменение, сохранение outbound/session/attempted/revision_count. Parked round
  переходит в observing, остальные open statuses сохраняются. Release проверяет
  исходную lease и последний claim event id (защита от двух claims в одну ms),
  не откатывает terminal/close/new-round/new-worker state. `mark_worker_started`
  различает timestamp даже при старте в ms claim. Error/event failures откатывают
  все строки. Это API поверх schema v15, без DDL/HTTP/spawn; 6 tests (включая
  10 round-kind/status cases, concurrent winner и rollback) и storage clippy.


## Поток 4. Security и Git

### 4.1. Простые command tokens

- **Завершено.** Новый workspace-crate `bridge-command-policy` воспроизводит
  только базовую семантику токенизации простых команд и leading environment
  assignments из reference `command_policy.py` (`basename`, `is_assignment`,
  `split_command`, `leading_assignments`) и не принимает policy-решений.
  `split_command` — POSIX-токенизатор, эквивалентный Python
  `shlex.split(text, posix=True)` с `comments=False` и `whitespace_split=True`:
  кавычки снимаются, quoted whitespace сохраняется, backslash экранирует
  следующий символ вне одинарных кавычек, соседние quoted/unquoted фрагменты
  склеиваются, `#` — обычный символ, а `\r`/`\n`/`\t`/пробел разделяют токены.
  Crate `shlex` не используется, потому что он трактует `#` как комментарий и
  `\<newline>` как продолжение строки, тогда как Python — нет. Как fail-closed
  расширение NUL отклоняется (reference отклоняет NUL до токенизации), хотя
  Python `shlex` сохранил бы его. `basename` повторяет `token.rsplit("/", 1)[-1]`,
  `is_assignment` требует непустое имя, начинающееся с alphabetic или `_` и
  состоящее далее из alphanumeric или `_`, а `leading_assignments` отделяет
  только ведущие assignments от argv, сохраняя порядок первого появления имени
  и last-value-wins для повторяющихся имён. Узкий typed API представлен
  `TokenizeError` без payload (ошибка не содержит исходную команду/токены) и
  `LeadingAssignments` с доступами `variables`/`get`/`argv`. Unit-тесты
  покрывают простые команды, whitespace, одинарные/двойные кавычки, quoted
  spaces/backslash, path-prefixed executable, валидные и невалидные
  assignments, повторяющиеся assignments, assignment-only вход,
  empty/whitespace вход, NUL и unmatched quotes. Запрещённые Git writes,
  wrappers env/sudo/command/exec/nohup/nice/time, shell executables, raw shell
  metacharacters/globs и permission decision остаются задачами 4.2/4.3.

### 4.2. Запрещённые Git writes и wrappers

- **Завершено.** `bridge-command-policy` переносит token-level семантику
  reference `command_policy.py`: `git_invocation_problem`, `env_invocation_problem`
  и wrapper-aware `tokens_problem`. Запрещённые `git add`/`commit`/`push`
  распознаются после пропуска leading (и любых) `NAME=value` assignments,
  path-prefixed executable (`/usr/bin/git`, `./git`) и wrappers `env`, `sudo`,
  `command`, `exec`, `nohup`, `nice`, `time` (включая вложенные и
  path-prefixed wrappers). `git` global options с отдельным значением
  (`-C`/`-c`/`--git-dir`/`--work-tree`/`--namespace`/`--exec-path`/
  `--config-env`/`--super-prefix`/`--shallow-file`) и `--opt=value`
  пропускаются; glob в позиции подкоманды даёт `unprovable_git_glob`; basename
  подкоманды сравнивается регистронезависимо (`git ADD`).
  `env -S`/`--split-string` (включая `--split-string=`, `-Sxxx`) даёт
  `unprovable_wrapper_command`. Узкий typed API: `tokens_problem` и
  `policy_decision`, `PolicyReason` (`git_write_blocked`/`unprovable_git_glob`/
  `unprovable_wrapper_command`) и `PolicyDecision`; решения и причины не несут
  command text, argv, пути и secrets, а Display/Debug содержат только
  статические строки. Как задокументированное fail-closed расширение над
  reference (intentional difference) `env` command-splitting отклоняется также
  для combined short-option cluster с флагом `S` (`-iS`, `-0S`, `-vS`) и
  однозначных long-option аббревиатур `--split-string` (`--split`, `--spl`,
  `--s`), а неизвестные, неоднозначные и value-missing `env` options
  завершаются fail closed; reference пропускает такие формы
  (`env -iS 'git push'`, `env --split 'git push'` действительно выполняют
  split-команду). Table-driven тесты покрывают read-only и запрещённые git
  операции, path-prefixed и абсолютные пути, `git` global options, glob
  подкоманды, все поддерживаемые wrappers, вложенность, assignments,
  combined/abbreviated/malformed env options и representative in-scope corpus
  из `docs/fixtures/command-policy-cases.json` (42 case, семантически сверены с
  Python reference; metacharacters/globs/shell executables/`eval` — 4.3).
  Существующие тесты 4.1 продолжают проходить. Cargo.toml/Cargo.lock не
  менялись.

### 4.3. Fail-closed compound shell syntax

- **Завершено.** `bridge-command-policy` переносит raw-pattern семантику reference
  `command_policy.py`: новый `bash_pattern_problem` (и typed
  `bash_pattern_decision`) проверяет raw-строку до токенизации, поэтому
  shell-синтаксис, который нельзя статически разрешить, отклоняется fail closed
  как `unprovable_shell_syntax` даже внутри кавычек: separators (`;`/`&&`/`||`),
  pipes (`|`), background (`&`), redirects (`<`/`>`), command substitution
  (`$(`), backticks, subshells, brace/variable expansion (`$`, `${}`, `{}`) и
  newline/CR. Пустой/whitespace-only вход даёт `empty_bash_pattern`, NUL или
  неразбираемая строка — `unparsable_bash_pattern`. `tokens_problem` расширен до
  полной reference-семантики: general glob в обычном токене даёт
  `unprovable_glob_command`; shell executables (`sh`, `bash`, `zsh`, `dash`,
  `ksh`, `fish`, включая path-prefixed) либо перепроверяются через `-c`
  (вложенный `tokens_problem`), либо отклоняются как `unsafe_shell_invocation`
  (без `-c`, включая combined `-lc`) и `shell_command_missing` (`-c` без
  строки); `eval` склеивает аргументы и заново проверяет их через
  `bash_pattern_problem`. Узкий typed API: `bash_pattern_problem`,
  `bash_pattern_decision`, `tokens_problem`, `policy_decision`; новые
  `PolicyReason` (`empty_bash_pattern`/`unprovable_shell_syntax`/
  `unprovable_glob_command`/`unsafe_shell_invocation`/`shell_command_missing`/
  `unparsable_bash_pattern`) и `PolicyDecision` не несут command text, argv, пути
  и secrets, а Display/Debug содержат только статические строки. Семантика
  воспроизводит reference точно, включая две наблюдаемые детали: raw-скан идёт
  до токенизации (метасимвол/glob внутри кавычек всё равно виден) и shell без
  `-c` (например `bash --version`) отклоняется как `unsafe_shell_invocation`.
  Table-driven тесты покрывают empty/whitespace, NUL/untokenizable, все
  separators/pipes/background/redirects/substitution/backticks/subshell/
  expansion/newline, quoted metacharacters и globs, shell executables (`-c`,
  missing, nested, path-prefixed, combined options), `eval`, а также все
  относящиеся к 4.3 cases из `docs/fixtures/command-policy-cases.json`;
  representative in-scope corpus 4.1–4.2 продолжает проходить. Verifier-конверт
  (`missing_executable` для assignment-only `test_command`) остаётся задачей 5.1.
  Существующие тесты 4.1–4.2 не регрессировали. Cargo.toml/Cargo.lock не
  менялись.

### 4.4. Workspace-relative allowed paths

- **Завершено.** Новый workspace-crate `bridge-path-policy` реализует только
  workspace-relative лексическую валидацию и нормализацию `allowed_paths` из
  reference `git_snapshot.validate_allowed_paths`, не обращаясь к файловой
  системе. Пустой список разрешён и нормализуется в пустой scope. Для каждого
  entry сохраняется reference-порядок проверок: пустая строка/не-строка
  (`invalid_allowed_paths_entry`), backslash (`backslash_in_path`), компонент
  `..` до нормализации (`parent_traversal`, поэтому `a/../b` отвергается),
  сведение к empty/`.` включая голый корневой слэш (`empty_or_dot_path`,
  `./`, `/`, `//`) и абсолютный путь (`absolute_workspace_path`). Оставшиеся
  относительные file/directory scopes нормализуются детерминированно: `.` и
  пустые компоненты удаляются, trailing `/` сохраняет семантическое различие
  file и directory scope, а missing file/directory обрабатываются лексически
  без требования существования. Узкий typed API — `validate_allowed_paths`
  (строковые entries) и boundary-уровень `validate_allowed_path_entries` с
  `AllowedPathEntry::NonText` для non-string JSON-значения; типизированный
  `PathPolicyReason` (`as_str`/`Display`/`Error`) не несёт входной путь, имена
  и secrets. Table-driven тесты покрывают все относящиеся к 4.4 cases и
  variants из `docs/fixtures/path-policy-cases.json`, включая
  `invalid_allowed_paths_entry` для non-string на boundary API, а также
  нормализацию, детерминизм и отсутствие утечки путей в ошибках. Symlink
  confinement (4.5), trusted external directories/абсолютные внешние пути и Git
  repository discovery (4.6), scope/snapshot/сравнение (4.7–4.9) и MCP-envelope
  не входят. Существующие crates и их public API не менялись.

### 4.5. Symlink confinement

- **Завершено.** `bridge-path-policy` расширен filesystem-aware слоем поверх
  лексической семантики 4.4, не меняя существующий узкий API
  (`validate_allowed_paths`, `validate_allowed_path_entries`) и его поведение.
  Новые typed-функции `validate_workspace_allowed_paths` и
  `validate_workspace_allowed_path_entries` принимают workspace и
  workspace-relative `allowed_paths`: workspace канонизируется через
  `fs::canonicalize`, каждый entry сначала проходит reference-лексические
  проверки 4.4, а затем лексически нормализованный relative scope разрешается
  относительно canonical workspace по семантике Python
  `Path.resolve(strict=False)` — существующие symlink-компоненты следуются
  (абсолютный target перезапускает разрешение от корня FS, относительный
  разрешается от родителя ссылки, `.`/`..` внутри target сворачиваются),
  отсутствующий компонент завершает разрешение, а оставшийся хвост
  присоединяется лексически. Symlink в промежуточном компоненте обрабатывается
  так же, как конечный. Если resolved target не равен canonical workspace и не
  является его потомком, entry отклоняется как стабильная reason category
  `workspace_escape` (`PathPolicyReason::WorkspaceEscape`), покрывая и
  существующий, и dangling symlink escape; разрешённый entry сохраняет
  нормализованный исходный workspace-relative scope (не target path) с
  сохранением trailing-slash различия file/directory. Symlink loops и
  I/O/canonicalization failures (неканонизируемый workspace, non-`NotFound`
  metadata-ошибки, например не-каталог в компоненте, нечитаемая ссылка)
  завершаются fail closed типизированными `PathPolicyReason::SymlinkLoop`/
  `ResolutionFailure` — задокументированное fail-closed расширение над
  reference, который в non-strict режиме молча продолжает лексически. Все
  ошибки не несут payload и не раскрывают входные пути, workspace, symlink
  target, secrets или OS error text в `Display`/`Debug`. Table-driven тесты
  воспроизводят четыре относящиеся к 4.5 ветви
  `docs/fixtures/path-policy-cases.json`
  (`validate-relative-symlink-inside-scope`,
  `validate-relative-symlink-escape`, `validate-dangling-symlink-inside-scope`,
  `validate-relative-dangling-symlink-outside`), а также промежуточные
  symlink-компоненты, сворачивание `..` в target, symlink loop, отказ
  канонизации/не-каталога, сохранение лексических отказов 4.4 и отсутствие
  утечки путей/target/OS-text. Absolute external paths, trusted roots и Git
  repository discovery (4.6), scope/snapshot/сравнение (4.7–4.9) и MCP-envelope
  не входят. Cargo manifests/dependencies не менялись, тесты 4.4 не
  регрессировали.

### 4.6. External Git repository paths

- **Завершено.** `bridge-path-policy` расширен trusted-root слоем поверх
  лексической (4.4) и workspace symlink-confinement (4.5) семантики, не меняя
  существующий узкий API и его поведение. Новые typed-функции
  `validate_allowed_paths_with_trusted_roots` и
  `validate_allowed_path_entries_with_trusted_roots` принимают workspace,
  trusted external roots и `allowed_paths`: workspace и каждый trusted root
  канонизируются fail closed, относительные entries обрабатываются ровно как в
  4.5, а абсолютные разрешаются по семантике Python `Path.resolve(strict=False)`
  и проверяются в reference-порядке. Абсолютный путь, разрешающийся внутри
  canonical workspace, отклоняется как `absolute_workspace_path` **до**
  trusted-root lookup; затем путь вне всех trusted roots отклоняется как
  `outside_trusted_roots` (в том числе при escape через symlink). Для
  допустимого пути канонический root содержащего Git worktree находится через
  `git rev-parse --show-toplevel` (ближайший существующий предок для missing
  leaf, сам repo root и child поддержаны), probe ограничен таймаутом с
  принудительным kill/reap, timeout — fail-closed `path_resolution_failure`, а
  stdout читается как raw Unix path bytes без lossy/strict UTF-8 (non-UTF-8 repo
  root распознаётся), отсутствие репозитория даёт
  `not_git_repository`, а repo root вне trusted roots —
  `external_repo_root_outside_trusted`, поэтому вложенный trusted subdir внутри
  более высокого репозитория не расширяет доверие. Нормализованный validated
  scope типизирован как `ValidatedAllowedPath`: workspace-relative entries
  остаются относительными, external entries — каноническими абсолютными, а
  `PathScope` (`File`/`Directory`) сохраняет trailing-slash различие. Узкий typed
  API `group_allowed_paths_by_repo` группирует raw entries по каноническому
  repository root: relative entries без нормализации/валидации попадают в
  canonical main workspace, absolute — в найденный repository root, trusted roots
  не применяются; main-workspace bucket всегда присутствует и идёт первым,
  внешние bucket-ы детерминированно отсортированы. Ошибки представлены
  payload-free `PathPolicyReason` с новыми стабильными категориями
  `outside_trusted_roots`/`not_git_repository`/`external_repo_root_outside_trusted`
  и не раскрывают workspace, roots, входные пути, Git output, symlink target или
  OS error text в `Debug`/`Display`; symlink loop и I/O/canonicalization failures
  остаются fail closed (`symlink_loop`/`path_resolution_failure`). Table-driven
  тесты покрывают все относящиеся к 4.6 cases `validate_allowed_paths` и
  `group_allowed_paths_by_repo` из `docs/fixtures/path-policy-cases.json`
  (`validate-absolute-trusted-repo-file`, `validate-absolute-trusted-repo-directory`,
  `validate-absolute-outside-trusted`, `validate-absolute-repo-root-outside-trusted`,
  `validate-absolute-symlink-escape-outside-trusted`,
  `validate-absolute-trusted-not-git-repository`, `validate-absolute-workspace-file`,
  `validate-absolute-workspace-directory`, `group-empty-list`,
  `group-external-not-git-repository`, `group-external-repo-root-and-child`,
  `group-relative-and-external`, `group-two-external-repos`), а также repo
  root/child, missing leaf, trusted symlink escape, nested trusted root, missing
  trusted root, boundary non-string, сохранение raw entries, отсутствие утечки и
  regression 4.4–4.5. Cargo manifests/dependencies не менялись.

### 4.7. Snapshot основного repository

- **4.7a. Базовый снимок — завершено.** Новый workspace-crate
  `bridge-git` (без зависимостей от других bridge-crates) реализует
  изолированный read-only слой для базового состояния одного Git worktree.
  Проверка worktree эквивалентна `git rev-parse --is-inside-work-tree`:
  `false`/nonzero даёт typed `GitError::NotRepository`, а
  spawn/wait/timeout/I/O и malformed output завершаются fail closed
  инфраструктурной ошибкой. `head` вызывает `git rev-parse HEAD`: nonzero даёт
  `None` для валидного repository без commit, а успешный ответ принимается
  только как opaque ASCII commit id строгой формы (40 или 64 lowercase hex) без
  lossy decode. `status --porcelain=v1 -z` парсится собственным parser-ом:
  обычные entries и обе стороны rename/copy, пути как Unix `OsString` bytes без
  lossy `String`, malformed porcelain — fail closed, результат сортируется и
  дедуплицируется. `index_fingerprint` — SHA-256 от точных bytes
  `git ls-files --stage -z`, затем NUL separator, затем `git ls-files -v -z`
  (SHA-256 реализован внутри crate и сверен с FIPS 180-4 vectors, внешних
  зависимостей нет). `RepositorySnapshot` содержит HEAD, raw status/dirty
  paths, index fingerprint, а с 4.7b — ещё manifest и worktree fingerprint;
  фиктивные поля не добавлены. Git command runner ограничен: фиксированный
  executable `git`, stdin null, stderr отбрасывается (не попадает в ошибку),
  stdout читается как raw bytes отдельным потоком (нет pipe deadlock),
  wall-clock timeout с принудительным kill/reap; shell и Git write-команды
  отсутствуют. Узкий payload-free `GitError` не раскрывает workspace, argv,
  stdout/stderr, Git config, OS errors и secrets через `Display`/`Debug`. Тесты
  выполняются только во временных synthetic repositories: clean, no commit, dirty
  tracked/untracked, staged, intent-to-add, rename/copy parser, malformed
  porcelain, non-repository, timeout/kill/reap, non-UTF-8 Unix paths и
  стабильность index fingerprint. Manifest/хеширование файлов и worktree
  fingerprint реализованы в 4.7b; changed/committed paths, history ancestry,
  scope/policy violations, external/multi-repository snapshots и worker/MCP
  integration не входят. Существующие crates, их schema/public API и fixtures не
  менялись.

- **4.7b. Worktree manifest и `worktree_fingerprint` — завершено.** Тот же
  `bridge-git` (внешних зависимостей по-прежнему нет) добавляет детерминированный
  manifest рабочего дерева и стабильный SHA-256 fingerprint. `worktree_manifest`
  берёт ровно релевантные Git-ом файлы: tracked из `git ls-files -z` и
  non-ignored untracked из `git ls-files --others --exclude-standard -z` (ignored
  entries исключает сам Git), а отсутствующий в worktree tracked путь (deletion)
  не даёт entry. Пути — точные Unix `OsString` bytes без lossy UTF-8; entries
  упорядочены ровно как Python `sorted()` над surrogateescape-decoded именами
  (code point order, а не raw bytes: одиночный invalid byte `0xff` → `U+DCFF`
  идёт перед supplementary `U+1F600`) и дедуплицируются, поэтому manifest
  детерминирован и совпадает с reference для любого byte sequence.
  Digest entry фиксирует тип, executable bit и symlink identity: обычный файл —
  `sha256("file\0" || content)`, при любом executable bit — `sha256("exec\0" ||
  content)`, symlink — `sha256("link\0" || raw target bytes)` (target-файл не
  читается). `worktree_fingerprint` воспроизводит компактный digest reference
  `verifier.fingerprint`: `sha256` по каждой entry в отсортированном порядке
  (`path || 0x00 || lowercase hex digest || 0x00`), затем `b"status\0"` и raw
  status bytes; сверен с Python reference на фиксированном synthetic repository.
  Fail-closed расширение над reference: файловая ошибка даёт payload-free
  `GitError::ManifestIo`, а listed entry, не являющийся обычным файлом или
  symlink (directory, FIFO, socket, device), — `GitError::UnsupportedFileType`;
  malformed `-z` framing — `MalformedOutput`. `RepositorySnapshot` получил
  `manifest()`/`worktree_fingerprint()`, вычисляемые из тех же status bytes.
  Focused tests во временных synthetic repositories покрывают stable clean
  snapshot, tracked content change, staged/untracked, ignored exclusion,
  executable-bit-only change, symlink retarget/dangling/ignored target,
  non-UTF-8 paths, deterministic ordering (включая mixed supplementary-Unicode
  и invalid-byte имена, сверенные с Python reference), deletion, fail-closed
  directory/FIFO/unreadable cases и совпадение с Python reference. Multi-repository
  snapshots (4.8), comparison/violations (4.9) и worker/MCP integration не входят;
  Cargo manifests/dependencies не менялись.

### 4.8. Multi-repository snapshots

- **Завершено.** `bridge-git` получил новый модуль `multi_repo`, который
  оркестрирует read-only снимки основного workspace и всех затронутых внешних
  репозиториев поверх существующего `take_snapshot` (4.7) и типизированных
  repository bucket-ов `bridge-path-policy::group_allowed_paths_by_repo` (4.6).
  Crate получил локальную dependency `bridge-path-policy`; `bridge-path-policy`
  не изменялся. Узкий typed API: `take_multi_repository_snapshot(main_workspace,
  &[AllowedPathGroup])`, результат `MultiRepositorySnapshot` из
  `RepositoryGroupSnapshot` (`root()`, `allowed_paths()`, `snapshot()`), плюс
  payload-free `MultiRepoError`. Контракт: ровно один snapshot на каждый
  уникальный canonical repository root; main repository всегда присутствует и
  идёт первым даже при пустом `allowed_paths`; внешние репозитории следуют в
  детерминированном порядке по возрастанию canonical root; каждый entry
  сохраняет canonical root, исходные raw allowed entries своего bucket-а и
  соответствующий snapshot, поэтому одинаковые relative пути в разных
  репозиториях не смешиваются. Перед snapshot каждого repository bucket root
  сверяется с фактическим canonical Git worktree root
  (`git rev-parse --show-toplevel`, canonicalize, точное равенство). Fail closed
  завершаются: неканонизируемый main workspace
  (`workspace_resolution_failed`), неканонизируемый/исчезнувший bucket root
  (`repository_resolution_failed`), пустой список bucket-ов
  (`missing_main_repository`), первый bucket не main
  (`main_repository_mismatch`), duplicate/conflicting canonical root
  (`duplicate_repository_root`), не-Git-worktree (`not_a_git_repo`), подмена,
  вложенный или иначе не-canonical root (`repository_root_mismatch`) и любая Git
  infrastructure-ошибка probe/snapshot (`MultiRepoError::Git`). Ошибки не несут
  payload и не раскрывают roots, allowed paths, Git output, argv, OS error text и
  secrets в `Debug`/`Display`; shell и Git write-команды отсутствуют. Тесты во
  временных synthetic repositories покрывают main-only с пустыми allowed paths,
  main + один/два внешних репозитория с детерминированным порядком, repo
  root + child в одном snapshot, одинаковый relative filename в разных
  репозиториях без смешивания, независимые dirty/head/index/worktree состояния,
  duplicate/root-mismatch/disappeared/non-repository fail closed, payload-free
  errors и regression 4.7 (`take_snapshot` переиспользуется без изменений).
  Observable semantics сверены с Python reference multi-repository verifier flow
  (`git_snapshot.take_external_snapshots` + main snapshot в
  `mcp_server.submit_task_impl`) и релевантными fixtures
  `docs/fixtures/path-policy-cases.json` (group-* cases) и
  `docs/fixtures/git-snapshot-cases.json`; 4.9 comparison/violations,
  `changed_paths`/`committed_paths`, ancestry и worker/MCP integration не входят.
  `Cargo.toml`/`Cargo.lock` изменились только добавлением локальной dependency
  `bridge-path-policy` у `bridge-git`.

### 4.9. Snapshot comparison и violations

- **Завершено.** `bridge-git` получил typed API сравнения одного и нескольких
  repository baseline: `compare_repository_snapshot` и
  `compare_multi_repository_snapshot`, результаты `RepositoryComparison` /
  `MultiRepositoryComparison` и стабильный `GitPolicyViolation`. Сравнение
  вычисляет worktree `changed_paths` по manifest, `committed_paths` через
  read-only `git diff`/`diff-tree`, объединённые scope violations с точной
  file/directory семантикой и Git-policy violations в reference-порядке:
  `history_rewritten`, `head_changed`, `index_changed`; `allow_commit=true`
  подавляет только последние два. Missing main/external repository даёт
  соответственно `not_a_git_repo`/`external_repo_missing` без Git-команд над
  исчезнувшим root. Multi-repository сравнение сохраняет baseline-порядок,
  canonical root и независимые repository-relative результаты; external scope
  проверяется по абсолютному root-qualified candidate, но наружу возвращаются
  relative paths. Commit ids передаются Git как отдельные OS-arguments, shell и
  Git writes отсутствуют; команды ограничены прежним timeout/kill/reap runner.
  NUL path output парсится fail closed, Unix non-UTF-8 paths сохраняются без
  lossy decode и сортируются по Python surrogateescape-порядку. Ошибки остаются
  payload-free. Focused synthetic-repository tests покрывают clean/dirty,
  file/directory scope, commit, ancestry rewrite, `allow_commit`, missing
  external repository и независимое main/external сравнение; workspace format,
  clippy и tests проходят. Worker/MCP flat aggregation остаётся последующей
  интеграцией, а не частью security/Git primitive.

**Готовность потока:** решения совпадают с security corpus.

## Поток 5. Verifier

### 5.1. Test command validation

- **Завершено.** `bridge-command-policy` получает узкий verifier-level API поверх
  существующих policy primitives: `validate_test_commands(commands: &[&str]) ->
  Vec<TestCommandProblem>`, эквивалент Python
  `verifier.validate_test_commands` для строковых входов. Пустой список валиден и
  даёт пустой результат; каждая команда сначала проверяется существующей
  fail-closed семантикой `bash_pattern_problem`, а команда, у которой после
  ведущих `NAME=value` assignments остаётся пустой argv, отклоняется отдельной
  стабильной причиной `missing_executable`. Порядок результатов и индексы
  сохраняются как в Python `enumerate`. Типизированный `TestCommandReason`
  (`Policy(PolicyReason)` / `MissingExecutable`) и `TestCommandProblem`
  (`index`/`reason`) не несут command text, argv, пути и secrets;
  `Debug`/`Display` содержат только индекс и статическую причину.
  Не-строковые/не-list входы verifier относятся к configuration/transport и в
  crate не входят, как и раньше. Table-driven тесты покрывают
  все cases и variants `context=test_command` из
  `docs/fixtures/command-policy-cases.json` (safe commands, policy denials,
  assignment-only, пустой/whitespace вход), пустой список,
  сохранение порядка/индексов и отсутствие утечки входа в `Debug`/`Display`;
  существующие тесты 4.1–4.3 не регрессировали. Cargo.toml/Cargo.lock не
  менялись.

### 5.2. Один command runner

- **Завершено.** Новый workspace-crate `bridge-verifier` реализует ровно один
  production-примитив: запуск одной заранее согласованной test command и
  типизированный результат, совместимый по смыслу с per-command записью
  reference `verifier.py:_run_command`. Публичный API —
  `run_test_command(workspace: &Path, command: &str, timeout: Duration,
  tail_bytes: usize) -> Result<CommandRunOutcome, CommandRunError>`, константа
  `DEFAULT_TAIL_BYTES = 4000` и аксессоры `CommandRunOutcome`
  (`exit_code`/`timed_out`/`duration`/`output_tail`/`succeeded`). Перед spawn
  команда проверяется существующей fail-closed
  `bridge_command_policy::validate_test_commands`: empty, assignment-only,
  shell/glob/Git-write и прочие отклонённые команды дают
  `CommandRunError::Rejected(TestCommandReason)` и никогда не доходят до ОС.
  Валидная команда токенизируется `split_command`, ведущие `NAME=value`
  assignments отделяются `leading_assignments`: argv передаётся процессу
  отдельными OS arguments (без shell и интерполяции), а ведущие assignments
  накладываются на унаследованное окружение процесса. Ребёнок запускается в
  явно заданном `workspace` (cwd) лидером собственной process group через
  безопасный API `command-group` (стандартный `process_group(0)`), stdin
  закрыт, stdout и stderr — pipes. Команда считается завершённой только когда
  прямой ребёнок reap-нут и оба pipe дошли до EOF: потомок, унаследовавший
  pipes, удерживает команду до deadline, как reference
  `communicate(timeout=...)`. При timeout или wait failure убивается вся
  process group (`SIGKILL` с `killpg`-семантикой `command-group`), reap-ается
  прямой ребёнок. Read-end каждого pipe на Unix переводится в non-blocking, а
  reader-поток ограничен тем же deadline, поэтому он завершается и джойнится в
  пределах одного poll-интервала даже тогда, когда потомок, создавший
  собственную group/session, ускользнул от группового kill и всё ещё держит
  унаследованный pipe: ни вечно заблокированный reader-поток, ни зависший
  вызов не остаются. Reap-ается только прямой ребёнок; потомок, оставшийся в
  process group, после группового kill reap-ается ОС — как и в reference
  `killpg`, а сбежавший потомок переживает timeout так же, как и в
  Python-референсе. Результат различает
  успешный exit, ненулевой exit, timeout и spawn/wait failure: у реально
  запущенной команды есть `duration` и `exit_code` (смерть от сигнала
  кодируется отрицательным номером сигнала, как Python `returncode`), timeout
  явно отмечен `timed_out`, а `output_tail` присутствует только у
  failed-команды. Output дренируется отдельным потоком на каждый stream,
  удерживаются только последние `tail_bytes` байт (без неограниченного
  удержания вывода в памяти), а комбинированный tail (`stdout` + `\n` +
  `stderr`, затем последние `tail_bytes` байт) детерминированно ограничен и по
  байтам. `CommandRunError` (`Rejected`/`Spawn`/`Wait`) не несёт command text,
  argv, environment, workspace path или output, и `Debug`/`Display` содержат
  только статический идентификатор (для `Rejected` — payload-free policy
  reason). Последовательность команд (5.3), HEAD/workspace fingerprints (5.4),
  side-effect detection (5.5) и persist-once (5.6) не входят. Focused tests
  покрывают exit 0, ненулевой exit с пустым tail, cwd, ведущие env
  assignments, отклонение assignment-only/unsafe/git-write без запуска,
  отсутствующий executable (spawn error), timeout с kill/reap (Linux-проверка
  отсутствия процесса по `/proc/<pid>/cmdline`), group cleanup на команде с
  долгоживущим потомком (`xargs -t ... sleep`: `xargs` — прямой ребёнок,
  `sleep` — потомок в той же process group; проверяется, что после timeout
  исчезли оба и что потомок реально запускался), bounded tail на
  неограниченном потоке (`yes`), комбинированный stdout+stderr tail через
  `sh -c 'ls ...'`, bounded возврат при потомке, сбежавшем в новую session
  (`setsid sleep 3`; проверяется, что вызов возвращается по deadline, не
  дожидаясь потомка), стабильные строки ошибок и отсутствие чувствительных
  входов в `Debug`/`Display`. Тесты не пишут и не исполняют свежесозданные
  executable-скрипты: это исключает наблюдавшийся при параллельном запуске
  `ETXTBSY`/`ExecutableFileBusy` и делает набор детерминированным.
  `Cargo.toml`/`Cargo.lock` менялись добавлением workspace-crate
  `bridge-verifier`, безопасной обёртки над process group `command-group`
  (транзитивно `nix`/`libc`) и Unix-only прямой зависимости `nix` (`fs`),
  используемой для non-blocking pipe read; public contracts других crates,
  schema и fixtures не менялись.

### 5.3. Последовательность команд

- **Завершено.** `bridge-verifier` получает узкий typed API последовательного
  запуска заранее согласованного списка test-команд поверх существующего
  `run_test_command` (single-command runner не дублируется):
  `run_test_command_sequence(workspace: &Path, commands: &[&str],
  timeout: Duration, tail_bytes: usize) -> Result<TestCommandSequenceOutcome,
  CommandSequenceError>`. Как и reference `verifier.run_round_verification`,
  **весь** список валидируется заранее существующей fail-closed
  `bridge_command_policy::validate_test_commands` до любого spawn: отклонённая
  команда в любой позиции даёт `CommandSequenceError::Rejected { index, reason }`
  (payload-free index и `TestCommandReason`) и ни одна команда не запускается,
  поэтому validation не может привести к частичному запуску. Пустой список
  валиден и возвращает пустой успешный результат. Далее команды выполняются
  строго в исходном порядке через `run_test_command` с теми же `timeout` и
  `tail_bytes`; runner останавливается сразу после первого non-zero exit или
  timeout, последующие команды не запускаются. Результат
  `TestCommandSequenceOutcome` содержит только фактически запущенные команды
  (`commands()`), позволяет определить общий success/failure (`succeeded()`,
  для пустого списка — true) и не несёт command text; spawn/wait failure
  валидной команды останавливает последовательность как
  `CommandSequenceError::Run { index, error }`. `CommandSequenceError` не
  раскрывает command text, argv, environment, workspace paths, output или
  secrets; `Debug`/`Display` `TestCommandSequenceOutcome` редактированы (только
  число команд и общий статус), а bounded output доступен лишь через явный
  `CommandRunOutcome::output_tail`. Focused tests покрывают пустой список,
  доказательство порядка через зависимые команды, stop-on-nonzero,
  stop-on-timeout, fail-closed pre-validation всего списка (ранняя безопасная
  команда не запускается при отклонении любой другой), rejected unsafe command,
  spawn failure без запуска последующих команд, сохранение bounded output
  семантики через single runner (`yes`) и redaction. Public contracts других
  crates, schema и fixtures не менялись; fingerprints (5.4), side-effect
  detection (5.5), persistence (5.6) и worker/MCP integration не входят.

### 5.4. HEAD/workspace fingerprints

- **Завершено.** `bridge-verifier` расширен узкой typed-оркестрацией
  `run_test_command_sequence_fingerprinted(workspace, commands, timeout,
  tail_bytes) -> Result<FingerprintedSequenceOutcome,
  FingerprintedSequenceError>` поверх общего с 5.3 внутреннего loop-примитива
  (private-хелпер над `run_test_command`; публичный контракт 5.3
  `run_test_command_sequence` сохранён без изменений) и read-only
  `bridge_git::take_snapshot` (4.7): Git snapshot/fingerprint логика не
  дублируется, crate получил только локальную dependency `bridge-git`
  (Cargo.lock — только новая строка в зависимостях `bridge-verifier`).
  `WorkspaceFingerprint` — typed представление точного reference-тройника
  `verifier.fingerprint` (HEAD, `index_fingerprint`, `worktree_fingerprint`),
  взятого из одного bridge-git snapshot, с доступорами `head()`/
  `index_fingerprint()`/`worktree_fingerprint()` и редактированным `Debug`
  (значения — только через accessors). Порядок воспроизводит reference
  `run_round_verification` точно: сначала fail-closed pre-validation всего
  списка (отклонение любой команды даёт `Rejected { index, reason }` до
  любого fingerprint и любого spawn), затем пустой список возвращает успешный
  результат **без** захвата fingerprint, затем before snapshot непосредственно
  перед sequence, затем команды через общий loop (строгий порядок и
  stop-on-first-failure/timeout сохранены), затем after snapshot — включая
  failed/non-zero, timed-out и spawn/wait-failed исходы. Git failure до запуска
  fail closed (`BeforeSnapshot`, стабильный идентификатор
  `git_fingerprint_failed`) и гарантирует, что не запущена ни одна
  test-команда. Spawn/wait failure валидной команды, как reference
  `spawn_failed` command entry, записывается в outcome: сохраняются предыдущие
  outcomes, typed `FingerprintedRunFailure` (`index()`/`error()`/`as_str()`:
  `spawn_failed`/`wait_failed`) и before fingerprint, loop останавливается, а
  after snapshot всё равно выполняется. Точная reference-семантика failure при
  after-snapshot зафиксирована в typed API: команды, run failure и before
  сохраняются в outcome, `after` отсутствует, `after_snapshot_failed()`
  сообщает failure, а типизированный общий статус
  `FingerprintedSequenceStatus` (`Succeeded`/`Failed`/`GitFingerprintFailed`,
  доступоры `status()`/`succeeded()`, стабильные идентификаторы
  `succeeded`/`failed`/`git_fingerprint_failed`) перезаписывается на
   `GitFingerprintFailed` (reference `error` + `git_fingerprint_failed`),
   поверх любого command-failure, поэтому `succeeded()` ложен и
  after-fingerprint infrastructure failure невозможно интерпретировать как
  общий успех. Ошибки payload-free; `Debug`/`Display` не раскрывают workspace
  path, command text, Git output/config, OS errors, secrets и fingerprints;
  `Debug` outcome редактирован (число команд, типизированный статус). Focused
  tests во временных synthetic Git repositories покрывают clean sequence
  (before == after, статус `succeeded`, совпадение с независимым bridge-git
  snapshot и `git rev-parse HEAD`), tracked+untracked worktree изменение (after
  отличается, index/HEAD стабильны), staged/index изменение (`git
  update-index` разрешён замороженной policy и не меняет HEAD), стабильный
  HEAD и typed mapping (repository без commit → `head` `None`), non-zero и
  timeout с after (статус `failed`), empty sequence без fingerprint (в том
  числе в non-repository — доказательство порядка), pre-validation до before
  snapshot (отклонённый список в non-repository даёт `Rejected`, а не Git
  failure), pre-snapshot failure без запуска команд, after-snapshot failure
  согласно reference (`rm -rf .git`: статус перезаписывается на
  `git_fingerprint_failed`, команды/before сохранены, `succeeded()` ложен),
  spawn failure с сохранением run и захваченным after, spawn failure с
  одновременно упавшим after snapshot (статус `git_fingerprint_failed`, run
  failure и before сохранены), typed-mapping wait failure (тот же путь
  записи) и redaction (включая recorded run failure). Side-effect
  detection/сравнение before-after (5.5), persist-once (5.6),
  multi-repository verifier integration, worker/MCP, schema/fixtures и
  Git-policy changes не входят; public contracts других crates не менялись.

### 5.5. Side-effect detection

- **Завершено (исправлено по ревью).** `bridge-verifier` расширен узким
  typed-слоем path-based side effects поверх завершённого шага 5.4; Git-логика
  не дублируется, новых зависимостей и публичных контрактов других crates не
  добавлено. `WorkspaceFingerprint` сохраняет приватный `WorktreeManifest`
  снимка, а
  `FingerprintedSequenceOutcome::side_effects() -> Option<WorkspaceSideEffects>`
  возвращает ровно frozen reference `Verification.side_effects`:
  repository-relative пути, созданные или изменённые прогоном. Список
  вычисляется из before/after manifest-записей, а не из агрегатного fingerprint
  digest: путь попадает в результат, если его after digest отличается от before,
  или он есть только в after (создание), или только в before (удаление); список
  отсортирован и дедуплицирован, пути хранятся как `OsString` (например
  `.pytest_cache/v`), поэтому non-UTF-8 путь не декодируется lossy. Сравнение
  независимо от исхода команд (non-zero, timeout и recorded spawn/wait failure
  всё равно сравнивают захваченные снимки). Clean non-empty прогон даёт пустой
  `WorkspaceSideEffects` (`Some` без путей), пустой список команд не захватывает
  fingerprints и не сообщает side effects (`None`), а недоступный after
  fingerprint из-за Git failure остаётся инфраструктурной ошибкой:
  `side_effects()` возвращает `None` (никогда не clean path result), а
  `after_snapshot_failed()`/`status()` (`git_fingerprint_failed`) и
  `succeeded()==false` сохраняются, поэтому clean run и infrastructure failure
  не смешиваются. Точные пути доступны только через `paths()`; `Debug`/`Display`
  outcome и side effects редактированы (side effects рендерят только число
  путей) и не раскрывают пути, commit id, digest, command text, output или
  secrets. Focused tests во временных synthetic repositories покрывают clean
  sequence (пустой результат), tracked modification и untracked creation (точный
  sorted список `b.txt`/`untracked.txt`), удаление tracked-файла, несколько путей
  одновременно (сортировка и дедупликация), ignored-файл не является side
  effect, non-zero и timeout с всё равно вычисленными путями, пустой список без
  side effects, before-snapshot failure без запуска команд, after-snapshot
  failure без мис-классификации (`side_effects()` `None`, статус
  `git_fingerprint_failed`) и redaction (точный путь только через `paths()`,
  редактированные `Debug`/`Display` без путей и fingerprint-значений).
  Persist-once (5.6), persistence, multi-repository/worker/MCP integration,
  schema/fixtures и Git-policy changes не входят; public contracts других crates
  не менялись.

### 5.6. Persist-once semantics

- **Завершено.** `bridge-verifier` получает узкий typed orchestration API
  `run_round_verification_persisted(storage: &mut StorageConnection,
  round: RoundRef, workspace: &Path, commands: &[&str], timeout: Duration,
  tail_bytes: usize) -> Result<PersistedVerification,
  PersistVerificationError>`, который связывает завершённый verifier flow
  (5.1–5.5) с атомарным storage lifecycle 3.9b, не дублируя command,
  fingerprint или storage logic: `bridge-verifier` получил только локальные
  dependencies `bridge-domain` и `bridge-storage`. Crate `bridge-storage`
  добавился в `[workspace.dependencies]`. Порядок reference сохранён точно:
  сначала `begin_verifier` (до любого spawn), затем существующий
  `run_test_command_sequence_fingerprinted` (fail-closed pre-validation всего
  списка → before snapshot → commands → after snapshot), затем однократный
  `complete_verifier`. Если round уже `verifier_state=done`, сохранённая
  `Verification` переиспользуется и **ни одна** test command не запускается
  (`PersistedVerificationOutcome::Reused`). Для нового или `running` verifier
  typed outcome конвертируется в компактный `bridge_domain::Verification`:
  `passed`/`failed`/`timed_out` для прогона (per-command `duration`/`exit_code`,
  `timed_out`, `output_tail` только у failed, `reason` только у команды,
  которая не запустилась), `unsafe` с `index`/`reason` для отклонённого списка
  и `error` с `git_fingerprint_failed` для упавшего fingerprint. Различаются
  две точки отказа Git: **before-snapshot failure** (5.4 `BeforeSnapshot`) —
  fail closed до любого spawn, компактный `error` без `commands`/`before`/
  `after`/`side_effects`; **after-snapshot failure** (5.4
  `GitFingerprintFailed`) — `status=error`, `reason=git_fingerprint_failed`, но
  `commands` содержат фактически выполненные команды и recorded
  `spawn_failed`/`wait_failed` entry, `before` сохраняется, `after`/`side_effects`
  отсутствуют. `before`/`after` — точный compact triple (`head`,
  `index_fingerprint`, `worktree_fingerprint`) из того же snapshot,
  `side_effects` — отсортированные пути (ключ опускается при пустом списке), а
  `log` — frozen reference `verification/<task_id>/round_<round_number>`. Первый
  результат фиксируется
  ровно один раз через `complete_verifier`: идентичный уже сохранённый
  результат даёт `Replayed` без записи, а отличающийся завершается fail closed
  typed `PersistVerificationError::Conflict` (`verifier_result_conflict`) без
  перезаписи. Storage failures дают `PersistVerificationError::Storage`, а
  отсутствие persisted verification в успешном storage outcome —
  `InvalidState`. `Debug`/`Display` `PersistedVerification`,
  `PersistedVerificationOutcome` и `PersistVerificationError` редактированы и не
  раскрывают task/project ids, команды, пути, output, fingerprints, SQL и
  secrets; `PersistVerificationError` не рендерит вложенную storage-ошибку
  (доступна только через `Error::source`). Focused tests покрывают первый запуск
  и persistence (compact passed, команды/fingerprints/log, совпадение с
  persisted row), `done` reuse без spawn (side-effecting команда не
  запускается), идемпотентный повтор идентичного входа, recovery из `running`
  (команды реально запускаются), typed conflict mapping, missing-task storage
  failure, persistence `error`-варианта при before-snapshot Git fingerprint
  failure (без команд/`before`), сохранение выполненных команд/`before` при
  after-snapshot failure (в том числе вместе со `spawn_failed` entry),
  `unsafe`-варианта при отклонённой команде, `timed_out`/`failed`/`spawn_failed`
  статусы и reason-entry, persistence side-effect путей и redaction
  `Debug`/`Display`. Concurrency persist-once доказана storage-тестами 3.9b и не
  дублируется; orchestration boundary проверен через `done` reuse и typed
  conflict mapping. SQLite schema/version/fixtures, Python state/history,
  worker/MCP integration, Git/path/command policies и public API других crates
  не менялись; `Cargo.toml`/`Cargo.lock` изменились только добавлением локальных
  dependencies `bridge-domain`/`bridge-storage` у `bridge-verifier`.

**Готовность потока:** verifier fixtures эквивалентны Python.

## Поток 6. OpenCode adapter

### 6.1. HTTP transport и basic auth

- **Завершено.** Новый workspace-crate `bridge-opencode` реализует минимальный
  переиспользуемый HTTP/1.1 transport к уже валидированному OpenCode endpoint и
  Basic Authorization, совместимую по смыслу с reference `opencode_client.py`
  (`httpx.Client(base_url=..., auth=(USERNAME, password), trust_env=False,
  headers={"accept": "application/json"})`). Transport принимает строго
  типизированные значения `bridge-config` — `Endpoint` (всегда
  `http://127.0.0.1:<port>`, без path/query/credentials) и `Secret`
  (единственная непустая строка `password_file`, task 2.8), — и не дублирует
  конфигурационную валидацию. `BasicAuth::new(Secret)` формирует
  `Authorization: Basic base64("opencode:<password>")` с константным username
  `opencode` (reference `credentials.USERNAME`) и UTF-8 кодировкой, точно как
  httpx; `BasicAuth::from_project(&ProjectEntry)` переиспользует
  `ProjectEntry::read_password` и отображает любую ошибку чтения credential в
  типизированный `TransportError::InvalidAuth`, не раскрывая путь, содержимое
  или текст ошибки. `HttpRequest` (GET/POST, относительный path, типизированный
  query, JSON body) и `HttpResponse` (`status`/`body`) образуют явный typed API.
  `HttpTransport::new(endpoint, auth, timeout)` выполняет один запрос
  `request(&HttpRequest)` по свежему `TcpStream` с `Connection: close`; весь
  обмен (connect, write, read) ограничен одним deadline от `timeout`
  (`DEFAULT_TIMEOUT = 30s`, как reference). Deadline не продлевается медленным
  или trickling peer: перед каждой частичной записью и каждым чтением
  пересчитывается `remaining(deadline)`, запись идёт циклом по `write` с
  обработкой `Ok(0)` (WriteZero — как разрыв сокета) и `Interrupted` (повтор под
  тем же deadline), а успешный `2xx` не возвращается после истечения deadline.
  Тело ответа фреймится по `Transfer-Encoding: chunked`, `Content-Length` или
  EOF; успешный `204` без `Content-Length` распознаётся как bodyless до framing
  и возвращает пустой body, не дожидаясь закрытия соединения, а `304` остаётся
  non-success ошибкой `HttpStatus(304)` и его body не читается. Статус-строка
  парсится строго: поддерживаются только версии `HTTP/1.0`/`HTTP/1.1`, а код
  обязан быть ровно тремя ASCII-цифрами в диапазоне `100..=599`;
  `HTTP/garbage 200 OK` и
  `HTTP/1.1 0200 OK` отвергаются как `TransportError::Protocol`. Типизированный
  `TransportError` различает `Timeout` (`TimedOut`/`WouldBlock`), `Unavailable`
  (connection/socket failure), `InvalidAuth` (credential setup),
  `Unauthorized` (HTTP 401, reference `OpenCodeAuthError`), `NotFound` (404),
  `HttpStatus(code)` (прочие non-success), `InvalidRequest` (malformed path,
  запрос не отправлен) и `Protocol` (невалидный HTTP). Успехом считается только
  `2xx`; тело non-success ответа никогда не читается. Query кодируется
  `application/x-www-form-urlencoded` (`url::form_urlencoded`), как reference
  `directory`-параметр. Пароль, `Authorization`, path/query, тело ответа,
  workspace-пути и OS-детали не попадают в `Debug`/`Display` и публичные типы:
  `BasicAuth` всегда `[redacted]`, `HttpRequest` скрывает path/query/body,
  `HttpResponse` показывает только status и длину, а `TransportError` — только
  static label и числовой status. Реализован только транспорт шага 6.1: health
  и workspace identity (6.2), OpenAPI compatibility (6.3), session/message APIs
  (6.4+), worker/MCP wiring и public API других crates не входят.
  `Cargo.toml`/`Cargo.lock` изменились добавлением workspace-crate
  `bridge-opencode` и уже присутствовавшей workspace-dependency `url`;
  `bridge-config`, schema и fixtures не менялись. Focused tests с локальным
  loopback mock server (без внешней сети) покрывают RFC 4648-векторы Base64,
  точный Basic-header, чтение credential из project entry и `InvalidAuth` при
  отсутствующем password file, construction GET/POST (method, path, encoded
  query, host, authorization, accept, connection, content-type/length, body),
  timeout, connection failure, mapping 401/404/400/403/500/503, chunked body,
  body без `content-length`, пустой `204` с `Content-Length: 0`, `204` без
  `Content-Length` при удерживаемом открытом соединении, большой POST к
  медленному и к вовсе не читающему peer в пределах общего deadline,
  детерминированный write-loop тест (fake clock и частичные записи) на остановку
  по общему deadline при успешных частичных записях, positive и negative
  проверки strict status parser (`HTTP/1.0`, `HTTP/1.1`, ровно три цифры,
  диапазон, `HTTP/garbage`, `0200`), malformed status line, отклонение
  malformed path без утечки, zero-timeout fail-closed и отсутствие секретов,
  путей и тел в `Debug`/`Display`.

### 6.2. Health и workspace identity

- **Завершено.** В `bridge-opencode` добавлен типизированный `OpenCodeClient`,
  связывающий transport 6.1 с одним каноническим workspace и воспроизводящий
  ровно два reference-зонда из `opencode_client.py`:
  `OpenCodeClient::health()` и `OpenCodeClient::verify_workspace()`.
  `OpenCodeClient::from_project(&ProjectEntry, timeout)` читает endpoint,
  credential и канонический workspace из уже валидированного `bridge-config`
  (`ProjectEntry::workspace`), не дублируя конфигурационную валидацию; ошибка
  чтения password-файла отображается в `TransportError::InvalidAuth`.
  `health()` отправляет `GET /global/health?directory=<workspace>` (reference
  `health()` вызывает `_json("GET", "/global/health")` со `scoped=True`, т.е. с
  `directory`-параметром) и считает сервер здоровым только если JSON-объект
  содержит литеральный boolean `true` под `healthy` — точная семантика
  reference `health().get("healthy") is True`; любое иное значение (отсутствие
  поля, строка, число, `false`, `null`) даёт `healthy == false`, а невалидный
  JSON или не-объект — `HealthError::Malformed`. Опциональная строка `version`
  доступна через `Health::version()`, но редактирована в `Debug`/`Display`.
  `verify_workspace()` отправляет `GET /path` **без** `directory`-параметра:
  reference `get_server_path` документирует, что scoped `/path` лишь отражает
  собственный workspace вызывающего, поэтому только собственный root сервера
  доказывает identity. Обязательное поле `directory` извлекается типизированно:
  отсутствие — `IdentityError::MissingDirectory`, не-строка или не-объект/невалидный
  JSON — `IdentityError::Malformed`. Полученный путь разрешается по семантике
  Python `Path.resolve(strict=False)`: существующие symlink-компоненты следуются
  (абсолютный target перезапускает разрешение от корня FS, относительный
  разрешается от родителя ссылки, `.`/`..` внутри target сворачиваются), а
  отсутствующий компонент присоединяется без остановки обхода, поэтому `..`
  после missing всё ещё сворачивается относительно symlink-разрешённого
  префикса. Лексического shortcut (раннее `reported == workspace` или lexical
  fallback после произвольной FS-ошибки) нет: reported принимается только когда
  его resolved-форма равна каноническому workspace. Несовпадение,
  отсутствующие/некорректные поля, невалидный JSON, встроенный NUL (Python
  `ValueError`), symlink loop и любая non-`NotFound` FS-ошибка
  (permission/not-a-directory/нечитаемая ссылка) дают fail-closed
  (`IdentityError::Mismatch`/`Malformed`/`MissingDirectory`). Транспортные сбои
  сохраняются как
  `HealthError::Transport`/`IdentityError::Transport` (`Timeout`, `Unavailable`,
  `Unauthorized`/401, `NotFound`/404, `HttpStatus`, `Protocol`); тело non-success
  ответа по-прежнему не читается. Новые публичные типы соблюдают redaction:
  `OpenCodeClient` не рендерит workspace, `Health` — `version`, а
  `HealthError`/`IdentityError` — только static label и вложенный
  `TransportError`; credential, `Authorization`, path/query и содержимое ответа
  не утекают. Реализованы только health и workspace identity шага 6.2: OpenAPI
  compatibility (6.3), session/message/prompt/permission APIs (6.4+),
  worker/MCP wiring, schema, config/security policies и public API других crates
  не входят. `Cargo.toml`/`Cargo.lock` изменились добавлением уже
  присутствовавшей workspace-dependency `serde_json` у `bridge-opencode`;
  `bridge-config`, schema и fixtures не менялись. Focused tests с локальным
  loopback mock server (без внешней сети и живых сервисов) покрывают успешные
  health (scoped query, `healthy`/`version`) и identity (unscoped `/path`),
  reference-правило «scoped echo игнорируется, чужой root отвергается»,
  `healthy` при отсутствующем/небулевом значении, malformed health, несовпадение
  и правила сравнения пути (точное, trailing slash, `.`, `..`, symlink alias,
  regression symlink+missing+`../..` false-positive, `..` после symlink, escaped
  NUL, чужой/несуществующий путь), missing/non-string `directory`, невалидный
  JSON, HTTP/auth failure, timeout/unavailable и redaction новых типов;
  существующие тесты 6.1 проходят.

### 6.3. OpenAPI compatibility

- **Завершено.** В `bridge-opencode` добавлен минимальный типизированный слой
  проверки установленного OpenCode `/doc` поверх transport/`OpenCodeClient`
  (6.1/6.2). `OpenCodeClient::check_compatibility()` отправляет
  `GET /doc?directory=<workspace>` — reference `get_doc` вызывает
  `_json("GET", "/doc")` с default `scoped=True`, т.е. с `directory`-query — и
  возвращает `DocCompatibility`. HTTP/auth/timeout/protocol категории
  сохраняются как `DocError::Transport(TransportError)`; успешный ответ, не
  являющийся валидным JSON, даёт `DocError::Malformed`, а валидный JSON, не
  являющийся объектом, не ошибка, а несовместимость
  `CompatibilityProblem::DocumentNotObject`; тело non-success ответа не читается.
  Чистая функция `openapi_problems(&serde_json::Value, require_prompt_model) ->
  Vec<CompatibilityProblem>` повторяет reference `opencode_client.py::
  openapi_problems` и порядок проблем: `openapi`-version, непустые `paths`
  (иначе early return), обязательные operations, обязательные schema
  properties, `AssistantMessage.time.completed`, `prompt_async` body и
  permission-reply body. Обязательные routes/methods покрывают `GET`/`POST
  /session`, `GET /session/status`, `GET /permission`, `POST
  /permission/{}/reply`, `GET /question`, health/path/session message/
  prompt_async; имена path-параметров нормализуются (`{id}`/`{sessionID}` →
  `{}`) как reference `_normalize_path`, поэтому переименование параметра не
  ломает совместимость, а существующий путь с неверным method так же
  несовместим, как отсутствующий. Обязательные schema properties `Path`/
  `Session`/`AssistantMessage` (`parentID`/`time`/`finish`) и вложенный
  `AssistantMessage.time.completed` читаются через локальный `$ref` resolver:
  chains `#/...` следуются с защитой от циклов (`seen`), а unresolvable ref
  (non-string, external, cyclic, broken/missing token) возвращает пустой узел,
  поэтому sibling-properties (например `properties.directory` рядом с
  self-`$ref`) не доказывают структуру; non-object `time.properties` тоже fail
  closed. Prompt body проверяет JSON schema
  `messageID`/`parts` и `model` **только** при
  `require_prompt_model=true`; permission reply проверяет body-schema, поле
  `reply`, list-enum и наличие `once`/`always`/`reject` (не-list enum —
  отдельная проблема). Типизированный `CompatibilityProblem` несёт только
  стабильные категории и имена обязательных контрактных полей (required
  path/schema/property из собственных констант crate), `Display` повторяет
  reference-тексты; произвольные path/schema/`$ref`/doc-значения, credentials,
  workspace и query не попадают в `Debug`/`Display`/ошибки, а
  `DocCompatibility`/`DocError` рендерят только флаг/количество и static label.
  Конфликт двух spelling'ов, нормализующихся в один path, разрешается
  first-wins в порядке JSON insertion order: `serde_json` собран с feature
  `preserve_order`, поэтому `/doc` и pure checker видят исходный порядок, как
  reference `setdefault`, и конфликт не превращает missing operation в ложный
  `compatible`. Функция тотальна: malformed JSON/не-объект/неверные nested
  types/пустые paths/broken refs/loops завершаются fail closed без panic и без
  ложного `compatible`; ошибочные nested-типы, на которых reference иногда
  бросает исключение (`components`/`properties`/`post` не-объект), трактуются
  как отсутствующие и дают несовместимость. `require_prompt_model` применяется
  однозначно: `OpenCodeClient::from_project` выводит его из
  `ProjectEntry::opencode_model` (`Some` → `true`), поэтому project без model
  совместим с документом без `model`, а project с model отвергает тот же
  документ; `OpenCodeClient::with_require_prompt_model` задаёт флаг явно, а
  `check_compatibility` всегда применяет сохранённый флаг. Это проверка
  наличия поля `model` в API body, не проверка существования конкретной
  provider/model на сервере. General-purpose OpenAPI validator намеренно не
  реализован. В `crates/bridge-opencode/Cargo.toml` включён feature
  `serde_json/preserve_order` (`Cargo.lock` дополнен транзитивным `indexmap`),
  чтобы `/doc` и pure checker сохраняли JSON insertion order; других
  dependencies не добавлялось (`url` уже присутствует), `bridge-config`, schema
  и fixtures не менялись. Focused tests покрывают reference-совместимый
  документ и fixtures, missing routes и wrong methods, переименование
  path-параметров, first-wins конфликт нормализованных paths в обеих
  очередностях raw JSON (pure и loopback), required properties и
  `time.completed` (включая non-object `properties`), inline и chained refs,
  cycles/broken/external/non-string refs с sibling-properties (включая
  body/time/permission schemas), malformed nested containers, non-object/пустые
  paths, prompt body/missing fields/conditional model, permission reply
  schema/enum/wrong method, loopback end-to-end `GET /doc` (точный scoped
  query), compatible/incompatible/malformed/non-object ответы, HTTP/auth/500
  ошибки и redaction; существующие тесты 6.1–6.2 не регрессировали.

### 6.4. Session create/list/get

- **Завершено.** `bridge-opencode` расширяет `OpenCodeClient` тремя
  типизированными session-операциями reference `opencode_client.py`, не меняя
  transport 6.1 и health/identity/doc слои 6.2/6.3. Все три запроса scoped с
  workspace `directory`-query и аутентифицируются существующим Basic auth:
  `list_sessions()` отправляет `GET /session` (reference `list_sessions`),
  `create_session(title)` — `POST /session` с компактным JSON body
  `{"title": <title>}` (reference `create_session` через httpx `json=`, т.е.
  compact UTF-8 без ASCII-escaping), `get_session(session_id)` —
  `GET /session/<id>` (reference `get_session`). Session id кодируется как один
  RFC 3986 path-сегмент: все байты вне unreserved (`ALPHA`/`DIGIT`/`-`/`.`/`_`/
  `~`) percent-encode'ятся из UTF-8, поэтому `/`, `?`, `#`, пробел, `%` и
  non-ASCII больше не могут изменить request target или внедрить query; для
  реальных alphanumeric `ses...` id кодирование — байт-в-байт no-op. Это
  единственное намеренное отклонение от reference, который подставляет id
  дословно (fail-closed hardening); пустой id и голые dot-сегменты `.`/`..`
  (которые сервер мог бы свести к другому route) отклоняются до отправки как
  `SessionError::InvalidSessionId`. Для приёма percent-encoded пути в transport
  `is_path_byte` дополнен `%` (RFC 3986 path-байт), а `validate_path` отдельно
  требует, чтобы каждый `%` начинал полный triplet `%` HEXDIG HEXDIG:
  bare/truncated/non-hex escapes (`%`, `%2`, `%GG`) отклоняются как
  `TransportError::InvalidRequest` до открытия сокета, а raw whitespace, `?`,
  `#` и control-байты остаются запрещены, поэтому действующие проверки не
  ослаблены. Типизированный `Session` хранит `id`/`title`/`directory` как
  `Option<String>` с accessor'ами `Option<&str>` — пермиссивно, как reference
  `session.get(...)`, поэтому typed-слой не изобретает ошибок, которых reference
  не даёт. `parse_session`/`parse_session_list` дают `SessionError::Malformed`
  для невалидного JSON или неверного top-level shape (не объект для create/get,
  не массив для list), а не-объектные элементы списка пропускаются ровно как
  reference `isinstance(session, dict)`-фильтр. Транспортные категории
  (timeout/unavailable/401/404/прочие HTTP/protocol) сохраняются как
  `SessionError::Transport(TransportError)`; тело non-success ответа
  по-прежнему не читается. Redaction соблюдён: `Session` не рендерит
  id/title/directory, `SessionError` — только static label и вложенный
  `TransportError`, `OpenCodeClient` — workspace; credential, `Authorization`,
  query, session content и тела ответов не утекают в `Debug`/`Display`.
  `Cargo.toml`/`Cargo.lock`, schema и fixtures не менялись (новых dependencies
  нет). Focused loopback tests (без внешней сети; mock server имеет управляемый
  lifecycle: RAII-хэндл `ServerCapture` останавливает accept-loop и join'ит
  серверный поток при drop, поэтому ни один тест не оставляет listener/thread и
  не поднимает внешних сервисов) покрывают успешные list/create/get с точным
  request (method/path/query/body/auth), compact UTF-8 body, percent-encoding id
  (`ses%2Fx%20y%3Fz%23w`), literal `%` и Unicode id, verbatim plain id,
  fail-closed unusable id без отправки запроса, приём валидных percent-encoded
  путей и отклонение bare/truncated/non-hex escapes до отправки, транспортные
  ошибки 401/404/500 для всех трёх операций, malformed list/create/get и
  redaction новых типов; существующие тесты 6.1–6.3 не регрессировали. Message
  parsing (6.5), prompt delivery, permissions/questions и worker/MCP wiring не
  входят.

### 6.5. Message list и parsing

- **Завершено.** `bridge-opencode` расширяет `OpenCodeClient` typed-операцией
  `list_messages(session_id)`, повторяющей reference
  `opencode_client.py::list_messages` (`GET /session/<id>/message`, scoped с
  workspace `directory`-query и существующим Basic auth), и переносит parsing
  message/parts, который читают reference consumers (`worker.py`, `usage.py`),
  не реализуя worker. Session id валидируется и percent-encode'ится тем же
  общим `encode_session_segment`/`encode_path_segment`, что и 6.4: пустой id и
  голые `.`/`..` отклоняются до отправки как
  `MessageError::InvalidSessionId`, а literal `%`, `/`, `?`, `#`, пробел и
  Unicode не могут изменить request target; действующая percent-triplet
  validation transport не ослаблена. Типизированный `Message` хранит
  `MessageInfo` и `Vec<MessagePart>`: `MessageInfo` отдаёт identity
  (`id`/`role`/`parentID`/`sessionID`), assistant lifecycle
  (`time.completed` присутствует и не `null`; `finish`; truthy `error`),
  provider/model (`model()` требует непустые `providerID`/`modelID`, как
  reference `normalize_model`) и нормализованный token/cost `Usage`
  (`_number`-семантика: только JSON-числа, missing/bool/negative/non-finite →
  `0.0`); `MessagePart` отдаёт text-контент (`type`/`text`/`ignored`) и tool
  lifecycle (`tool`, `state.status`, `state.error`,
  `metadata.providerExecuted`, `state.metadata.interrupted`). `Message::text()`
  повторяет reference `_text_of` (text-части с falsy `ignored`, join через
  `\n`, фильтр пустых, Python `str.strip()` whitespace: Rust `is_whitespace`
  плюс U+001C..U+001F, которые `White_Space` не включает), а
  `Message::has_pending_tool_parts()` —
  reference `_has_tool_parts` (provider-executed и orphaned interrupted error
  tool уже разрешены). Parsing пермиссивен как reference `message.get(...)`
  только для скалярных полей (отсутствующее/неверно типизированное →
  `None`/`false`/zero) и для отсутствующих `info`/`parts` (reference defaults
  `{}`/`[]`). Намеренные fail-closed отклонения: present-но-неверно-
  типизированные lifecycle-контейнеры и элементы message/parts дают
  типизированный `MessageError::Malformed` — не-объектный элемент массива,
  не-объектный `info`, не-массив `parts`, не-объектная часть, truthy
  не-объектные `time`/`state`/`metadata`/`state.metadata` и truthy не-string
  `text` text-части (reference на них упал бы с `AttributeError`/`TypeError`),
  а невалидный JSON или не-массив top-level — тоже `Malformed`. Это
  гарантирует, что malformed структура не вызывает panic, не даёт ложного
  `completed`/успеха и не может спрятать более позднюю незавершённую запись за
  старой завершённой. Транспортные категории (timeout, unavailable, 401,
  404, прочие HTTP, protocol) сохраняются как
  `MessageError::Transport(TransportError)`; тело non-success ответа не
  читается. Redaction соблюдён: `Message`/`MessageInfo`/`MessagePart` рендерят
  только presence/lifecycle флаги и счётчики (не id, text, error, tool,
  provider/model), `Usage` — только accounting-числа, `MessageError` — только
  static label и вложенный `TransportError`; credential, `Authorization`,
  workspace, query и тела ответов не утекают. `Cargo.toml`/`Cargo.lock`, schema
  и fixtures не менялись (новых dependencies нет). Focused loopback tests (mock
  server с управляемым lifecycle, без внешней сети) покрывают точный
  method/path/scoped query/auth, reference-compatible user/assistant fixture,
  percent-encoding и fail-closed unusable id без отправки запроса, transport
  errors 401/404/500, malformed/non-array body, fail-closed malformed
  message/part/lifecycle (включая завершённый assistant с повреждёнными parts и
  malformed trailing entry), lifecycle/completion/error, normalization
  usage/model, Python-whitespace text extraction и tool-part lifecycle, а также
  redaction; существующие тесты 6.1–6.4 не регрессировали. Async prompt delivery
  (6.6), permissions/questions и worker/MCP wiring не входят.

### 6.6. Async prompt delivery

- **Завершено.** `bridge-opencode` добавляет к `OpenCodeClient` typed-операцию
  `send_prompt_async(session_id, message_id, text)`, повторяющую reference
  `opencode_client.py::send_prompt_async` (`POST /session/<id>/prompt_async`,
  scoped с workspace `directory`-query и существующим Basic auth/transport).
  Session id валидируется и percent-encode'ится тем же общим
  `encode_session_segment`/`encode_path_segment`, что и 6.4/6.5, а действующая
  percent-triplet validation transport не ослаблена: пустой id и голые
  `.`/`..` отклоняются до отправки как `PromptError::InvalidSessionId`. Тело
  запроса — ровно тот компактный UTF-8 JSON, который сериализует reference
  httpx (`ensure_ascii=False`, `separators=(",", ":")`):
  `{"messageID": <id>, "parts": [{"type": "text", "text": <text>}]}`, а
  `"model": {"providerID": ..., "modelID": ...}` добавляется последним полем
  только когда клиент построен из проекта с валидированным
  `ProjectEntry::opencode_model` (`OpenCodeClient::from_project`); клиент без
  модели не отправляет `model` вовсе, как reference
  `config.opencode_model is None`-ветка. `messageID` и `text` едут внутри JSON
  и экранируются сериализатором (UTF-8 без ASCII-escaping), а не подставляются
   в path. Успех — любой `2xx`, включая bodyless `204`; транспорт читает и
   фреймит тело успешного ответа по HTTP framing (кроме bodyless `204`) прежде
   чем вернуть `HttpResponse` — как reference `_request`, где httpx тоже читает
   тело, — но `send_prompt_async` не интерпретирует и не парсит его как JSON
   (reference `_request`, не `_json`), поэтому `PromptError` не имеет
   `Malformed`-варианта. Reference `_request` принимает любой статус `< 400`,
   включая `3xx`, тогда как существующий transport сохраняет политику только
   `2xx`; это намеренное отклонение для prompt delivery, и `3xx` возвращается
   как `TransportError::HttpStatus` (политика transport не ослабляется).
   Транспортные категории (timeout, unavailable, 401, 404, прочие HTTP,
   protocol) сохраняются как `PromptError::Transport(TransportError)`. Успешный
   вызов означает только
  принятие POST и намеренно **не** утверждает завершение assistant-хода;
  автоматического retry неидемпотентной отправки нет, а транспортная ошибка
  после записи запроса оставляет исход доставки неопределённым, а не
  гарантирует отсутствие отправки. Redaction соблюдён: `OpenCodeClient` не
  рендерит workspace и model, `PromptError` — только static label и вложенный
  `TransportError`; session/message id, text, model, body, credential и
  `Authorization` не утекают в `Debug`/`Display`. `Cargo.toml`/`Cargo.lock`,
  schema и fixtures не менялись (новых dependencies нет). Focused loopback
  tests (mock server с управляемым lifecycle `ServerCapture`, без внешней сети)
  покрывают точный method/path/query/auth/body без модели, добавление model из
  валидированного project entry последним полем, UTF-8 и JSON-escaping
   (`"`, `\`, newline, tab, не-ASCII без ASCII-escaping), приём bodyless `204`,
   игнорирование 2xx-тела, percent-encoding id, fail-closed unusable id и
   zero-timeout без отправки, transport errors 401/404/500, 3xx как
   `HttpStatus`, timeout и unavailable, а также неопределённый исход после
   полной отправки (mock получает и фиксирует POST body, затем не отвечает до
   истечения client deadline: ожидается `TransportError::Timeout`, ровно один
   POST, без retry), redaction; существующие тесты 6.1–6.5 не регрессировали.
  Permissions/questions (6.7+), worker/MCP/runtime wiring и worker recovery/
  status machine не входят.

### 6.7. Permissions list/reply

- **Завершено.** `bridge-opencode` расширяет `OpenCodeClient` двумя
  типизированными permission-операциями reference `opencode_client.py`, не
  меняя transport 6.1 и слои 6.2–6.6. `list_permissions()` отправляет
  `GET /permission` (reference `list_permissions`), `reply_permission(request_id,
  reply, message)` — `POST /permission/<id>/reply` (reference
  `reply_permission`); оба запроса scoped с workspace `directory`-query и
  аутентифицируются существующим Basic auth. Тело reply — ровно компактный
  UTF-8 JSON reference httpx: `{"reply": <reply>}`, а `"message"` добавляется
  последним полем только когда message передан; отсутствующий message (`None`)
  не отправляет поле, а пустой (`Some("")`) отправляет `"message": ""`, как
  reference `message is not None`. `reply` типизирован enum
  `PermissionReply::{Once,Always,Reject}` (`as_str` → `once`/`always`/`reject`),
  поэтому unsupported reply не может быть отправлен (reference бросает
  `OpenCodeError`). Request id валидируется и percent-encode'ится как один
  RFC 3986 path-сегмент общим с 6.4–6.6 `encode_session_segment`/
  `encode_path_segment`, а пустой id и голые `.`/`..` отклоняются до отправки
  как `PermissionError::InvalidRequestId`; literal `%`, `/`, `?`, `#`, пробел и
  Unicode не меняют request target, действующая percent-triplet validation
  transport не ослаблена. Это намеренное fail-closed hardening над reference,
  который подставляет id дословно. `reply_permission` следует reference
  `_request` (не `_json`): успех — любой `2xx`, включая bodyless `204`, тело
  успешного ответа читается и фреймится транспортом, но не интерпретируется как
  JSON, поэтому `PermissionError` не имеет `Malformed`-варианта для reply.
  Reference `_request` принимает любой статус `< 400`, включая `3xx`, тогда как
  существующий transport сохраняет политику только `2xx`; `3xx` возвращается
  как `TransportError::HttpStatus` (намеренное отклонение, политика transport не
  ослабляется). Автоматического retry нет, а транспортная ошибка после записи
  запроса оставляет исход reply неопределённым. Типизированный `Permission`
  сохраняет реальные reference поля `PermissionRequest` (OpenCode SDK):
  `id`/`sessionID`/`permission` (`Option<String>`), `patterns`/`always`
  (`Vec<String>`), `metadata` (raw JSON только за явным `Permission::metadata()`
  accessor, redacted в rendering) и optional `tool: {messageID, callID}`
  (`PermissionTool`), которые читают будущие permission consumers
  (`worker.py::_permission_decision`/`_pending_permissions`,
  `mcp_server.py::_blockers_present`), без реализации consumers. Parsing явно
  разграничивает required SDK-поля, absent/null optional defaults и malformed:
  `id`, `sessionID`, `permission` и `patterns` обязательны по SDK-shape, поэтому
  их отсутствие/`null`/неверный тип → `PermissionError::Malformed` (не `None` и
  не пустой список), чтобы повреждённый pending request нельзя было принять за
  отсутствующий (например, молча отфильтровать по `sessionID`) или разрешить;
  валидный пустой `patterns: []` остаётся допустимым и отличимым. Необязательные
  `always`/`metadata`/`tool` сохраняют reference defaults: absent/`null` →
  пусто/пустой объект/`None`. Present-но-неверно-типизированное значение,
  не-объектный элемент массива и не-массив top-level →
  `PermissionError::Malformed`. Поэтому повреждённый request не пропускается
  молча и список не может выглядеть пустым или разрешённым.
  Транспортные категории (timeout, unavailable, 401, 404, прочие HTTP,
  protocol) сохраняются как `PermissionError::Transport`. Redaction соблюдён:
  `Permission`/`PermissionTool` рендерят только presence-флаги и счётчики,
  `metadata` — `[redacted]`, `PermissionError` — static label и вложенный
  `TransportError`; credential, `Authorization`, workspace, id, sessionID,
  permission name, patterns, commands, metadata, tool ids, message и тела
  ответов не утекают в `Debug`/`Display`. `Cargo.toml`/`Cargo.lock`, schema и
  fixtures не менялись (новых dependencies нет). Focused loopback tests (mock
  server с управляемым lifecycle `ServerCapture` stop/join, без внешней сети)
  покрывают точный method/path/scoped query/auth для обоих endpoints,
  reference-compatible `PermissionRequest` fixture (с `metadata.directories` и
  `tool`), валидный пустой `patterns: []` и optional defaults, отсутствие/`null`
  required identity (`id`/`sessionID`/`permission`) и `patterns`, malformed
  request между двумя валидными (вся операция `Malformed`), malformed
  top-level/non-object element/полей, transport errors 401/404/500, unavailable
  и zero-timeout, каждый
  `once`/`always`/`reject`, optional message (отсутствующий vs пустой), compact
  UTF-8 body, percent-encoding и fail-closed unusable request id без отправки,
  bodyless `204` и игнорирование 2xx-тела, `3xx` как `HttpStatus` (намеренное
  отклонение), protocol error, а также неопределённый исход после полной
  отправки reply (mock получает и фиксирует POST body, затем не отвечает до
  истечения client deadline: `TransportError::Timeout`, ровно один POST, без
  retry) и redaction; существующие тесты 6.1–6.6 не регрессировали. Questions и
  blockers (6.8), automatic permission approval, worker/MCP/runtime wiring и
  worker recovery/status machine не входят.

### 6.8. Questions и blockers

- **Завершено.** `bridge-opencode` расширяет `OpenCodeClient` типизированной
  question-операцией reference `opencode_client.py` и минимальной
  session-scoped логикой определения blockers, не меняя transport 6.1 и слои
  6.2–6.7. `list_questions()` отправляет `GET /question` (reference
  `list_questions`) scoped с workspace `directory`-query и существующим Basic
  auth; успех — любой `2xx`, тело не-успешного статуса не читается, retry нет,
  транспортные категории (timeout, unavailable, 401, 404, прочие HTTP,
  protocol) сохраняются как `QuestionError::Transport`. Типизированный
  `Question` сохраняет реальные reference поля `QuestionRequest` (OpenCode SDK):
  `id`/`sessionID` (`Option<String>`), `questions` (`Vec<QuestionInfo>`) и
  optional `tool: {messageID, callID}` (`QuestionTool`); каждый `QuestionInfo`
  несёт `question`/`header` (`String`), `options` (`Vec<QuestionOption>` с
  `label`/`description`) и optional `multiple`/`custom` (`Option<bool>`).
  Parsing явно разграничивает required SDK-поля, absent/null optional defaults
  и malformed: `id`, `sessionID` и `questions` обязательны, поэтому их
  отсутствие/`null`/неверный тип, не-объектный элемент массива и не-массив
  top-level → `QuestionError::Malformed` (не `None` и не пустой список), чтобы
  повреждённый pending question нельзя было молча отфильтровать по `sessionID`
  или принять за отсутствующий; required поля `QuestionInfo`
  (`question`/`header`/`options`) и `QuestionOption` (`label`/`description`)
  проверяются так же, а optional `multiple`/`custom`/`tool` при absent/`null`
  дают `None`, при ином типе — `Malformed`. Валидный пустой `questions: []` и
  `options: []` остаются допустимыми и отличимыми от malformed. Reusable
  логика определения blockers — чистый `SessionBlockers::detect(&[Permission],
  &[Question], session_id)`, который повторяет reference presence-проверки
  `worker.py::_pending_permissions`/`_pending_questions` и
  `mcp_server.py::_blockers_present`: точное byte-for-byte сравнение `sessionID`
  не смешивает сессии, `Question::blocker()` повторяет reference
  `{"type": "question", "text": ...}` (первый `questions[].question`,
  усечённый до 300 Unicode code points, как Python `text[:300]`; пустой
  `questions` даёт пустой текст), а при успешно полученных списках наличие
  blockers эквивалентно `!SessionBlockers::detect(...).is_empty()` (reference
  `_blockers_present=True` означает наличие blocker, тогда как `is_empty()=True`
  — отсутствие). Хелпер не делает HTTP сам и не скрывает transport/malformed,
  поэтому
  потребитель сам выбирает reference-политику (worker глотает сбой, MCP
  fail-closed). Намеренные fail-closed hardening-отклонения над reference:
  reference `_pending_questions` при `OpenCodeError` возвращает `[]` и
  коэрсит `str(...)` над первым `question`; reference `_blockers_present` при
  ошибке возвращает `True`; reference фильтрует сырые dict без проверки типов
  `sessionID`. Здесь повреждённый ответ — `QuestionError::Malformed` (fail
  closed), а не пустой список; `list_questions` не реализует question reply
  API, automatic approval, worker state machine, MCP/runtime wiring,
  SQLite/schema и не добавляет dependencies. Redaction соблюдён:
  `Question`/`QuestionInfo`/`QuestionOption`/`QuestionTool`/`QuestionBlocker`
  рендерят только presence-флаги, счётчики и длину текста, `QuestionError` —
  static label и вложенный `TransportError`; credential, `Authorization`,
  workspace, id, sessionID, question/header, options, tool ids и тела ответов
  не утекают в `Debug`/`Display`. Focused loopback tests (mock server с
  управляемым lifecycle `ServerCapture` stop/join, без внешней сети) покрывают
  точный method/path/scoped query/auth, reference-compatible `QuestionRequest`
  fixture, валидные пустые `questions`/`options` и optional defaults,
  отсутствие/`null` required identity (`id`/`sessionID`) и `questions`,
  malformed request между двумя валидными (вся операция `Malformed`), malformed
  top-level/element/полей `QuestionInfo`/`QuestionOption`, transport errors
  401/404/500, unavailable и zero-timeout, session filtering
  permission/question blockers без смешения сессий, усечение текста до 300
  code points (включая multi-byte) и empty-`questions`, а также redaction;
  существующие тесты 6.1–6.7 не регрессировали (попутно снят
  `clippy::single_element_loop` в тесте 6.6, обнаруженный `--all-targets`).

## Поток 7. Worker

### 7.1. Worker argv и spawn

- **Завершено.** Новый узкий workspace-crate `bridge-worker` строит
  типизированный argv и запускает отдельный worker-процесс, останавливаясь до
  lock/startup grace (7.2), session resolution, worker state machine,
  MCP/runtime wiring и production CLI `worker` subcommand (production CLI `worker` пока не реализован). `WorkerInvocation` валидирует входы и рендерит reference-порядок
  ровно как `worker.py::worker_argv`: `<absolute-agent-bridge> worker --project
  ID --config PATH --state-root PATH --task TASK_ID --round N`. Executable,
  config path и state root обязаны быть абсолютными: production путь
  executable — `WorkerInvocation::from_current_exe` (`std::env::current_exe`,
  тот же Rust binary), а caller резолвит config/state против собственного cwd
  (reference config loader делает это через `Path.absolute()`); явный
  абсолютный executable injection допустим для коротких integration
  fixtures/будущего wiring. Абсолютные `--config`/`--state-root` не дают child,
  запущенному в workspace, интерпретировать их относительно другого cwd и
  ломать config resolution и state isolation. Аргументы — `OsString` и
  передаются напрямую в `Command::args`, поэтому пробелы и Unicode сохраняются
  byte-for-byte, shell/PATH-поиск/Python interpreter не используются;
  идентификаторы — domain `ProjectId`/`TaskId`, а round — положительный `u32`,
  что совпадает с persisted `round_number` `1..=u32::MAX` (`RoundRow`), без
  изобретения несовместимых правил. `spawn_worker` до любых FS side effects
  fail-closed проверяет state namespace: `invocation.state_root` и
  `invocation.project_id` должны совпадать с `RustStateLayout`, а layout должен
  быть уже инициализированным валидным Rust-owned state, что доказывает
  существующий `RustStateLayout::open` (marker, format_version, project
  namespace, normalized state root, `meta.runtime_owner` и schema v6). Foreign,
  missing или mismatched state (например Python state или каталог без Rust
  marker) отклоняется до создания каталога/chmod/log/spawn, поэтому чужой
  state не изменяется. Только затем создаётся приватный project state dir по
  `RustStateLayout` (mode `0o700` на Unix), append-ит stdout и stderr ребёнка в
  один Rust-owned `worker.log` (`RuntimeLog::Worker`), ставит cwd = workspace,
  stdin = `/dev/null` и на Unix — новую session и process group через safe
  `process-wrap::std::ProcessSession` (`setsid`, reference
  `start_new_session=True`), а не только process group; `unsafe_code=forbid`
  соблюдён. Новая минимальная dependency `process-wrap` (default-features=false,
  features `std`+`process-session`) обоснована отсутствием safe setsid API в
  std/`command-group`; Cargo.lock обновлён. Caller не блокируется до завершения
  worker, drop `SpawnedWorker` не убивает процесс, а `pid`/`try_wait`/`wait`/
  `kill` дают достаточный handle/outcome контракт. Python state не читается, не
  копируется и не инициализируется, runtime logs — только в Rust-owned
  namespace. Типизированный `WorkerError` (InvalidExecutable, InvalidWorkspace,
  InvalidConfigPath, InvalidStateRoot, InvalidRound, CurrentExecutable,
  StateRootMismatch, ProjectMismatch, StateOwnership, StateDirectory, LogFile,
  Spawn) рендерит в `Display`/`Debug` только static labels, не раскрывая
  executable, config/state paths, project/task id и round; I/O причина
  доступна лишь через `Error::source`. Focused tests покрывают точный argv,
  сохранение пробелов/Unicode без shell, отклонение relative
  executable/config/state/workspace, пустых paths, round 0, state root и project
  mismatch, `from_current_exe` с абсолютным argv[0], typed/redacted spawn
  failure, append (не truncate) `worker.log`, fail-closed ownership regressions
  (foreign marker, missing marker, state под другим root, project mismatch) с
  проверкой неизменности чужого каталога/marker/log и отсутствия child и
  meaningful integration spawn короткоживущего локального helper-бинарника:
  реальный `setsid` (pid == pgrp == sid, sid != parent session), workspace cwd,
  stdin EOF, 0700 state dir и stdout+stderr в логе, а bounded lifecycle tests с
  `try_wait`-deadline и kill/reap доказывают return-before-exit и контракт
  `try_wait`/`kill`; внешние сервисы/сеть не запускаются, после тестов
  процессов не остаётся. Lock/startup grace (7.2), session resolution/round
  execution, worker state machine, MCP wiring, auto-approval, verification
  integration и process ownership CLI 9.6 не входят; crate — проверенная
  reusable argv/spawn foundation, не готовый исполняющий задачи worker.

### 7.2. Lock и startup grace

- **Завершено.** `bridge-worker` расширен project-scoped non-blocking worker
  lock и reusable bounded startup grace; session resolution, round execution,
  worker state machine, MCP/runtime wiring, auto-approval, verification и
  production CLI `worker` по-прежнему не входят. `WorkerLock::try_acquire`
  открывает Rust-owned `worker.lock` по существующему
  `RustStateLayout::lock(RuntimeLock::Worker)` (`<root>/<project>/worker.lock`),
  `O_RDWR | O_CREAT` с mode `0o600` и берёт **эксклюзивный неблокирующий** BSD
  `flock` через safe `nix::fcntl::flock` (`LOCK_EX | LOCK_NB`), совместимый с
  reference `worker.py::worker_lock`/`worker_lock_is_free` (`fcntl.flock`).
  Результат типизирован: `WorkerLockOutcome::Acquired(WorkerLock)` и
  `WorkerLockOutcome::Busy`. RAII guard `WorkerLock` держит lock до drop (закрытие
  fd освобождает flock), а ядро освобождает lock и при завершении процесса,
  поэтому падение worker не оставляет stale held lock; разные проекты
  используют разные пути и не блокируют друг друга; наличие `worker.lock` не
  равно held lock (`WorkerLock::is_free`/`is_held` реально пробят состояние), а
  существующий файл не удаляется, не пересоздаётся и не усекается
  (`.truncate(false)`, inode и содержимое сохраняются). До любых действий с
  lock artifact layout fail-closed проверяется существующим production guard
  `RustStateLayout::open` (marker, format_version, project namespace,
  normalized state root, `meta.runtime_owner`, schema v6): missing/foreign/
  скопированный под другой root state возвращает
  `WorkerLockErrorKind::StateOwnership`, не создавая ни lock-файл, ни
  project dir, поэтому чужой (в том числе Python) state не изменяется. Новая
  минимальная dependency `nix` добавлена под `cfg(unix)` (переиспользование
  workspace dependency с feature `fs`), safe-wrapper сохраняет
  `unsafe_code=forbid`; Cargo.lock обновлён. `WorkerLockError` рендерит в
  `Display`/`Debug` только static labels, не раскрывая state root, project id,
  lock path и содержимое; I/O/storage причина доступна лишь через
  `Error::source`. Reusable startup grace — `StartupGrace` с явными
  монотонными `Instant` и grace (`DEFAULT_STARTUP_GRACE` = reference 10 секунд):
  `observe` возвращает `StartupObservation::Pending` (lock ни разу не был held и
  `elapsed <= grace`), `Held` (первое held-наблюдение закрывает startup
  window), `Released` (ранее виденный held lock затем свободен — worker
  finished) и `GraceExpired` (grace истёк, ни разу не увидев held lock).
  Истечение строго `elapsed > grace` (граница `elapsed == grace` ещё Pending);
  свободный lock сразу после spawn — это startup, а не завершение; повторный
  spawn (`restart`) сбрасывает часы и `seen_held`; `observe_lock` пробит
  `WorkerLock::is_held`. Settled statuses (`complete`/`awaiting_review`/
  `accepted`/`closed`) и `failed` при ещё удерживаемом lock обрабатываются
  вызывающим раньше startup tracking — это зафиксировано в документации узкого
  API без реализации MCP/state machine/автоспавна. Focused unit tests покрывают
  process-isolation (два проекта независимы, guard эксклюзивен и освобождается
  на drop), ownership refusal без side effects (missing/foreign/copied state не
  создают lock), mode 0600, сохранение inode/содержимого существующего файла,
  redaction и детерминированные boundary-тесты startup grace на явных
  `Instant` (граница `>` grace, free-right-after-spawn, held→released,
  bounded never-acquire, restart). Meaningful integration tests через
  короткоживущий локальный `worker_lock_fixture` проверяют реальное
  конкурирующее acquisition в отдельном процессе (`Busy`), release при
  kill/process exit и drop guard, независимость разных проектов,
  delayed acquisition (`Pending` → `Held`), bounded never-acquires и early-exit
  (никогда не `Released`) с короткими deadlines; родитель не пробит lock, пока
  fixture не опубликует атомарный `acquired`/`ready` evidence (bounded readiness
  handshake: `--ready` и parent→child `--wait-for`), поэтому transient
  exclusive probe не может сорвать единственное non-blocking acquisition, а
  процессные lock-тесты сериализованы, чтобы `fork` одного теста не наследовал
  held lock-fd другого. Все helper'ы завершаются и reaped, сеть/внешние сервисы
  не запускаются, после тестов процессов не остаётся. Регрессии 7.1 проходят.

### 7.3. Session resolution

- **Завершено (narrow foundation).** `bridge-worker` получил reusable
  `resolve_round_session(client: &OpenCodeClient, layout: &RustStateLayout,
  round: RoundRef) -> Result<ResolvedSession, SessionResolutionError>` поверх
  существующих production
  `OpenCodeClient::{list_sessions, create_session}` (scoped `directory` и
  существующий Basic auth) и атомарного
  `StorageConnection::bind_round_session(RoundRef, ...)`, без runtime
  orchestration. Resolver принимает проверенный `RustStateLayout` и открывает
  именно его через production ownership guard `RustStateLayout::open` (sidecar
  marker, implementation/format version, project namespace, normalized
  state-root, `meta.runtime_owner='rust'` и schema v6) **до** чтения task/round,
  HTTP и записи; `layout.project_id` сверяется с `round.project_id`. Поэтому
  schema-v6 unmarked/Python-owned DB, foreign/missing marker и Rust state,
  скопированный под другой root, отвергаются типизированным `StateOwnership`
  (project mismatch — `TaskMismatch`) без HTTP/write; нет unchecked
  `StorageConnection` injection-точки — единственное соединение создаёт сам
  guard. Детерминированный title — ровно
  `agent-bridge {task_id} round {round_number}` (`round_session_title`).
  Непустой persisted `rounds.session_id` выигрывает немедленно
  (`SessionResolutionSource::Existing`) без `list`/`create` HTTP и без
  rebinding; `tasks.session_id` и session прошлого round не используются.
  Иначе один `list_sessions`: transport/malformed — fail-closed
  `SessionUnknown` без создания session; совпадения по точному byte-for-byte
  title: `>1` — `SessionAmbiguous` (первое не выбирается, session не
  создаётся, даже если directory совпадает лишь у одного); ровно одно —
  adoption, id обязан быть непустым и начинаться с `ses` (без требования
  `ses_`), иначе `SessionUnknown`, а `directory` для adoption обязателен и
  должен resolved-совпадать с workspace, иначе `SessionDirectoryMismatch`,
  после чего атомарный bind и только затем resolved id. Без совпадений
  `create_session(title)` вызывается ровно один раз без parentID/fork/старой
  session; HTTP ошибка/неверный id — `SessionUnknown`; отсутствующий
  `directory` допустим, присутствующий обязан resolved-совпадать с
  workspace, иначе `SessionDirectoryMismatch`; до успешного возврата обязателен
  успешный atomic bind. До любых HTTP side effects task и current round
  читаются из Rust-owned storage (production `Task::from_row`/`RoundRow::from_row`)
  и проверяются task/round/project/current-round/workspace
  (`UnknownTask`/`TaskMismatch`/`StaleRound`/`WorkspaceMismatch`/`InvalidInput`),
  поэтому stale/mismatched input не создаёт session для чужой задачи. Storage
  failure — типизированный `Storage`, никогда не resolved success; созданная до
  сбоя binding session на следующем вызове adopt-ится по title, а не
  дублируется; create с неизвестной доставкой не retry-ится автоматически
  (повторный resolution начинается с `list_sessions`). Сравнение `directory`
  fail-closed: relative-путь, встроенный NUL, missing/non-resolvable путь и
  чужой tree (никакого lexical prefix) отвергаются, symlink-алиасы
  существующего workspace принимаются через canonical resolution; это
  документированное fail-closed отличие от reference `Path.resolve()`, чей
  результат зависел бы от cwd. Типы `bridge_opencode::Session`, сворачивающие
  non-string/отсутствующие id/title/directory в `None`, обрабатываются fail
  closed (non-string title не матчится, отсутствующий/непустой-non-`ses` id и
  adoption-directory отвергаются); для созданной session отсутствующий/null
  `directory` допустим совместимо с reference, non-string `directory`
  сворачивается typed parser в `None` и тоже допустим — документированное
  отличие от reference, где `Path(directory)` поднимает `TypeError`; string
  directory по-прежнему обязан resolved-совпадать с workspace; transport crate
  не менялся. Concurrency precondition задокументирован: caller держит project
  `WorkerLock`, взятый из того же `RustStateLayout`, на весь resolver.
  `ResolvedSession` (`id()`/`source()`: `Existing`/`Adopted`/`Created`) и
  `SessionResolutionError` редактированы и не раскрывают project/task/session
  ids, title, directory, HTTP body, credentials, SQL и пути; `Debug`/`Display`
  — только static labels, причина — лишь через `Error::source`. Focused
  loopback-mock HTTP + fresh Rust-owned SQLite на временных roots (без live
  OpenCode) покрывают persisted-session без HTTP/write, exact title, adoption
  без POST, multiple-match ambiguous, table-driven invalid/missing/empty/
  non-string matched и created session id, malformed create JSON/top-level,
  directory отсутствует для adoption vs create, non-string create directory как
  документированное отличие (Rust принимает, reference `TypeError`),
  mismatched directory без binding, symlink
  workspace alias, scoped GET/POST auth и body без parentID, persisted
  round/task pointers до успешного возврата, повторный resolution без
  дубликата, created-but-not-bound orphan recovery, storage rejection,
  transport/malformed fail-closed без unintended POST/retry, отсутствие
  fallback `tasks.session_id` и reuse между rounds и redaction, а также
  ownership-guard rejection: unmarked schema-v6 DB, foreign/missing marker,
  mismatch `layout.project_id`/`round.project_id` и Rust state, скопированный
  под другой root, — все до HTTP и без изменения foreign DB/marker bytes и без
  bindings. Регрессии
  7.1/7.2, handshake и serialization process tests сохранены. Initial/revision
  prompt, `prompt_async`, outbound ids, наблюдение завершения, auto-approval,
  worker state machine/CLI/MCP wiring, полная CI matrix, production services и
  внешние writes не входят. `Cargo.toml`/`Cargo.lock` изменились добавлением у
  `bridge-worker` dependency `bridge-opencode` и `rusqlite` (чтение persisted
  round через production mapping), dev-dependencies `bridge-config` и
  `serde_json`; bridge-storage/schema/fixtures не менялись.

### 7.4. Initial prompt happy path

- **Завершено (narrow foundation).** `bridge-worker` получил pure prompt builder
  `initial_prompt(task: &Task, workspace: &Path) -> String` и reusable production
  `dispatch_initial_round(client: &OpenCodeClient, layout: &RustStateLayout,
  round: RoundRef) -> Result<DispatchedRound, DispatchError>` поверх 7.3 resolver,
  существующего `OpenCodeClient::send_prompt_async` и атомарных
  `StorageConnection::{prepare_round, mark_round_sent, mark_round_observing}`.
  Dispatch до любых HTTP/write открывает переданный layout через production
  ownership guard `RustStateLayout::open` (sidecar marker, implementation/format
  version, project namespace, normalized state-root, `meta.runtime_owner='rust'`,
  schema v6) и проверяет `layout.project_id == round.project_id`, существование
  task и его project, согласованность task/round/project, current
  (highest-numbered) round и resolved-совпадение task workspace с workspace
  клиента. Task обязан быть `implementing` и без pending `close_requested_at`
  (`TaskNotDispatchable`), `implement`-round обязателен
  (`RevisionNotSupported`), round должен быть `pending` и `attempted == false`
  (`RoundNotDispatchable`), поэтому stale/mismatched/foreign/revision/
  non-pending/repeated/closed/close-requested вход fail-closed отвергается до
  side effects. Затем 7.3 `resolve_round_session` резолвит и атомарно bind-ит
  ровно одну session (list/create — единственный HTTP до prompt). Так как
  resolution делает HTTP и binding-write, task/round перепроверяются сразу после
  него теми же условиями до `prepare`/`mark_sent`/`send`, поэтому close request
  или мутация task/round во время resolution наблюдаются и не доходят до prompt
  POST. Persisted task перечитывается, prompt рендерится byte-for-byte по
  reference `prompts.INSTRUCTION_TEMPLATE`: `allowed_paths` через `", "`,
  `test_commands` через `"; "`, `<none>` для пустых, baseline из
  `snapshot.dirty_paths` + `external_repositories[].dirty_paths` (root без
  trailing `/`), permissive/strict git rule по `snapshot.allow_commit is True`;
  non-string scalar рендерится как Python `str`, JSON-контейнер в path-поле
  пропускается (документированное fail-closed сужение над `str(container)`), а
  подстановка идёт одним `format!`-проходом без повторного раскрытия. Outbound id
  — reference `"msg_" + uuid.uuid4().hex` (`msg_` + 32 lowercase hex),
  persisted через `prepare_round`; уже persisted id переиспользуется без второго
  prepare. `mark_round_sent` фиксирует `sent`/`attempted=1` **до** единственного
  `send_prompt_async` (persist-before-send), а после успешного `2xx`
  `mark_round_observing` переводит round в `observing`; task остаётся
  `implementing`, observation loop отсутствует. Ошибка storage/HTTP —
  типизированный redacted `DispatchError`, никогда не ложный success; при сбое
  доставки round остаётся `sent`/`attempted` с persisted outbound id (delivery
  outcome undefined), не retry-ится автоматически, а повторный dispatch
  отвергается `RoundNotDispatchable` до POST. `DispatchError`
  (`InvalidInput`/`UnknownTask`/`TaskMismatch`/`WorkspaceMismatch`/`StaleRound`/
  `StateOwnership`/`RevisionNotSupported`/`RoundNotDispatchable`/
  `TaskNotDispatchable`/`Session`/`Storage`/`Delivery`) и `DispatchedRound` в
  `Display`/`Debug` не раскрывают prompt, task text, project/task/session/message
  ids, workspace, HTTP body, SQL и пути; причина — только через `Error::source`.
  Concurrency precondition задокументирован: caller держит project `WorkerLock`
  (из того же layout) на весь dispatch; lock сериализует worker'ов одного
  проекта, но намеренно **не** берётся cooperative-close writer'ами
  `request_task_close`/`complete_requested_close` (будущий MCP close path) и
  внешними писателями того же SQLite. Задокументирован remaining race: close
  request в окне между повторной проверкой и `mark_round_sent` здесь не
  детектируется, его отказ — cooperative-close lifecycle 7.12, который в этот
  шаг не входит. Focused loopback-mock HTTP + fresh Rust-owned SQLite на
  временных roots (без live OpenCode и внешних writes) покрывают exact
  prompt/body, outbound id/body, persistence `sent`/`attempted` до prompt POST
  (наблюдение из mock handler'а через отдельное соединение), session binding,
  successful `observing` lifecycle, scoped auth/query, отсутствие лишних POST,
  reuse persisted session и prepared outbound id, fail-before-send для
  invalid/stale/foreign/revision/non-pending/repeated входа, fail-before-HTTP
  для closed task (`request_task_close` + `complete_requested_close`),
  существующего pending close и таблицы non-`implementing` task statuses без
  binding/outbound/attempt мутаций, close request из mock session handler'а во
  время resolution без prompt POST, typed delivery failure без retry,
  session-resolution failure без prompt POST, ownership-guard rejection (unmarked
  schema-v6) без изменения foreign DB/marker, model-поле последним и redaction.
  Регрессии 7.1/7.2/7.3 сохранены. Completion
  observation, permission/question blockers, auto
  approval, failed/delivery-unknown (7.9), continuation recovery (7.10),
  verification integration (7.11), cooperative close (7.12), worker state
  machine/CLI/MCP wiring и production services не входят. `Cargo.toml`/`Cargo.lock`
  изменились добавлением `uuid` feature `v4` (и `getrandom`/`r-efi`) и переводом
  `serde_json` в обычные dependencies `bridge-worker` (нужен prompt builder'у);
  bridge-opencode/transport, bridge-storage/schema/fixtures не менялись.

### 7.5. Revision round

- **Завершено (narrow foundation).** `bridge-worker` получил pure revision
  prompt builder `revision_prompt(task: &Task, workspace: &Path, findings: &str,
  round_number: u32) -> String` и reusable production
  `dispatch_revision_round(client: &OpenCodeClient, layout: &RustStateLayout,
  round: RoundRef) -> Result<DispatchedRound, DispatchError>` поверх 7.3 resolver,
  существующего `OpenCodeClient::send_prompt_async` и атомарных
  `StorageConnection::{prepare_round, mark_round_sent, mark_round_observing}`.
  Initial и revision пути разделяют один внутренний pipeline (ownership guard,
  project/task/round/current/workspace проверки, session resolution,
  повторная проверка, reuse prepared outbound id, persist-before-send,
  `observing`-переход), а различаются только ожидаемым `RoundKind`/`TaskStatus`
  и prompt builder'ом; публичный API 7.4 и его ошибки/поведение сохранены.
  `revision_prompt` рендерит byte-for-byte reference
  `prompts.REVISION_TEMPLATE`: task id, workspace, `раунд {round_number}`,
  `allowed_paths` через `", "`, `test_commands` через `"; "`, `<none>` для
  пустых, те же baseline/git rules, исходная задача и findings; подстановка —
  один `format!`-проход, поэтому placeholder-looking текст в task/findings не
  раскрывается повторно. Findings берутся только из persisted current
  `rounds.findings` (`None -> ""`, как reference `round_obj.findings or ""`), не
  из response/result/blocker полей и не из caller-supplied текста.
  `dispatch_revision_round` до любых HTTP/write открывает переданный layout через
  production ownership guard `RustStateLayout::open` и требует
  `round.kind == Revise`, `task.status == Revising`, отсутствие
  `close_requested_at`, current (highest-numbered) `pending` unattempted round,
  согласованность layout/project/task/round и resolved workspace; initial
  dispatch по-прежнему требует `Implement`/`Implementing` и отвергает revision
  (`RevisionNotSupported`), а revision dispatch отвергает implement
  (`ImplementNotSupported`), неверный task.status/close-request —
  `TaskNotDispatchable`, non-pending/attempted — `RoundNotDispatchable`,
  stale/foreign/workspace/ownership — соответствующие типизированные категории,
  все до side effects. 7.3 resolver даёт каждому round собственную независимую
  session по детерминированному title текущего round без parentID/fork;
  `create_revision_round` уже очистил `tasks.session_id`, поэтому session
  прошлого round не переиспользуется. Так как resolution делает HTTP и
  binding-write, task/round перепроверяются теми же условиями сразу после него
  до `prepare`/`mark_sent`/`send`, поэтому close request или мутация во время
  resolution не доходят до prompt POST. Prepared outbound id переиспользуется
  без второго prepare; новый id — reference `"msg_" + uuid.uuid4().hex`
  (`msg_` + 32 lowercase hex); `mark_round_sent` фиксирует `sent`/`attempted=1`
  **до** единственного `send_prompt_async`, а после успеха `mark_round_observing`
  переводит round в `observing` (task остаётся `revising`, observation loop
  отсутствует). Ошибка storage/HTTP — типизированный redacted `DispatchError`,
  не ложный success; сбой доставки оставляет round `sent`/`attempted` с
  persisted outbound id (delivery outcome undefined) и не retry-ится
  автоматически, а повторный revision dispatch отвергается до POST.
  `DispatchError`/`DispatchedRound` в `Display`/`Debug` не раскрывают prompt,
  findings, task text, project/task/session/message ids, workspace, HTTP body,
  SQL и пути; причина — только через `Error::source`. Concurrency precondition
  тот же: caller держит project `WorkerLock` (из того же layout) на весь
  dispatch; lock намеренно **не** берётся cooperative-close writer'ами
  `request_task_close`/`complete_requested_close` и внешними писателями, поэтому
  задокументирован remaining close race между повторной проверкой и
  `mark_round_sent`, отказ которого — cooperative-close lifecycle 7.12 (в этот
  шаг не входит). Focused loopback-mock HTTP + fresh Rust-owned SQLite тесты
  покрывают законченный `implement round -> create_revision_round ->
  dispatch_revision_round`: exact prompt/body и persisted findings, dedicated
  new session без использования previous round session/title, persistence
  `sent`/`attempted`/outbound до prompt POST, task `revising`/round `observing`,
  None findings как пустая секция, prepared outbound reuse, повторный revision
  dispatch без HTTP, cross-kind `ImplementNotSupported`/`RevisionNotSupported`,
  таблицу non-`revising` task statuses, pending close и close request во время
  session resolution без prompt POST, stale/foreign/unknown/zero/workspace и
  ownership-guard ошибки до side effects, typed delivery failure без retry,
  session-resolution failure без prompt POST, unmarked state без изменения
   foreign DB/marker и redaction. Dispatch-level storage failures покрыты через
   временный test-only SQLite trigger на свежей Rust-owned DB (production
   schema/код не меняются): сбой `prepare_round` — typed `Storage`, ноль prompt
   POST, `attempted=0` и outbound не записан; сбой `mark_round_sent` после
   успешного prepare — typed `Storage`, ноль prompt POST, сохранён outbound,
   round остаётся `pending`/`attempted=0`; сбой `mark_round_observing` после
   успешного prompt POST — typed `Storage` вместо ложного success, ровно один
   POST, persisted `sent`/`attempted=1`/outbound и повторный dispatch без
   второго POST. Регрессии 7.1–7.4 сохранены. Completion
  observation, permission/question blockers (7.6/7.7), auto-approval,
  failed/delivery-unknown (7.9), continuation recovery (7.10), verification
  integration (7.11), cooperative close (7.12), worker state
  machine/CLI/MCP wiring и production services не входят.
  `Cargo.toml`/`Cargo.lock`, bridge-opencode/transport,
  bridge-storage/schema/fixtures не менялись.

### 7.6. Permission blocker

- **Завершено (narrow foundation).** `bridge-worker` получил reusable
  `handle_permission_blocker(client: &OpenCodeClient, layout: &RustStateLayout,
  round: RoundRef) -> Result<PermissionBlockerOutcome, PermissionBlockerError>`
  поверх существующего `OpenCodeClient::list_permissions` (scoped `directory` и
  Basic auth), pure `round_session_title` и атомарного
  `StorageConnection::finish_round`. Модуль `src/permission.rs` до любых HTTP/
  write открывает переданный layout через production ownership guard
  `RustStateLayout::open` (sidecar marker, implementation/format version,
  project namespace, normalized state-root, `meta.runtime_owner='rust'`, schema
  v6) и сверяет `layout.project_id == round.project_id`; затем проверяет
  существование task, принадлежность project/task/current round, resolved
  совпадение task workspace с workspace клиента и отсутствие pending
  `close_requested_at` (`CloseRequested`), поэтому unmarked/foreign/Python state,
  stale/чужой round, workspace mismatch и pending cooperative close
  отвергаются до HTTP и без записей. Round обязан быть `observing` (write path)
  либо уже `needs_user` (idempotent replay), а task — `implementing`/`revising`
  (write) либо `needs_user` (replay), иначе
  `RoundNotObservable`/`TaskNotObservable` до HTTP. Текущая session — только
  persisted `rounds.session_id` текущего round (не `tasks.session_id` и не
  session прошлого round); отсутствующая/пустая даёт `SessionUnknown`.
  `list_permissions` вызывается ровно один раз, и permissions фильтруются по
  точному byte-for-byte текущему `sessionID` (`Permission::belongs_to_session`),
  поэтому чужая session не блокирует, а пустой/foreign-only список — typed
  `PermissionBlockerOutcome::NoBlocker` no-op без записей. Cooperative close
  writers не берут `WorkerLock`, поэтому после HTTP round-trip persisted
  task/round/session/status/pending close читаются повторно (`inspect_round_state`)
  до любого outcome: close, пришедший во время GET, даёт `CloseRequested`, а
  write/replay решение и возвращаемые round/task берутся только из свежего
  состояния, не из pre-HTTP snapshot. Релевантный permission ровно один раз
  сохраняет `needs_user` через `finish_round`
  (`round_status=NeedsUser`, `task_status=NeedsUser`, `error_code="needs_user"`,
  `result_json={"blockers":[{"type":"permission","permission","patterns",
  "reason":"auto_approval_disabled"}]}`); committed task из `finish_round`
  авторитетен: если close попал в последнее окно перед atomic write и task реально
  `closed`/несёт `close_requested_at`, вызов возвращает `CloseRequested`, а не
  ложный `Blocked`/`UserAction`. Успешный вызов возвращает типизированные
  `PermissionBlocker`/`UserAction`: `open_project_console`, message, session_id,
  детерминированный `session_title`, instructions, а также `command`/
  `fallback_command` (типизированные поля, `None` в этом foundation, так как
  ready-to-run `console`/`attach-opencode` builders принадлежат runtime CLI
  stream 9). Повтор при уже `needs_user` round выполняет ту же фильтрацию, но
  **не пишет**: второй `needs_user` event и повторный сдвиг `updated_at`
  невозможны. Автоматическое разрешение запрещено: `reply_permission` и
  auto-approval не вызываются, permission POST отсутствует, внешний доступ не
  открывается. Fail-closed отличие от reference `worker.py::_pending_permissions`
  (который глотает OpenCode error): transport/malformed permission-list
  завершается типизированным `Permissions`, а storage failure —
  `Storage`, никогда не ложным success. Типы
  `PermissionBlockerOutcome`/`PermissionBlocker`/`UserAction`/
  `PendingPermission`/`PermissionBlockerError` в `Display`/`Debug` не раскрывают
  project/task/session ids, title, permission name, patterns, HTTP body, SQL,
  credentials и пути; причина — только через `Error::source`. Concurrency
  precondition задокументирован: caller держит project `WorkerLock` из того же
  layout на весь вызов. Focused loopback-mock HTTP + fresh Rust-owned SQLite
  тесты покрывают session filtering (текущая + чужая session), persisted
  `needs_user`/`user_action`/`result_json`, пустой и foreign-only no-op,
  idempotent repeat без duplicate lifecycle side effects, transport/malformed
  typed errors, ownership guard до HTTP, отсутствующую session, pending close,
  non-observing round, project mismatch и zero round без HTTP, отсутствие
  permission POST и redaction. Дополнительно deterministic loopback тесты
  сохраняют `request_task_close` во время GET (write и replay пути): ложный
  `Blocked`/`UserAction` не возвращается, close semantics и отсутствие duplicate
  `needs_user` events соблюдены; отдельный trigger-тест закрывает task прямо во
  время `finish_round`, и outcome guard возвращает `CloseRequested`; test-only
  trigger, отвергающий `needs_user` lifecycle/event write, доказывает typed
  `Storage` без false success и полный atomic rollback round/task/result/event без
  permission POST (production storage/schema не менялись). Регрессии 7.1–7.5
  сохранены. Question blocker
  (7.7), auto-approval (7.8), failed/delivery-unknown (7.9), continuation
  recovery (7.10), verification integration (7.11), cooperative close (7.12),
  observation loop/state machine, CLI/MCP/runtime wiring и production services не
  входят. `Cargo.toml`/`Cargo.lock`, bridge-opencode/transport,
  bridge-storage/schema/fixtures не менялись.

### 7.7. Question blocker (service завершён)

- **Минимальные prerequisites:** только согласованный refresh **0A** (manifest
  и fixtures). 7.7 не требует B2/runtime/worktree: это исторический шаг
  foundation v6, продолжающий 7.1–7.6, и он не блокируется новыми v7–v15
  задачами.
- **Результат:** `bridge-worker::question::handle_question_blocker` использует
  общие ownership/current-round/session/close guards с permission service,
  фильтрует точную session, сохраняет typed question blockers (первые 300 Unicode
  characters) и user action через atomic finish. Empty/foreign-only — no-op;
  повтор needs_user не пишет events/updated_at; malformed HTTP/storage failure
  fail closed. Close during GET/replay и committed close вместо blocker учтены.
  Ни reply, ни automatic answer, ни новый prompt/session не вызываются.
  6 loopback tests и clippy; это historical direct-workspace blocker service,
  worktree observer/FSM и CLI/MCP adapters остаются следующими consumers.

### 7.8. Auto-approval integration

- **7.8a. Worker state-directory permissions (service завершён).**
  `_external_permission_roots` добавляет opt-in state_root только к permission
  checks, не к external Git/linked roots. Literal/glob/symlink/traversal
  confinement сохраняется; default false — прежнее поведение. Source:
  `worker.py:868-916` (v17). Checks: `tests/test_worker.py` state approval
  cases. Depends on 2.13, 6.7, 0B.2.
  Реализованы pure `permission_decision` и
  `handle_permission_blocker_with_auto_approval`: opt-in state_root только
  permission roots, raw malformed decisions fail closed, bash использует
  существующий command-policy. Literal/glob confinement сверяет lexical и
  resolved roots, каждый direct glob child (включая symlink/dangling/loop),
  traversal/unsafe pattern/root rejects. Успешные ответы всегда Once и
  дедуплицируются в buffer одного state namespace/endpoint/round/session;
  foreign sessions игнорируются.
  Неодобренные и failed replies сохраняют needs_user/reason; mixed success
  evidence записывается как auto_approved. HTTP/storage/close/session checks
  не дают stale outcomes; successful cache переживает storage rollback.
  Проверены все 42 frozen worker delta cases, ordinary/bash policy и 7 новых
  local HTTP scenarios (duplicate/foreign/default/mixed/failure/rollback/
  close/session/namespace/endpoint races). Default historical no-reply entry point сохранён.
  Это service для saved direct round; worktree observer/full worker FSM и
  production CLI/MCP wiring остаются отдельными consumers.
- **7.8b. Controller permissions (HTTP launch wiring завершён; stdio ждёт 9.5).** Generated OpenCode config:
  external_directory `*→ask`, `<state_root>/*→allow` при opt-in; edit/task deny
  и bash ask сохраняются. Source: `opencode_launcher.controller_agent_permission`
  (v17). Checks: `tests/test_launchers.py`; unsupported pattern fail closed.
  Depends on 2.13, 0B.2, 9.13.
  `bridge-runtime::controller_permissions::controller_agent_permission`
  генерирует точные hard denies/ask и opt-in external_directory scope, не
  создавая state и не расширяя trusted external Git roots. Общий generator
  state pattern повторно проверяется при вызове; filesystem root, wildcards,
  relative/unrepresentable root fail closed. CONTROLLER_SUBAGENT_DEPTH=0
  сохранён отдельным security constant для launch consumer. Шесть frozen
  controller cases и дополнительные default/normalization/no-creation cases
  пройдены; workspace all-targets clippy без предупреждений. Полный generated
  config и foreground HTTP launch реализованы service 9.13a; CLI consumer
  подключён в 9.13b; stdio transport остаётся в 9.13c.

### 7.9. Failed/delivery_unknown

- **7.9a. Guarded message observer (service завершён).** `RoundObserver`
  привязан к execution context, owned layout, current round/session/outbound;
  root/current/close guards до и после единственного GET. Наблюдает `sent`
  после ambiguous POST без resend; delivery evidence проверяется до deadline,
  grace/deadline используют строгий `>`. Подтверждённая доставка запоминается,
  последний полный message history сохраняет usage/model при transport failure.
  Assistant error без later TUI continuation -> failed; неизвестная доставка
  -> delivery_unknown; deadline -> needs_user с caller-supplied pending blockers.
  Final predicate учитывает completed/finish/tool lifecycle и возвращает только
  candidate, без awaiting_review до verifier. Error diagnostics фиксированные,
  raw HTTP/assistant/tool errors не публикуются. Checks: loopback HTTP/SQLite
  boundary, TUI continuation, no-resend, close during GET, stale session и
  decreasing clock. Caller держит worker fences, доказывает session identity
  и управляет permission/question grace; full FSM остаётся открыт.

- **7.9b. Combined blocker grace (service завершён).**
  `RoundObserver::poll_with_blockers` сначала наблюдает messages/final/errors и
  deadline, затем permission/question lists только при видимом outbound history.
  Exact session filtering; shared stale window сбрасывается при исчезновении всех
  blockers, boundary `>= grace` сохраняет needs_user, final выигрывает до grace.
  Configured permissions отвечаются Once; successful reply кешируется до
  post-HTTP guard. Questions никогда не получают answer. Failed reply становится
  blocker; list transport/malformed failures игнорируются как в frozen worker,
  last pending blockers используются при deadline. Main/external change collection
  теперь выполняется и для failed/delivery_unknown/needs_user, без verifier;
  immutable baseline и close guards повторяются перед terminal publication.
  28 observer tests прошли: stale исчезновение/final, combined permission+question,
  foreign filtering, grace/deadline boundary, accounting/external changes,
  Once deduplication, failed reply и close во время GET. Workspace Clippy прошёл.
  Remote session identity, sleep loop, cooperative cleanup и CLI остаются consumers.

- **7.9c. Identity-checked observation loop (service завершён).**
  `poll_verified` проверяет unscoped GET /path и saved GET /session до messages.
  Workspace probe errors -> failed/workspace_mismatch как в reference; session
  404 -> failed/session_not_found. Transient/malformed session failures повторяются
  до strict `> deadline`, затем needs_user/transient_error без delivery_unknown.
  Повторный session GET не повторяет успешный workspace probe; HTTP и retries
  расходуют единый monotonic budget. Session id/directory должны присутствовать
  и совпадать: fail-closed отличие от Python, допускавшего absent directory.
  Diagnostics фиксированные, remote body/path/credentials не публикуются.
  `observe_to_completion` объединяет verified polling, stale grace и saved
  verifier/collection/publication, удерживая borrowed worker fences. Sleep
  разбит на <=100ms с повторным close/current guard. Terminal identity failure
  переводит sent -> observing перед guarded finish без prompt POST.
  34 observer tests прошли, включая wrong/missing remote roots/session ids,
  404, transient retry/deadline/recovery, close во время обоих identity GET и
  running -> final -> verifier -> awaiting_review loop, close during long sleep
  и zero cadence refusal; workspace Clippy прошёл.
  Итоговый regression: `cargo test --workspace --all-targets --offline` —
  1235 passed; all-target Clippy и fmt check прошли. CLI startup/dispatch/
  worker-start settings и cooperative cleanup остаются открыты.

- **7.9d. Worker runner/startup (service завершён).**
  `runner::run_worker` читает только initialized Rust-owned state, проверяет
  project/workspace/current round и не активирует waiting/parked задачи.
  Admission/task/project fences удерживаются через worker-start write, root
  preparation, session identity до prompt, dispatch и observer/verifier.
  Attempted resume использует saved context; ambiguous POST начинает observation
  без resend. Deadline включает pre-send session retries и dispatch HTTP.
  Defaults и пять AB_* переменных соответствуют frozen worker; malformed strings
  используют defaults, non-finite/negative/zero required durations fail closed.
  Narrow atomic `finish_worker_pre_send` разрешает только фиксированные
  workspace/session errors и blockers, сохраняет checkpoint и zero usage без
  фиктивных outbound id/attempted flags. Generic round transitions не расширены.
  Explicit recovery unsent needs_user -> pending; attempted -> observing;
  failed spawn rollback восстанавливает parked status. Revision dispatch передаёт
  trusted external roots; saved worktree revision допускает executor commits,
  сохраняя baseline/runtime. Infrastructure/metadata/storage failures остаются
  typed refusals с сохранённым state; full source error-policy parity/live provider
  smoke и MCP adapters ещё впереди. CLI consumer подключается отдельным этапом.

### 7.10. Continuation recovery

- **7.10d / 8.11b. Delivery-unknown observation recovery (завершён).**
  Startup и task_status проверяют saved endpoint/root, затем admission/task
  fences и atomic delivery claim переводят attempted delivery_unknown в
  implementing/revising + observing. Outbound/session/baseline не меняются;
  prompt не отправляется. Dead/unhealthy server оставляет прежний статус.
  Failed spawn делает exact-claim rollback с исходным error_code; общая event
  identity защищает lease от stale release другого recovery kind. Checks:
  storage claim/identity/rollback и concurrent startup/status loopback test;
  MCP regressions и targeted Clippy.

- **7.10a. Recovery spawn lease/probes (service завершён).** Task+project lock
  probes сериализуются внутри процесса, исключая ложный busy от двух probe;
  startup grace закрывает spawn-before-worker-lock window, expired lease
  допускает crash recovery. Source: `mcp_server._task_worker_running`,
  `_spawn_lease_pending`, `_maybe_spawn` (v17). Checks: concurrent probes,
  lease expiry и mark_worker_started overwrite. Depends on 3.15, 7.2.
  Реализованы `task_worker_running` и общая process-local probe serialization
  (включая historical project probes); обе worker fences проверяются независимо
  от config. `spawn_lease_pending` читает ISO UTC/naive/offset timestamp,
  использует строгий `< grace`, bounded expiry и malformed/missing no-lease.
  `StartupGrace::observe_task_lock` связывает старый monotonic tracker с B2.
  Проверки: 200 concurrent idle probes, real task/project holders, timestamp
  goldens/leap/offset/fraction/expiry, cross-process lock suite; 3.15 отдельно
  доказывает worker-start overwrite и stale claim release. Spawn/session/MCP
  orchestration остаётся в 7.10b/8.20.

- **7.10b. Resume existing session (service завершён).** Explicit needs_user
  recovery проверяет endpoint identity, claim/spawn/release; observation
  delivered round без повторного prompt. Background/review actions сохраняют
  blocker gate. Source: `mcp_server._maybe_spawn` (v17).
  Checks: spawn failure, live/dead server, stale/real blockers/no-resend.
  Depends on 7.10a, 6.2, 6.6. Opens 8.20.
  `bridge-worker::recovery::recover_needs_user` проверяет saved direct/worktree
  endpoint+root, background permission/question blockers, затем повторно
  проверяет current round/session/endpoint/close под admission lock, берёт
  atomic claim и вызывает injected spawn после release admission fence.
  Failed spawn делает exact-claim rollback. Нет prompt/session-create/reply/
  question-answer; повтор/конкуренция не дублируют spawn. Worktree lifecycle
  gate применяется до HTTP, static project endpoint не используется для worktree.
  5 local HTTP/subprocess tests: same-round/no-resend, background/explicit
  blockers, foreign session, failure rollback, dead/unhealthy/wrong-root
  endpoint, concurrent single spawn и close during GET /path; clippy passed.
  Полный observer FSM и public task_status adapter остаются в worker/MCP потоках.


- **7.10c. Reconstruct attempted execution (service завершён).**
  `resume_round_execution`/`resume_fenced_round_execution` восстанавливают
  current sent/observing/needs_user/delivery_unknown round из saved session,
  outbound, snapshot/scope/profile. Task должен быть implementing/revising:
  parked tasks сначала проходят explicit recovery claim; attach не активирует их.
  Нет HTTP, новых sessions/checkouts/runtime, prompt/resend или baseline refresh.
  Worktree attach требует created checkout и exact saved port/endpoint/runtime,
  допускает executor commits с исходным frozen baseline. Эти endpoint guards
  повторяются у downstream execution consumers. Checks: 23 observer tests,
  включая four open round states и no-resend, invalid identifiers/close/terminal;
  real runtime fixture after checkout commit доказывает unchanged process record,
  baseline и saved port, rejects rebound endpoint. Workspace Clippy прошёл.
  Remote identity/session probes и observation loop подключены в 7.9c;
  CLI startup/dispatch и cooperative cleanup остаются следующими consumers.

### 7.11. Verification integration

- **7.11a. Final candidate publication (single-repository service завершён).**
  `RoundObserver::publish_final` принимает только candidate того же observer,
  проверяет binding/close до verifier, после него и перед atomic finish.
  Observer теперь заимствует `FencedRoundExecution`: task/project fences
  удерживаются на всём пути observation/verifier/collection/publication.
  Только saved test_commands и persist-once verifier; failed/unsafe/timed_out
  outcomes и tool_errors сохраняются для review, без automatic acceptance.
  Изменения собираются после verifier, result включает repository-relative
  changed/committed/scope/policy/head, repositories и task_changed_paths,
  response/message id, verification, usage/model; checkpoint/status/event
  публикуются atomic finish. Collection failure допускает retry с уже сохранённым
  verifier; non-UTF8 paths fail closed. External snapshots/absolute scope
  отклоняются до запуска verifier, а не пропускаются в review result.
  Checks: 16 HTTP/Git/SQLite/subprocess scenarios совместно с 7.9a, включая
  foreign candidate, close before/during verifier, actual timeout, lock lifetime,
  failed/unsafe review и persisted verifier reuse после collection failure.
  Full observer FSM и multi-repository publication остаются открыты.

- **7.11b. Persisted multi-repository verifier (service завершён).**
  Domain `Verification.repositories` сохраняет root, before/after и relative
  side_effects внешних репозиториев; historical single-repository JSON неизменён.
  `run_round_verification_persisted_with_repositories` fingerprint-ит main и
  canonical sorted/deduplicated external roots до единственного запуска checks,
  затем после него; caller доказывает trusted roots и удерживает worker fences.
  External before failure предотвращает запуск всех команд; after failure
  сохраняет main fingerprints и уже собранные external entries с error outcome,
  без ложного clean side_effects. Общие side_effects квалифицированы external
  root, persisted reuse не теряет repositories и не повторяет команды.
  Checks: real temporary Git repositories, identical path names, ordering,
  storage roundtrip/reuse, before/after failure, unsafe/empty no-probe;
  domain/storage/verifier suites и workspace Clippy прошли. Worker consumer
  aggregation/checkpoint wiring остаются следующим этапом 7.11c.

- **7.11c. Multi-repository worker publication (service завершён).**
  Direct execution сохраняет immutable submit snapshot/scope и external baselines,
  доказывает canonical saved roots по configured trusted directories; extra,
  duplicate, malformed, untrusted/nested-mismatch и отсутствующие baseline scopes
  fail closed до HTTP/checks. Guards повторяются при observation/verification/
  collection/finish, symlink rebindings запрещены, baseline не переснимается.
  Verifier получает только authenticated affected roots. Final result сохраняет
  main-first/sorted external repositories, qualified changed/committed/scope/
  Git-policy lists, task_changed_paths и baseline_dirty_paths только main;
  worker checkpoint получает configured trusted roots. Missing external repo
  приводит к verifier error без запуска checks и external_repo_missing в review.
  Worktree mode сохраняет запрет external scopes. Checks: 21 observation/publication
  HTTP/Git/SQLite/subprocess tests (включая 8 invalid preparation cases), все 10
  frozen aggregation fixtures, actual check effects, three-repository checkpoint,
  snapshot/scope tamper и root/scope symlink rebindings. Terminal error change
  aggregation подключена в 7.9b; Full FSM и live provider smoke остаются открыты.

### 7.12. Cooperative close

- **7.12a. Runner close consumer (завершён).** Startup close и close во время
  preparation/HTTP/observation/verifier заканчивают execution context до cleanup.
  Direct close повторно берёт admission/lifecycle fences; worktree close вызывает
  existing lifecycle recovery, который останавливает только owned runtime,
  удаляет registered checkout и затем atomically closes task. Busy/unsafe cleanup
  возвращает CloseDeferred и сохраняет marker/checkout; closed до removed row
  невозможен. Новые runner HTTP/SQLite tests покрывают startup/identity/session/
  prompt close, no resend, pre-send rollback и explicit unsent claim. Real runtime
  fixture проверяет deferred live fence -> cleanup -> removed -> closed, main repo
  не меняется. Revision fixture после executor commit также прошёл.


### 7.13. Structured findings validation (v7, завершено)

- **Цель:** валидировать persisted `rounds.structured_findings` на
  `request_changes`/revision-пути и запрещать ослабление обязательного textual
  findings.
- **Source evidence:** `worker.py:57,107-113,1532-1656,1741-1762`;
  `mcp_server.py:174-351,753-761,2712-2859`.
- **Содержание:** optional `structured_findings` на `request_changes`,
  обязательный textual findings, validation severity/path/line/code/message,
  scope-check через `validate_allowed_paths`, canonical hash/idempotency,
  persisted revision prompt и read-only UI.
- **Критерии приёмки:** malformed/unknown keys/too many/out-of-scope/severity
  fail closed (`structured_findings_invariant`), textual findings обязателен;
  canonical payload hash сохраняет идемпотентность.
- **Targeted checks:** worker structured-findings tests; MCP
  `request_changes` structured cases.
- **Результат:** `bridge-worker::validate_revision_findings` возвращает
  immutable `RevisionFindings`: обязательный textual summary, строгий domain
  schema, filesystem-aware path normalization и task scope check, включая
  trusted external Git roots и symlink escapes. Hash повторяет Python sorted
  UTF-8 JSON с default separators; None/[] сохраняют historical text-only hash.
  `create_round` использует атомарный storage `create_revision_round_with_findings`:
  findings/round/event/task commit вместе; identical retries возвращают исходный
  round с `replayed=true`, без новых writes и необходимости повторного spawn.
  Project/task/kind/hash conflict и awaiting-review/close gates проверяются в
  транзакции. Read-only `get_round_structured_findings` строго разбирает колонку.
  Revision dispatch проверяет persisted findings до session HTTP и повторно
  после resolution; malformed/schema/scope/text violations атомарно завершают
  unsent revision как failed с `structured_findings_invariant`. Отдельный
  `fail_revision_findings` сохраняет current/project/task fencing, close priority
  и historical transition table. Prompt содержит нормализованный single-line
  block; text-only/[] сохраняют прежний template. Default dispatch не доверяет
  external roots; для configured roots есть явный entry point.
- **Проверки:** 409 worker/storage tests, включая 10 frozen MCP structured cases,
  2 independent Python hash goldens, shape/limits/scope/symlink/trusted-root cases,
  corrupted JSON/SQL type, pre/post-session checks, no-HTTP invariant failures,
  rollback, concurrent None/[] replay, close precedence и redaction.
  Loopback integration suite прошла при разрешённом socket creation;
  workspace all-targets clippy, format и diff check прошли.
- **Границы:** Rust schema target остаётся v15; historical fixtures не менялись.
  Public MCP handler/wiring — 8.12; GUI/read-only rendering — 12.13;
  secret/budget gates, worker observation/recovery и live models не входят.
- **Зависит от:** 0A.3, 1.7, 3.12a. **Открывает:** 12.13.

### 7.14. Soft budgets usage aggregation (v8, завершено)

- **Цель:** сохранять/агрегировать observed usage и поддерживать gate между
  раундами.
- **Source evidence:** `usage.py:15-297`; `worker.py:2042,2069-2131,1011`;
  `mcp_server.py:938-948,2289-2291,2798-2831`.
- **Содержание:** limits input/output/reasoning/cache_read/cache_write/cost +
  `warning_threshold` (default `0.8`), saved usage aggregation
  (`total_saved_usage`), gate между раундами, explicit `allow_budget_override`.
  Worker только наблюдает/persist usage; gate — в MCP `request_changes_impl`.
- **Критерии приёмки:** не realtime kill/billing guarantee; exhausted →
  `budget_exhausted`; corrupt budget fail closed; override только gate.
- **Targeted checks:** usage tests; budget MCP cases.
- **Результат:** `bridge-storage::usage` нормализует и агрегирует saved usage,
  вычисляет deterministic budget state и decision `budget_exhausted`/`budget_corrupt`.
  Read-only storage service читает budget/results в одном snapshot, без task writes
  и reservation; explicit override меняет только decision. Worker accounting
  пересчитывает dedicated-round history, включая TUI continuations, сохраняет
  latest valid model и usage через atomic finish, без суммирования повторных polls.
  Budget gate применяется будущим MCP handler 8.13 под его lifecycle fence;
  полный observation loop остаётся в 7.9–7.11. Corrupt budget override не отменяет
  отдельные строгие storage guards. Floating overflow насыщается fail-closed,
  а не превращается в нулевое spending.
- **Проверки:** 18 frozen-v15 Python goldens, 413 worker/storage tests,
  history/continuation/model/rollback/project fence и override integration;
  workspace all-targets clippy, format и diff check.
- **Зависит от:** 0A.3, 1.7, 3.12f. **Открывает:** 12.14.

### 7.15. Per-round checkpoints (v12, завершено)

- **Цель:** считать и persist per-round `checkpoint_json`.
- **Source evidence:** `git_snapshot.py:32-458`; `worker.py:1366-1529`;
  `storage.py:44-65,854-976,1991-1999`.
- **Содержание:** HEAD/index/worktree fingerprints и настоящий per-round
  diff-stat add/modify/delete/rename + file/binary/symlink/unknown; только
  immediate predecessor; corrupt/unavailable/topology fail closed; bounded exact
  state без частичного truncation (`ROUND_CHECKPOINT_MAX_STATE_ENTRIES=2000`);
  relative external `1..N` labels; atomic round finish. **НЕ** архив full patch
  и **не** auto rollback.
- **Критерии приёмки:** predecessor-only; превышение bound → unavailable;
  canonical topology `workspace`/`external N`.
- **Targeted checks:** `diff_round_stat` tests; worker checkpoint tests;
  storage `parse_round_checkpoint` tests.
- **Результат:** Git checkpoint state использует тот же listed-file set и digests,
  с file/binary/symlink classification и отдельным kind-aware fingerprint.
  `diff_round_stat` считает predecessor-only add/modify/delete/rename, включая
  deterministic duplicate-digest rename pairing и baseline unknown kind.
  Worker builder берёт строго round N-1, проверяет topology/availability/trusted
  external roots, хранит exact changed/absent до 2000 entries; oversized или
  non-UTF8/unrepresentable state unavailable без усечения. Только relative paths
  и workspace/external N labels сериализуются. Worktree root/baseline передаются
  явно от authenticated caller; worktree execution wiring остаётся в 7.16.
  `finish_round_with_checkpoint` и worker `finish_round_with_diagnostics` пишут
  checkpoint/usage/task/round/event атомарно; read-only corrupt parsing unavailable.
- **Проверки:** 9 frozen Python diff-stat goldens; 481 Git/storage/worker tests,
  реальные temporary repos, predecessor-only delta, corrupt/missing predecessor,
  exact-state bound/topology, binary/symlink/exec, ignored files, rollback и
  combined checkpoint/usage finish. Clippy/format/diff check прошли.
- **Зависит от:** 0A.4, 1.7, 3.12a. **Открывает:** 12.13.

### 7.16. execution_mode=worktree execution (v10, завершено)

Завершено как исполнимые application services: подготовка checkout/runtime,
исполнение round consumers в execution root, recovery/close и logical quarantine.
Config validation — 2.10, storage lifecycle — 3.12e, security policy — 0A.5.
Full worker FSM/CLI/MCP adapters, parallel per-task locks 7.17 и delivery 9.18
остаются отдельными пунктами; live OpenCode/model smoke ещё не заявлен.

- **7.16a. Git checkout/binding и base HEAD (завершено).**
  - **Цель:** dedicated checkout + submit-time base HEAD, task execution root vs
    project workspace.
  - **Source evidence:** `git_worktree.py:41-769`; `worker.py:344-490,660`.
  - **Критерии приёмки:** direct default не меняется; checkout вне project
    workspace; external repos/submodules/LFS/sparse/nested/environment setup —
    deferred (см. ограничения).
  - **Результат:** `bridge-git::checkout` создаёт detached checkout по exact
    submit-time OID, доказывает canonical slot/root/common-dir и admin backref.
    Symlink/foreign/missing registered paths fail closed; explicit remove без
    global prune. Unsupported LFS/submodule/sparse и state внутри workspace
    отклоняются до add. Worker/storage binding реализован в 7.16c.
  - **Проверено:** 72 Git tests, включая 7 real-repository checkout scenarios;
    Clippy, fmt и diff check.
  - **Зависит от:** 0A.5, 2.10, 3.12e. **Открывает:** 7.16b.
- **7.16b. Task-scoped OpenCode runtime и identity guard (завершено).**
  - **Цель:** task-scoped endpoint/token/process logs вне checkout; identity
    guard.
  - **Source evidence:** `worktree_runtime.py:45-484`; `worker.py:660`;
    `mcp_server.py:2259-2378,2502-2523`.
  - **Критерии приёмки:** runtime files вне checkout; чужой checkout fail
    closed.
  - **Delta v17:** startup берёт manager lock с bounded wait 60s
    (`3 * READY_TIMEOUT`, READY_TIMEOUT=20s); lock timeout происходит до
    spawn/reservation/record. Runtime files/identity checks остаются под lock.
    Source: `worktree_runtime.start_worktree_server:450-485` (v17).
  - **Результат:** `bridge-runtime` запускает/reuses `opencode serve` только
    после storage/Git binding proof; loopback ports 43000..43999, reservation
    до readiness, health + unscoped /path + OpenAPI, frozen model requirement.
    Private runtime/log/atomic record вне checkout; pidfd + start ticks + boot id
    защищают stop от PID reuse; failed spawn/readiness killed/reaped/unreserved.
    CLI adapter остаётся отдельным потоком 9; runtime library исполнима.
  - **Проверено:** 10 local-process runtime scenarios и 173 config tests;
    concurrency/reuse/default lock/timeout/partial locks, foreign/stale/corrupt
    records, root/doc refusal, cleanup, pinned model и port reservations.
  - **Зависит от:** 7.16a, 6.1, 9.7a. **Открывает:** 7.16c, 15.2.
- **7.16c. Revision reuse и verifier/change collection cwd (завершено).**
  - **Цель:** revision reuse checkout + verifier/change collection cwd.
  - **Source evidence:** `worker.py:344-490,1366-1529`; `git_worktree.py`.
  - **Критерии приёмки:** revision не пересоздаёт checkout; verifier/change
    collection видит worktree.
  - **Targeted checks:** worktree revision/verifier integration tests.
  - **Результат:** submit атомарно сохраняет pending checkout и точный clean
    base HEAD; worker service создаёт checkout, сохраняет baseline и связывает
    session/dispatch/verifier/change collection/checkpoint с execution root.
    Task.workspace остаётся main workspace; revision сохраняет checkout и runtime.
    Created checkout и baseline никогда не пересоздаются/не обновляются молча.
    Direct execution и исторический submit hash сохранены; full FSM/CLI/MCP
    wiring остаются отдельными пунктами плана.
  - **Проверено:** 519 tests Git/storage/submission/worker/runtime, включая
    реальные Git checkout, два раунда с fixture server и verifier cwd;
    workspace all-target clippy без warnings.
  - **Зависит от:** 7.16b, 5.4. **Открывает:** 7.16d.
- **7.16d. Close/recovery/retention и orphan logical quarantine (завершено).**
  - **Цель:** close/recovery/retention и logical orphan quarantine.
  - **Source evidence:** `git_worktree.py:455-769`;
    `storage.py:3319-3929`.
  - **Критерии приёмки:** fail-closed transition maps; orphan logical
    quarantine без физической очистки.
  - **Targeted checks:** worktree recovery/quarantine tests.
  - **Результат:** `bridge-worker::lifecycle` persist close → nonblocking
    worker lock → removing → owned server stop → proven checkout removal →
    private task dir removal → removed → closed. Pending terminalizes logically;
    error rows and review/failed/needs-user/delivery-unknown/accepted results retained.
    Failed cleanup сохраняет close marker/removing и фиксированный deferred event;
    crash after physical removal retries idempotently. Storage forbids early
    worktree close in the same transaction; historical v6/direct behavior preserved.
    Created missing checkout fails active round with worktree_missing, blocks
    admission and never recreates; existing checkout requires Git binding proof.
    Orphan scan is logical/idempotent, without move/remove/chmod/global prune.
    Nested/external/symlinked scopes проверяются до создания/HTTP.
  - **Проверено:** 24 runtime/worker integration scenarios и 8 checkout scenarios,
    включая close while busy, foreign record, pending/creating/error, failure after
    physical cleanup, missing registered checkout, retained results, orphan
    idempotency и symlink root/entry refusal; **1080 workspace tests passed**,
    all-target clippy без warnings, format/diff checks passed.
  - **Зависит от:** 7.16c, 3.12e. **Открывает:** 9.17, 9.18a.

### 7.17. Writers admission/locks (v14 B1 + v15 B2, не завершено)

Не завершено. B1 (direct) и B2 (parallel worktree) — независимые подзадачи;
каждая даёт один основной результат/критерий/targeted check.

- **7.17a. B1 direct sequential same-project chains (завершено).**
  - **Цель:** direct-mode `max_active_tasks=1`, sequential same-project
    `depends_on` chains, explicit activation/rebaseline.
  - **Source evidence:** `config.py:44-56,90-101,418-466`;
    `worker.py:205-318,2152-2186`; `storage.py:2182-2487,2775-2878`;
    `mcp_server.py:1859-1970`.
  - **Содержание:** reservation ledger single-writer + `admission.lock` +
    `workers/<task_id>.lock`; project lock в single-writer; explicit activation
    direct before first attempt; no automatic inheritance accepted worktree
    result without delivery.
  - **Критерии приёмки:** no startup auto-activation/scheduler/autoaccept; old
    submit hash idempotent; v14 same-project chains (старое self-project
    ограничение не финальный контракт).
  - **Targeted checks:** storage admission + worker lock tests.
  - **Зависит от:** 1.6, 2.11, 3.12c, 3.12d. **Открывает:** 7.17b, 8.14.
  - **Результат:** `bridge-worker::admission` связывает project admission,
    project worker и per-task locks; открытие state/worker admission не
    активирует waiting tasks. Явная direct activation проверяет strict persisted
    dependency objects, same-project accepted/workflow gate, close fence,
    пересобирает main/external baseline с собственными dirty/commit policies.
    Storage refresh/activation отвергают pending close атомарно. Guard drop
    освобождает и частично взятые locks; lock symlinks/nonregular files отвергаются.
    Linked-project gate и workflow submit persistence остаются в 8.14;
    worker FSM/CLI/MCP adapters не считаются завершёнными.
    Проверки: worker admission (8 cases), cross-process lock tests (8),
    storage dependencies (10), targeted clippy; old request hash не меняется.

- **7.17b. B2 parallel worktree writers (services завершены; live smoke 15.3 открыт).**
  - **Цель:** `allow_parallel_writers=true` (только worktree), parallel
    admission без ослабления defaults.
  - **Source evidence:** `storage.py:2516-2641`; `mcp_server.py:2453-2500`;
    `config.py:439-466`.
  - **Содержание:** `active_writers.parallel` + partial unique
    `ux_active_writers_single`; atomic admission against ledger AND real writer
    statuses; canonical symlink-resolved file/directory ancestor scope overlap;
    corrupt scope fail closed; per-task lock; both lock fencing independent of
    live config downgrade/upgrade; all-task recovery.
  - **Критерии приёмки:** overlapping writer refused; defaults не ослаблены; не
    путать worktree execution с уже готовым worktree manifest `bridge-git`
    (4.7/4.8).
  - **Targeted checks:** storage scope/admission tests; parallel smoke (15.3).
  - **Зависит от:** 7.17a, 7.16b. **Открывает:** 7.17c, 12.15, 15.3.
  - **Результат:** saved reservation flag выбирает worker fences независимо от
    live config; single worker удерживает project+task locks, parallel — task
    lock после project probe под admission lock. Single fallback проверяет
    все task locks, включая потерянный ledger. `admit_saved_writer` атомарно
    проверяет scope/real statuses/own reservation и восстанавливает missing row;
    overlap/corruption fail closed. `prepare_fenced_round_execution` удерживает
    fences через checkout/server/dispatch/verification. Worktree activation
    сохраняет base/snapshot; project recovery посещает все задачи без scheduler.
    Close/recovery одной parallel task не затрагивает соседний executor.
    Проверки: 13 worker admission, 18 storage writer, 25 runtime tests, clippy.
    Local subprocess/HTTP fixture доказывает два checkout/server/ports и
    independent dispatch/collection/close. Это не live-model smoke:
    production CLI и реальный `smoke-opencode --parallel-worktrees` остаются 15.3.

- **7.17c. Shared safe full active set (service завершён).**
  - **Цель:** read-model полного active set для API/UI/diagnostics/hook.
  - **Source evidence:** `storage.py:2182-2346`;
    `diagnostics.py:351-367`; `mcp_server.py:2567-2596,2032-2052`.
  - **Содержание:** несколько active writers + reservations + waiting count;
    `ambiguous_task` без ID при нескольких unfinished.
  - **Критерии приёмки:** ID-less `task_status` при >1 → `AMBIGUOUS_TASK` +
    read-only список.
  - **Targeted checks:** active-set read-model tests.
  - **Зависит от:** 7.17a. **Открывает:** 8.16, 9.19a, 12.15.
  - **Результат:** `bridge-storage::active_set` читает полный unfinished set
    и strict reservations одним read transaction; writers идут перед waiters,
    counts и activity fence не зависят от config или целостности ledger.
    `resolve_status_task` с ID проверяет project и допускает terminal lookup;
    без ID при >1 возвращает typed `Ambiguous`/`ambiguous_task` и безопасные
    task summaries. Read path не активирует задачи, не восстанавливает ledger
    и не пишет state; query_only tests проходят. Unknown nonterminal status,
    malformed task/mode/reservation fail closed. Transport/UI wiring — 8.16,
    9.19a, 12.15. Проверки: 5 active-set tests и storage all-targets clippy.

- **7.17d. Delivery gate on active writer.**
  - **Цель:** `deliver` gated on any real/reserved active writer под admission
    lock.
  - **Source evidence:** `delivery.py`; `storage.py:238-242,3512`.
  - **Критерии приёмки:** deliver невозможен при active/reserved writer.
  - **Targeted checks:** delivery writer-gate tests.
  - **Зависит от:** 7.17a, 9.18a. **Открывает:** 9.18d.

### 7.18. Executor profiles (v13, завершено)

- **Цель:** применить immutable submit-time profile snapshot к prompt overlay и
  effective model.
- **Source evidence:** `profiles.py:37-416`; `prompts.py:9-159`;
  `worker.py:1768-1780`; `config.py:45,58,344-413`;
  `docs/executor-profiles-design.md:61-92,254-260`;
  `tests/test_profiles.py`, `tests/test_prompts.py`.
- **Содержание:** builtins `implementer`/`test-writer`/`migration-specialist`/
  `review-investigator` и custom/`default_profile`; canonical immutable
  submit-time `profile_json`/`profile_hash`/`source`; effective model
  (`snapshot_model_source` → profile model else project model); prompt overlay
  (`{profile_block}`) **без weakening mandatory rules** (allowed_paths/git/
  test/verifier rules — fixed template lines); historic default hash backward
  compatible (`is_historical_implementer`/`is_historical_snapshot`).
- **Границы:** профиль **не** grants permissions/sandbox и **не** заменяет
  review.
- **Критерии приёмки:** corrupt/mismatched snapshot fail closed до
  prompt/session/HTTP; historical implementer байт-в-байт как legacy.
- **Targeted checks:** profile snapshot/integrity tests; prompt byte-identical
  historical test.
- **Зависит от:** 0A.2, 2.12, 3.12a, 8.18. **Открывает:** 12.13.
- **Реализовано:** обе dispatch-mode применяют только persisted snapshot;
  профиль проверяется до render/session/HTTP и повторно после session resolution.
  Missing/corrupt identity атомарно завершает unsent round/task, сохраняя close
  priority. Явный pinned model=null не подхватывает новую модель config;
  legacy profile=NULL сохраняет прежнее поведение. Fixed scope/Git/test/verifier
  template не меняется. Шесть frozen Python cases проверяют оба prompt побайтово;
  интеграционные тесты проходят через submission → storage → HTTP mock.
  Production worker loop/CLI, MCP transport и GUI остаются отдельными задачами.
- **Проверено:** 1045 workspace tests, all-targets Clippy без warnings, fmt/diff check.
  Submit hash corpus включает Python float notation для дробных бюджетов.

**Готовность потока:** worker scenarios совпадают по БД и результату.

## Поток 8. MCP

### 8.1. `project_info` (завершён)

`bridge-mcp::McpServer::project_info` возвращает frozen v17 immutable binding,
execution/delivery modes, limits, default/profile labels и полную reservation
identity. Активная задача/статус и writer ids читаются через единый active_set
snapshot (7.17c), без reconcile/activation/recovery. Instructions/models/secret
paths/task contents не попадают в profile surface. Ownership/schema/corrupt
state fail closed; tool failure не возвращает partial data. Все 5 frozen
project_info cases проверены (legacy fields + additive v17 surface).

### 8.2. `submit_task`

- **8.2a. Standalone adapter (завершён).** Required/type/secret/profile/budget/
  command/scope guards; raw public-path hash и replay до filesystem/HTTP;
  main/relevant external Git snapshots с recheck под admission fence;
  direct health/root proof, worktree без static endpoint probe. Atomic
  submission pins normalized scope/profile/policy, worker spawn lease
  предотвращает duplicate и откатывается только при failed spawn.
  Workflow metadata пока возвращает explicit refusal, полный parity открыт.

### 8.3. Compact `task_status`

- **8.3a. Standalone adapter (завершён).** Explicit ID или coherent active-set
  selection; ambiguity не активирует задачи. Current round/status, in-flight
  agent/verifying phase, worker fences/lease и saved actionable diagnostics.
  Close recovery и orphan active worker spawn подключены; terminal не spawn.
  Полный frozen response parity остаётся открытым.

### 8.4. Verbose `task_status`

- **8.4a. Saved diagnostics adapter (завершён).** Для non-inflight добавлены
  task/scope/tests, result/response, timestamps и saved checkpoint; review
  показывает budget. Full rounds/delivery/TUI recovery surface ещё открыт.

### 8.5. Long wait и раннее пробуждение

- **8.5a. Bounded wait (завершён).** wait_seconds 0..300, monotonic deadline,
  re-read persisted status, early return при terminal/review/verifying или
  отсутствии worker после lease. No-id needs_user не выполняет recovery.

### 8.6. `request_changes`

- **8.6a. Standalone adapter (завершён).** Secret/structured scope/idempotency,
  review fences, saved budget gate/explicit override, automation ownership,
  idle-session proof и atomic revision перед worker spawn. Max revision
  guard атомарно переводит задачу в needs_user и возвращает revision_limit
  с безопасным detail (9.5c3).

### 8.7. `accept_task`

- **8.7a. Review prerequisites (завершены).** Narrow atomic manual accept
  проверяет awaiting_review/current complete round, frozen manual policy и close;
  task/event/reservation release commit together. Repeated accepted returns no event.
  Worker `try_review_fences` не активирует задачи и не изменяет reservations;
  `task_execution_view` доказывает saved worktree checkout/endpoint без fallback
  к static project server. Scoped session/status gate: absent/idle -> idle,
  busy/retry/unknown state -> active, malformed -> refusal. Exact spawn lease
  rollback не отменяет started child и close. Shared secret scanner экспортирует
  только stable categories. 3 storage atomic/rollback/guard tests и OpenCode
  loopback activity matrix прошли, workspace Clippy чист. MCP adapters и
  on_accept delivery остаются отдельными consumers.

- **8.7b. Manual adapter (завершён).** Under review fences proves saved session
  idle; stops owned worktree server before atomic accept, retains checkout.
  Repeated manual accept read-only; frozen on_accept подключён в 8.19.

### 8.8. `close_task`

- **8.8a. Cooperative adapter (завершён).** Nonempty reason, current session
  activity gate до marker; busy worker сохраняет close_requested. Direct
  completes under fences, worktree becomes closed only after owned cleanup;
  repeated/status calls finish deferred cleanup without spawn.

### 8.9. stdio transport (foundation завершён)

`bridge-mcp::protocol` + `stdio`: bounded newline JSON-RPC framing (1 MiB),
initialize/initialized/ping/tools/list/tools/call, protocol negotiation
2024-11-05/2025-03-26/2025-06-18, fixed redacted errors, EOF releases MCP lock.
Stdout содержит только JSON-RPC; notifications не получают reply. Без worker launcher Tools list содержит project_info. Production CLI из
9.5c1 injects launcher и объявляет шесть tools; notifications не вызывают
handlers. Полный source parity всех delegated modes ещё не завершён.
Протокол проверен по [MCP transport spec](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports)
и [lifecycle](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle).

### 8.10. Authenticated HTTP transport (read-only foundation завершён)

`bridge-mcp::http` реализует sessionless Streamable HTTP JSON responses:
POST /mcp, notifications/responses → empty 202, GET/SSE → 405. Protocol
2025-03-26/2025-06-18 headers проверяются, initialize negotiates supported
version. Только 127.0.0.1; каждый request перечитывает private configured MCP
bearer token (rotation/revocation без restart), digest compare без раннего
выхода. Exact local Host/Origin allowlist, auth before dispatch, без OAuth
issuer/access logs/raw token diagnostics. CLI preflight credentials/atomic
port bind до state initialization; occupied port не затрагивается.
HTTP/1.1 Content-Length и chunked bodies поддержаны; duplicate headers,
TE+CL ambiguity, chunk trailers и Expect отклоняются. Limit: 1 MiB body,
32 KiB headers/chunk overhead, 8 KiB line, 16 connections, total request-read
deadline 10s. Overload → 503; JSON-RPC malformed input → fixed errors, no echo.
Это bounded HTTP profile без SSE/session resumption/HTTP2. Foundation bind
оставляет project_info; bind_with_workers/CLI включает standalone handlers. Проверки: 9 real loopback cases (auth/rotation/Origin/Host/
framing/parallel/overload/deadline/lock), HTTP CLI process + startup preflight.
Source: `mcp_http.py` v17 и
[MCP transport specification](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports).

### 8.11. Startup recovery

- **8.11a. Standalone startup scan (завершён).** Stdio/HTTP запускают recovery
  в отдельном scoped thread одновременно с transport loop: handshake не ждёт
  HTTP probes/spawn. MCP ownership lock удерживается до завершения recovery,
  включая shutdown; ошибки выводятся только fixed warning в stderr.
  Под admission fence сверяется writer ledger; orphan worktrees регистрируются
  только в logical quarantine. Восстанавливается весь unfinished set:
  implementing/revising — через existing lifecycle/spawn fences и persisted
  startup lease, needs_user — через background blocker gate и atomic claim.
  Deferred close завершается без нового prompt. Waiting/review/failed и
  read-only embedders без spawner инертны. Delivery recovery подключён в 8.11b.
  Ошибка одного spawn не останавливает остальных writers, failed spawn отпускает
  собственный lease/claim. Saved attempted round/session/outbound и baseline
  сохраняются; pending worktree передаётся worker без static endpoint probe.
  **Граница:** full source error-policy parity остаётся в 9.5c;
  full source parity не заявлена. Targeted tests: 9 новых startup cases,
  включая concurrent claims, live worker/lease, blocker gate/rollback,
  parallel writers, deferred close/reconcile, orphan preservation и
  stdio/authenticated HTTP handshake при blocked spawn.
  Проверки: `cargo test --offline -p bridge-mcp -p agent-bridge-cli`;
  targeted all-target Clippy, fmt и diff-check. MCP/CLI regressions: 57 tests
  passed (24 task tests, включая 9 новых startup cases); warnings отсутствуют.

### 8.12. `request_changes` structured findings (v7, не завершено)

- **Цель:** handler `request_changes` с optional `structured_findings` и
  обязательным textual findings.
- **Source evidence:** `mcp_server.py:174-351,753-761,2712-2859`;
  `tests/test_mcp.py:1963-2291`.
- **Критерии приёмки:** canonical hash/idempotency; validation severity/path/
  line/code/message; textual findings обязателен. Depends on 7.13.
- **Targeted checks:** MCP structured-findings contract fixtures.

### 8.13. `submit_task`/`request_changes` soft budgets (v8, не завершено)

- **Цель:** budget contract и gate между раундами.
- **Source evidence:** `mcp_server.py:938-948,1124-1131,2289-2291,2798-2831`;
  `usage.py:198-297`.
- **Критерии приёмки:** warning/exhausted gate, `allow_budget_override` только
  для gate, corrupt fail closed, не realtime kill/billing. Depends on 7.14.
- **Targeted checks:** MCP budget cases.

### 8.14. Workflow/dependency metadata и gate (v9 + v11, завершено)

- **Цель:** `workflow_id`/`depends_on` metadata/graph и `waiting_dependencies`
  gate.
- **Source evidence:** `mcp_server.py:354-688,2353-2360,2526-2543`;
  `storage.py:103,2807-2878`; `tests/test_mcp.py:1030-1927,6370-6697`.
- **Критерии приёмки:** gate только accepted predecessors; explicit
  `task_status` activation atomic single winner; read-only linked DBs; no
  startup auto-activation/scheduler/autoaccept; missing/unlinked/unreadable/
  cycle/workflow mismatch fail closed; v14 same-project chains поддержаны.
  Depends on 1.6, 7.17a, 3.12c (baseline не ждёт B2 parallel).
- **Targeted checks:** workflow/dependency MCP fixtures; cycle tests.

### 8.15. Suspected secrets scanner gate (не завершено)

- **Цель:** categories-only secret gate на `submit_task`/`request_changes`.
- **Source evidence:** `secret_scanner.py:29-134`;
  `mcp_server.py:764-780,2297-2299,2735-2740`;
  `tests/test_mcp.py:345-539`.
- **Критерии приёмки:** `{"error":"suspected_secrets","categories":[...]}` без
  value/position; `allow_suspected_secrets` bypass; type-checked. Depends on
  0A.5.
- **Targeted checks:** secret-scanner cases.

### 8.16. `project_info` active set и `ambiguous_task` (v15, не завершено)

- **Цель:** read-only active set и ID-less `task_status`.
- **Source evidence:** `mcp_server.py:2567-2596,2032-2052`;
  `storage.py:105-129,2182-2346`; `diagnostics.py:351-367`;
  `tests/test_mcp.py:5820-6793`.
- **Критерии приёмки:** `project_info` отдаёт
  `execution_mode`/`max_active_tasks`/`profiles`/`parallel_writers_enabled` и
  full active set; ID-less `task_status` при >1 unfinished →
  `AMBIGUOUS_TASK` + read-only список. Depends on 7.17c.
- **Targeted checks:** project_info/ambiguous-task fixtures.

### 8.17. `submit_task` execution_mode validation (v10, не завершено)

- **Цель:** submit-time проверка worktree-режима.
- **Source evidence:** `mcp_server.py:2259-2378,2502-2523`.
- **Критерии приёмки:** absolute external `allowed_paths` запрещены в worktree;
  `_worktree_submit_validation` (repo support, nested rejection); pending
  `worktrees` row. Depends on 2.10, 7.16a.
- **Targeted checks:** worktree submit validation tests.

### 8.18. Submit-time profile resolution (v13, завершено)

- **Цель:** resolve/build canonical profile snapshot на `submit_task`.
- **Source evidence:** `mcp_server.py:736-749,2231-2257`;
  `profiles.py:210-416`; `config.py:45,58,344-413`.
- **Содержание:** resolution origin `argument`/`project_default`/
  `builtin_default`; canonical snapshot/hash; non-historical profile входит в
  submit payload; `profile` — параметр `submit_task`.
- **Критерии приёмки:** immutable snapshot; historical implementer не меняет
  payload/hash; corrupt definition fail closed.
- **Targeted checks:** MCP profile submit cases.
- **Зависит от:** 0A.2/0A.3, 2.12, 3.12a. **Открывает:** 7.18.
- **Реализовано:** `bridge-submission::submit_task_with_profile`: строгий публичный
  аргумент profile, selection origin, historical/non-historical payload hash,
  atomic task/round/profile/budget/admission write и immutable replay.
  `get_task_profile` проверяет полную сохранённую identity без current config.
  Проверены rollback, concurrent replay, changed definition conflict и Python goldens.
  MCP transport/handler adapter остаётся в 8.2/8.9–8.10; этот этап закрывает
  application service создания задачи, без запуска worker.

Одна tool-задача содержит только один handler и его contract fixtures.

### 8.19. On-accept delivery MCP (v16, завершено)

- **8.19a. Accept orchestration.** Task-frozen on_accept policy. Lock order:
  project worker → task lifecycle → admission; admission берётся до server stop
  и accepted/reservation release и удерживается через build/apply. Busy first
  accept оставляет awaiting_review; other writer fence действует после release
  собственного reservation. Source: `mcp_server._accept_awaiting_review`,
  `_accepted_result`, `_accept_already_accepted`, `_retry_on_accept_locked`
  (v17). Already-delivered repeat — read-only, manual/direct repeat сохраняет
  прежний ответ без locks. Checks: `tests/test_delivery_on_accept.py` first/
  repeat/busy/live-config/crash cases. Depends on 3.13b, 2.14, 9.18e, 0B.4.
- **8.19b. Status/refusal surfaces.** Frozen delivery_mode в task result,
  project_info config mode; pending/refused/applying/delivered summary с
  последней попыткой, реальный persisted state. OSError/SQLite failure после
  accept → accepted + refusal (`delivery_io_error`/`state_unavailable`),
  unreadable state → unknown, private absolute paths редактируются. Process
  termination/crash injection не маскируются. Source: `_delivery_summary`,
  `_auto_delivery_payload`, `_delivery_failure_detail` (v17). Checks: I/O,
  partial apply, stale refusal, state read failure и redaction. Depends on
  8.19a. Opens 12.16.

### 8.20. task_status explicit recovery (завершено)

- **Цель:** wait_seconds=0 делает один needs_user recovery attempt и возвращает
  фактический persisted implementing/revising/awaiting_review/needs_user status.
- **Source:** `mcp_server.task_status_impl:2120-2300` (v17).
- **Приёмка:** проигравший claim re-reads task; repeated/concurrent calls не
  spawn duplicate в lease window. Failed recovery требует positive wait;
  review/background opt out сохраняют gate. No prompt resend/auto permission.
- **Проверки:** zero-wait/concurrent recovery MCP fixtures.
- **8.20b. Failed assistant recovery (завершён).** Только explicit task_status
  с positive wait восстанавливает attempted current failed/assistant_error.
  Endpoint/root, round/session/outbound/error проверяются до и после HTTP;
  atomic claim переводит тот же round в observing, сохраняя baseline/usage.
  Admission losers перечитывают status и ждут в пределах positive window;
  spawn-before-lock lease исключает дубли. Failed spawn возвращает исходные
  failed/error_code, stale release другого recovery kind отклоняется.
  Infrastructure errors и background/startup не reopening. Checks: storage
  eligibility/revise/rollback/cross-kind fencing и concurrent MCP recovery.
- **8.20a. needs_user adapter (завершён).** Explicit zero-wait invokes guarded
  service 7.10b, re-reads persisted status, failed spawn releases claim;
  implicit task selection не восстанавливает needs_user. Проверен no-resend/
  one-claim lease. Full source error-policy parity ещё открыт.
- **Зависит от:** 7.10b, 0B.4.

## Поток 9. Runtime CLI

### 9.1. CLI parser и общие flags

Parser для `launch-opencode` (9.13b), `mcp`/`serve-mcp` (9.5a/b): explicit
project/config/state-root, help/version, separated/inline values, safe errors.
Остальные commands, defaults и общая унификация parser остаются открытыми.

- **9.1b. CLI worker (завершён).** `agent-bridge worker` принимает required
  `--project`, `--config`, absolute `--state-root`, typed `--task` UUID и positive
  u32 `--round`. Worker-only flags не принимаются другими commands; separated и
  inline values/duplicate checks сохраняют общий parser contract. Config/state
  разрешаются относительно caller cwd; state не создаётся/не импортируется.
  Runner получает полный configured project/layout registry для task runtime.
  Пять AB_* настроек берутся из environment; stdout остаётся свободным, stderr
  использует fixed diagnostics. Success/parked/deferred close -> 0, unknown task/
  round или parse error -> 2, busy -> 3, ownership/settings/service failures -> 1.
  Четыре реальные subprocess tests: pending dispatch и attempted recovery с
  no-resend, terminal repeat без HTTP, malformed/duplicate flags без echo,
  unknown/busy/unowned/settings exit codes и requested direct close. Targeted
  CLI tests, all-target Clippy и fmt прошли. Итоговый
  `cargo test --workspace --all-targets --offline`: 1248 passed, 0 failed.
  MCP delegated tools и live provider smoke остаются отдельными этапами.

### 9.2. `setup`

### 9.3. `doctor`

Delta v17: вывод execution/delivery modes из validated config
(`cli.cmd_doctor`); добавить output fixtures после 2.14.

### 9.4. `serve-opencode`

### 9.5. `serve-mcp` и `mcp`

- **9.5a. Stdio CLI + project MCP ownership (завершён).** Новый crate
  bridge-mcp, команда `mcp --project ID --config PATH --state-root ABSOLUTE_PATH`.
  Safe shared flags, guarded Rust-only initialize, private project dir 0700,
  O_NOFOLLOW/CLOEXEC MCP flock 0600; второй stdio/HTTP process того же project
  должен отказаться. Symlink aliases/foreign schema-marker/workspace-local root
  отклоняются; executor password не читается, Python не запускается.
  Проверки: 6 MCP integration tests (включая 5 frozen project_info cases),
  3 реальные CLI process tests (handshake/lock/foreign state), 8 controller CLI
  regression tests; targeted all-target clippy clean.
- **9.5b. Authenticated HTTP CLI (read-only foundation завершён).**
  `serve-mcp --project ID --config PATH --state-root ABSOLUTE_PATH` запускает
  foreground JSON HTTP server из 8.10 с configured mcp_url/mcp_token_file.
  HTTP/stdio используют один MCP lock; no stdout banner/HTTP access logs.
  Guarded Rust state и tool surface project_info совпадают со stdio.
  Проверены 9 HTTP cases, 5 CLI MCP process cases и 8 controller CLI regressions;
  Full workspace run: 1195 tests passed; all-target clippy clean.
  После финальной совместимости common request _meta и обновления help
  повторно пройдены protocol/help targeted checks и workspace clippy.
  Full task handlers/startup recovery остаются
  в 9.5c; process records/managed start/stop — в 9.6–9.10.
- **9.5c1. Standalone delegated handlers/worker launcher (завершён).**
  `mcp`/`serve-mcp` injects trusted Rust worker argv, detached spawn и reaper
  thread; ошибки fixed/content-free, stdout только protocol. Foundation
  embedders без spawner сохраняют read-only surface. 15 integration tests:
  idempotency/raw hash, refusals/no writes, scoped dirty baseline, failed spawn,
  concurrent status lease, explicit needs_user claim, activity fail-closed,
  budget override/structured scope, revision/accept/close и pending worktree.
  Реальный CLI MCP → child worker → awaiting_review → manual accept проверен
  на loopback OpenCode mock, один prompt и reservation release.
  Full workspace: 1268 tests passed; fmt/diff-check и all-target Clippy чисты.
- **9.5c2. Status payloads + verifier progress (завершён).** Новый status
  adapter разделяет compact/review/verbose, возвращает response как result,
  saved usage/models/repository views, baseline fallback и session-aware
  user_action. Worktree status использует worktree_state/delivery_mode без
  private runtime paths; checkout виден только в review/verbose. Task без round
  возвращает round_number=null. Corrupt budget читается только diagnostic
  mapper (lifecycle mapping остаётся strict), status не раскрывает malformed
  budget. Verifier сохраняет command_index/count перед командой; progress write
  failure запрещает запуск команды и публикацию completion. Done result
  неизменяем, corrupt progress counters очищаются до fixed shape.
  Проверки: 21 frozen payload case без skips, 27 MCP regression cases,
  294 storage regressions + progress guards, 71 verifier regressions +
  progress-write-failure test, 5 real CLI worker tests и targeted Clippy.
  Full error envelopes/workflow/on_accept остаются отдельными consumers.
- **9.5c. Full handlers/startup recovery (открыт).** Remaining: full error envelope parity,
  workflow metadata/activation и on_accept delivery. Local controller
  consumer 9.13c и live provider smoke остаются отдельными этапами.

### 9.6. Process ownership и pidfd primitives

### 9.7. `start` одного проекта

- **9.7a. Optional bounded manager-lock wait (завершено).** Canonical sorted
  roots, all-or-nothing nonblocking acquisition, monotonic deadline и poll
  backoff 20ms..200ms; все fd закрываются при timeout/error/interrupt.
  Обычные start/status/stop сохраняют wait=0; только worktree startup задаёт
  timeout. Source: `runtime._try_acquire_locks`, `_manager_lock:292-332`
  (v17). Checks: `tests/test_runtime.py` wait/acquire/timeout/default tests.
  Реализовано в `bridge-runtime::lock::ManagerLock`; safe worktree process
  ownership/pidfd prerequisite реализован в 7.16b. Direct runtime orchestration
  9.6 остаётся отдельной задачей.
  Depends on 0B.5, 9.6. Opens 7.16b.

### 9.8. Multi-project start и rollback

### 9.9. `status`

### 9.10. `stop`

### 9.11. `console` и `attach-opencode`

### 9.12. `launch-codex`

### 9.13. `launch-opencode`

- **9.13a. Controller launch service (завершён для HTTP MCP).**
  `bridge-runtime::controller` генерирует primary/linked MCP entries с
  `{env:VAR}` bearer placeholders, timeout 330000ms, frozen mandatory prompt,
  primary controller agent, subagent_depth=0 и permissions из 7.8b.
  Workspace opencode.json/jsonc проверяются до чтения tokens/записи state:
  JSONC comments/trailing commas поддержаны; malformed/unreadable config,
  reserved mcp names/default_agent/subagent_depth/agent.bridge-controller
  и env collision linked ids fail closed. Wrong-shaped mcp/agent отклоняются
  даже при пустых/false значениях (строже Python truthiness).
  Tokens передаются только child env; executor username/password удаляются;
  inherited provider env сохраняется, NO_PROXY/no_proxy дополнены loopback.
  Atomic same-dir config write/fsync, directory 0700/file 0600; отдельный
  controller.lock удерживается до выхода TUI. Guarded Rust-owned state,
  symlink ancestors/namespace/database/marker и workspace-overlapping root
  отклоняются до initialize. Child стартует без аргументов в workspace с
  inherited stdio; exit status возвращается caller, process env не меняется.
  Source: `opencode_launcher.py` (v17); checks: 10 offline controller tests,
  включая реальный fixture child, failure/lock/foreign-state/symlink cases;
  runtime all-targets clippy без предупреждений.
- **9.13b. CLI consumer (завершён для HTTP MCP).** Binary принимает
  `launch-opencode --project ID --config PATH --state-root ABSOLUTE_PATH`.
  Все три flags обязательны; config может быть relative к cwd caller;
  state root всегда explicit Rust namespace. Help/version доступны без state;
  duplicate/unknown/missing flags отклоняются safe error/exit 2, operational
  errors дают exit 1. TUI stdio наследуется; child exit code либо 128+signal
  возвращается shell. Проверены 8 CLI integration cases: argv/relative config,
  inline flags, help/version, inherited provider env/proxy/token isolation,
  exit/signal propagation, preflight failure without state, spawn recovery,
  NUL token reject before state и точный mode 0600 даже при umask 0777.
  Workspace all-targets clippy без предупреждений. CLI defaults/остальные
  commands остаются в 9.1 и соответствующих этапах 9.x.
  Full workspace run после CLI wiring: 1173 tests passed. После финального
  NUL/umask hardening повторно пройдены 20 targeted CLI/controller/permission
  tests и workspace all-targets clippy.
- **9.13c. Local stdio MCP transport (завершён).**
  Controller запускает primary/linked local entries через absolute Rust
  `mcp --project --config --state-root` argv. Remote entries получают
  собственные bearer tokens; local entries не требуют HTTP token.
  HTTP endpoints должны быть запущены заранее: launch не запускает servers.
  Реальный OpenCode 1.18.34 подключился через generated config;
  [live smoke](live-smoke.md) отделён от offline fixture evidence.

### 9.14. Linked-project routing

### 9.15. `add-project` dry-run/apply (не завершено)

- **Цель:** безопасное добавление проекта.
- **Source evidence:** `add_project.py:94-459`; `cli.py:382-421,865`;
  `tests/test_add_project.py:59-639`.
- **Содержание:** dry-run default writes nothing; apply = timestamped backup +
  atomic config write (same-dir temp, fsync, `os.replace`, preserve mode) +
  credentials (`O_EXCL`, 0600) + offline checks.
- **Критерии приёмки:** append-only byte-preserving config; state dir 0700.
- **Targeted checks:** add-project dry-run/apply/backup tests.

### 9.16. History prune gates (не завершено)

- **Цель:** управляемая очистка истории.
- **Source evidence:** `prune.py:27-279`; `cli.py:423-497,877-898`;
  `storage.py:158-162,4275-4472`; `tests/test_prune.py`.
- **Содержание:** `--older-than` required, `--apply`, `--include-failed`,
  `--vacuum` (requires `--apply`); terminal/age gates; dry-run read-only; active
  statuses never auto-deleted; WAL checkpoint always, VACUUM only explicit.
- **Targeted checks:** prune gate/atomic/vacuum tests.

### 9.17. Quarantine prune CLI (не завершено)

- **Цель:** crash-safe физическая очистка orphan worktree.
- **Source evidence:** `quarantine.py:55-860`; `cli.py:423-497,877-895`;
  `storage.py:3662-3929`; `tests/test_quarantine.py`.
- **Содержание:** `prune --quarantine --apply --confirm <ids|all:snapshot>
  [--purge] [--worktree]`; reviewed snapshot/entry ids; symlink/path/inode/
  locks/active protection; dry-run no writes; safe resume; fs effect first,
  durable transition last. Не автоматическая физическая очистка orphan
  recovery.
- **Targeted checks:** quarantine crash-resume/snapshot tests.
- **Зависит от:** 3.12e, 7.16d.

### 9.18. `deliver-task` (завершено)

9.18a–e подключены; проверки и ограничения записаны в итоговом ledger ниже.
Это CLI command, **НЕ** новый MCP tool.
`deliver-task --task T [--build|--dry-run|--apply]` только `accepted`; **no**
git staging/commit/ref moves/automatic apply и **no automatic rollback**.
Глобальной atomic multi-file транзакции нет (см. 9.18b/c).

- **9.18a. Artifact build/journal.**
  - **Цель:** byte-complete artifact/journal только для `accepted`.
  - **Source evidence:** `delivery.py:52-1121`; `cli.py:499-530,914-1002`;
    `tests/test_delivery.py`.
  - **Критерии приёмки:** только `accepted`; byte-complete.
  - **Targeted checks:** delivery artifact tests.
  - **Зависит от:** 7.16d. **Открывает:** 9.18b.
- **9.18b. Clean-main preflight и artifact drift.**
  - **Цель:** clean-main/HEAD/index/fingerprint/byte preflight; drift.
  - **Source evidence:** `delivery.py`; `worktree-execution-design.md` §5.5/5.6;
    `tests/test_delivery.py` (preflight fail-closed, drift).
  - **Критерии приёмки:** любое несоответствие до первой записи (dry-run/
    validation/conflicts/drift) → apply не начинается, основное дерево
    байт-в-байт неизменно (zero writes до apply); drift detected.
  - **Targeted checks:** delivery preflight/drift tests.
  - **Зависит от:** 9.18a. **Открывает:** 9.18c.
- **9.18c. Materializer, fsync и crash-safe resume.**
  - **Цель:** собственный materializer (не `git apply`/`checkout`/
    `update-index`) + `delivery_state='applying'`/operation journal + fsync +
    crash-safe resume.
  - **Source evidence:** `delivery.py`; `worktree-execution-design.md` §5.5/5.7;
    `tests/test_delivery.py` (`crash_at` injection после каждой операции).
  - **Содержание:** per-file atomic temp+rename; journal с точными base/artifact
    ожиданиями; на resume состояние каждого пути сверяется по exact state
    machine (base/artifact — продолжить, третье — fail-closed/
    `needs manual recovery`); `delivered` только после post-verify; index/refs не
    трогаются.
  - **Критерии приёмки:** crash во время apply может оставить частично
    материализованное/смешанное состояние; resume доводит его до byte-complete
    без blanket-гарантии «zero partial writes»; zero writes только для dry-run/
    отклонённого preflight/validation/conflicts **до** apply; автоматического
    rollback нет (ручной откат оператором).
  - **Targeted checks:** delivery crash-resume tests.
  - **Зависит от:** 9.18b.
- **9.18d. Writer-gate под admission lock.**
  - **Цель:** gate on any real/reserved active writer.
  - **Source evidence:** `storage.py:238-242,3512`.
  - **Критерии приёмки:** deliver blocked при active/reserved writer.
  - **Targeted checks:** delivery writer-gate tests.
  - **Зависит от:** 7.17d, 9.18a.

- **9.18e. Automatic build/apply wrapper (завершено).** Reuse существующего
  materializer: none — build artifact if missing + apply, applying — resume
  journal без rebuild, delivered — no-op. Accepted task/artifact/checkout
  остаются доступны после отказа, partial apply без rollback. Helper сам не
  берёт locks, caller держит lifecycle/admission. Source:
  `delivery.run_auto_delivery:1158-1205` (v17). Checks:
  `tests/test_delivery_on_accept.py`, existing crash injection. Depends on
  9.18c/d, 0B.4. Opens 8.19a.

### 9.19. Runtime status/diagnostics/hook/console (не завершено)

Не завершено. Разбито на независимые подзадачи.

- **9.19a. `status --json` и diagnostics.**
  - **Цель:** версионный readiness/diagnostics (`exit 1` если не ready).
  - **Source evidence:** `runtime.py:515-587`; `diagnostics.py:41-523`;
    `cli.py:369-376`; `tests/test_runtime.py:394-532`.
  - **Содержание:** safe phase `agent|verifying`/`verification_progress`;
    `DIAGNOSTICS_SCHEMA_VERSION=1`; full active set.
  - **Targeted checks:** runtime status-json tests.
  - **Зависит от:** 0A.6, 7.17c.
- **9.19b. Codex `hook-status` read-only.**
  - **Цель:** read-only fail-open Codex hook.
  - **Source evidence:** `hook_status.py:19-132`; `cli.py:1402-1433`;
    `tests/test_hook_status.py`.
  - **Критерии приёмки:** без raw prompt text; injection validation.
  - **Targeted checks:** hook-status tests.
  - **Зависит от:** 0A.6.
- **9.19c. `console`/`attach-opencode --task` routing.**
  - **Цель:** task-scoped console/attach routing.
  - **Source evidence:** `cli.py:549-574,1343-1433`; `worktree_runtime.py`.
  - **Критерии приёмки:** routing к task-scoped server/checkout.
  - **Targeted checks:** console/attach routing tests.
  - **Зависит от:** 7.16b.

### 9.20. OpenCode config migration helper (не завершено)

- **Цель:** эквивалент `scripts/migrate_opencode_config.py`.
- **Source evidence:** `scripts/migrate_opencode_config.py:68-933`;
  `docs/opencode-config-migration.md`; `tests/test_opencode_config_migration.py`.
- **Содержание:** `OPENCODE_CONFIG` env wiring; byte-preserved JSON/JSONC
  (verbatim copy, no re-serialization); env rules/`PROTECTED_ENV_NAMES`/exact
  0600; backups/path disjointness/symlink refusal/dry-run no writes/idempotency;
  `controller-opencode.json` forbidden. Actual config migration сейчас не
  делается. Python helper multi-file writes атомарны по файлу, **НЕ** общая
  transaction.
- **Targeted checks:** migration helper tests (dry-run/idempotent/JSONC/mode).

**Готовность потока:** совместимы argv, exit codes и безопасные ошибки.

## Поток 10. Tauri 2 + React/TypeScript/Vite foundation (не завершён)

Стек принят 2026-10-04, см. [решение](risks-and-decisions.md#десктопный-ui-tauri-2-react-typescript-vite).
Rust backend и ближайшая задача 7.14 сохраняются. GUI пока не реализован.
Полные settings/dashboard начинаются после успешного desktop/PTY prototype.

### 10.1. Tauri window и React/TypeScript/Vite scaffold

- **Цель:** минимальное desktop окно, frontend manifest/lockfile, Vite dev/build
  и Tauri configuration; отдельный headless CLI target без WebView dependency.
- **Приёмка:** dev и production assets открываются в Tauri на целевом Linux;
  WebView/system dependencies документированы; IPC/capabilities/CSP заданы явно.
- **Проверки:** frontend typecheck/build и desktop window smoke под Wayland/X11.

### 10.2. React application state и typed DTO/IPC contract

- **Цель:** project selection, navigation/loading/error states и typed Rust/TS
  boundary. Domain/services не зависят от Tauri/React.
- **Приёмка:** безопасные DTO без credentials; IPC inputs проверяет Rust,
  проект/session не перепривязываются произвольным frontend input.

### 10.3. Resizable terminal/dashboard split

- **Цель:** React layout с сохранением размера панелей и keyboard focus.
- **Приёмка:** resize не пересоздаёт terminal/session; empty/loading/error
  dashboard states видимы до подключения полноценного query service.

### 10.4. Tauri adapter к background Rust services

- **Цель:** узкие commands и channels поверх общих CLI/MCP/desktop services.
- **Приёмка:** SQLite/Git/HTTP/process waits выполняются вне UI thread;
  cancellation и bounded queues проверяются; generic SQL/shell API отсутствует.

### 10.5. Theme, fonts и accessibility baseline

- **Приёмка:** keyboard navigation, видимый focus, HiDPI/font sizing и
  статусы, различимые без цвета, проверяются в Tauri WebView.

### 10.6. Первый desktop/PTY prototype

- **Цель:** window + resizable terminal/dashboard split + настоящая PTY session.
- **Содержание:** xterm.js React adapter, Rust PTY input/output, resize, Unicode,
  large-output/backpressure, process exit/cleanup и window-close policy.
- **Приёмка:** настоящий процесс принимает ввод, корректно получает resize,
  вывод сохраняет порядок и не блокирует UI; session не теряется при rerender.
  Compatibility Codex/OpenCode подтверждается отдельно в 13.13/13.14.
- **Зависит от:** 10.1–10.4, минимальных 13.1–13.4/13.7/13.16; не требует
  завершения всех terminal features или backend parity.
- **Открывает:** полноценные settings/dashboard потоков 11/12.
- **Проверки:** real PTY integration и packaged Tauri smoke; mock UI не заменяет
  проверку IPC/WebView/PTY lifecycle.

## Поток 11. Настройки проектов

### 11.1. Read-only project list

### 11.2. Project edit form

### 11.3. Workspace picker

### 11.4. Endpoints и port validation

### 11.5. Permissions/trusted roots editor

### 11.6. Credential fields с redaction

### 11.7. Full-config validation

### 11.8. Change preview

### 11.9. Atomic save

### 11.10. Create/clone project

### 11.11. Setup/Doctor actions

### 11.12. Start/Stop actions

**Готовность потока:** проект настраивается без ручного редактирования TOML.

## Поток 12. Dashboard

### 12.1. Read-only task query service

### 12.2. Таблица одного проекта

### 12.3. Aggregation связанных проектов

### 12.4. Active/All filter

### 12.5. Search и project/status filters

### 12.6. Virtualized rows

### 12.7. Selection persistence по task ID

### 12.8. Task details

### 12.9. Verification/repository details

### 12.10. Periodic refresh

### 12.11. WAL notification optimization

### 12.12. Open/attach session action

### 12.13. Task cards: structured findings, checkpoints, workflow graph

- **Цель:** read-only отображение новых полей.
- **Source evidence:** `dashboard.py:174-443,1186-1695`;
  `tests/test_dashboard.py`.
- **Содержание:** task cards, parsed structured findings, per-round checkpoint
  (`parse_round_checkpoint`), read-only workflow graph (`collect_workflow_graph`).
  Depends on 7.13/7.15.
- **Targeted checks:** dashboard card/checkpoint/workflow tests.

### 12.14. Usage/budgets card

- **Цель:** read-only usage/budget/warning.
- **Source evidence:** `dashboard.py`; `usage.py:247-297`.
- **Содержание:** usage aggregation и budget/warning/exhausted gate.
  Depends on 7.14.
- **Targeted checks:** dashboard budget tests.

### 12.15. Parallel active set

- **Цель:** read-only safe full active set.
- **Source evidence:** `dashboard.py:174`; `diagnostics.py:351-367`;
  `storage.py:2182-2346`.
- **Содержание:** несколько active writers, reservations, waiting count.
  Depends on 7.17c.
- **Targeted checks:** dashboard parallel-writer tests.

**Готовность потока:** паритет с curses dashboard, UI read-only.

### 12.16. Delivery и automation run cards (не завершено)

- **Цель:** read-only delivery state/last refusal и run/step/control/blocker
  progress, distinct ready/completed.
- **Приёмка:** accepted не отображается как delivered; applying и unknown
  честные; private logs/credentials/path metadata не раскрываются.
- **Проверки:** card model fixtures. Depends on 8.19b, 16.9.

## Поток 13. Embedded terminal

### 13.1. xterm.js React adapter

Renderer выбран: xterm.js. Подключить terminal instance к DOM, определить
component/session lifecycle и проверить работу в Tauri WebView. Rust PTY crate
и транспорт stream уточняются spike; собственный cell renderer не планируется.

### 13.2. PTY spawn и exit

Rust создаёт PTY и управляет процессом; frontend получает opaque session
reference и safe lifecycle events. Project binding/ownership guards сохраняются.

### 13.3. PTY resize/SIGWINCH

Размеры xterm.js передаются в Rust через typed IPC; resize до ready и после exit
обрабатывается явно и не обращается к чужой session.

### 13.4. PTY stream через Tauri channels

Передавать byte chunks в xterm.js с сохранением порядка и incremental decoding.
Bounded buffering/backpressure реализуются на уровне bridge; поток не хранится
в React state. Проверить большой output, завершение stream и cancellation.

### 13.5. ANSI colors и cursor compatibility в xterm.js

### 13.6. Unicode, wide и combining glyphs compatibility

### 13.7. Keyboard input

xterm.js input передаётся через scoped IPC в Rust PTY; focus и key handling не
должны терять ввод или перехватывать управляющие клавиши TUI.

### 13.8. Scrollback

### 13.9. Selection и clipboard

### 13.10. Bracketed paste

### 13.11. Mouse reporting

### 13.12. Alternate screen

### 13.13. Codex launch profile

### 13.14. OpenCode launch profile

### 13.15. External-terminal fallback

### 13.16. Project switch/window close policy

**Готовность потока:** Codex/OpenCode compatibility suite проходит в Tauri
WebView с xterm.js и настоящим Rust PTY; input/resize/stream/cleanup проверены.

## Поток 14. Миграция

### 14.1. Подготовка переключения и остановка Python runtime

Python runtime останавливается; проверяется отсутствие живых locks и
PID/process records. Python state при этом не копируется, не читается и не
импортируется.

### 14.2. Создание нового Rust state

Rust инициализирует собственную пустую БД **fresh schema v15** и отдельную
историю задач; Python state не импортируется, не копируется и не переносится.
Rust-owned legacy v6 может быть поднят additive-миграцией до v15 только для
Rust-owned тестовых fixtures, а не как импорт Python runtime state.

### 14.3. Проверка изоляции и ownership marker

Раздельные state root, SQLite, locks, PID/ownership records, token-файлы, логи и
endpoints; fresh **schema v15**, `meta.runtime_owner='rust'` и sidecar marker
(ownership/format guard из 3.11 расширяется на v15); fail-closed при чужом
state. Исторические завершённые 3.10/3.11 были foundation schema v6 и
сохраняются как история, а не как финальный target.

### 14.4. Backup и независимый rollback rehearsal

Откат возвращает к неизменённому Python state; Rust state не переносится
обратно, а Python state не получает записей от Rust.

### 14.5. Один тестовый проект на Rust runtime

### 14.6. Crash/recovery drill

### 14.7. Soak period и метрики

### 14.8. Последовательный перевод проектов

### 14.9. Решение об архивировании Python

## Поток 15. Compatibility smoke и полная test matrix

Smoke evidence не заменяется mock-only тестами. Полная matrix запускается на
границе потока/в CI; внутри задач — targeted checks.

### 15.1. `smoke-opencode` direct

- **Source evidence:** `opencode_smoke.py:120-183,799-1639`;
  `tests/test_opencode_smoke.py`.
- **Содержание:** direct `OPERATION_ORDER` (health, workspace_binding, openapi,
  session create/adopt, exactly-once prompt, observation, permission
  detection/reply, question detection, reconnect_no_resend, model_usage,
  git_scope, shutdown), deterministic local mock backend, bounded report.
- **Критерии приёмки:** PASSED/OPENAPI_INCOMPATIBLE/RUNTIME_REGRESSION/
  ENVIRONMENT_LIMITED с точными exit codes.

### 15.2. `smoke-opencode --worktree`

- **Source evidence:** `opencode_smoke.py:820`; `tests/test_opencode_smoke.py`.
- **Содержание:**   `WORKTREE_OPERATION_ORDER` добавляет worktree_binding,
  verification_relative, runtime_files_outside, primary_copy_unchanged.
  Depends on 7.16b.

### 15.3. `smoke-opencode --parallel-worktrees`

- **Source evidence:** `parallel_worktree_smoke.py:1-1514`;
  `tests/test_parallel_worktree_smoke.py`.
- **Содержание:** config `execution_mode="worktree"`,
  `allow_parallel_writers=true`, `max_active_tasks=2`; два disjoint writers;
  third overlapping submission refused; independent accept; reservations
  released.
- **Критерии приёмки:** staggered startup + model rendezvous доказывают
  **concurrent execution**, не simultaneous start. Production worktree runtime
  сериализует START task servers через `runtime.lock`. Delta v17: bounded
  startup wait снижает кратковременные collision, timeout всё ещё может fail;
  текущий staggered smoke сам по себе не доказывает simultaneous startup.
  Добавить отдельный concurrent-start lock test. Depends on 7.17b, 9.7a.

### 15.4. Explicit local service lifecycle и полная CI matrix

- **Содержание:** явные lifecycle-команды `setup`/`doctor`/`start`/`stop`
  (существующие 9.2/9.3/9.7–9.10) и полный Rust workspace test/clippy, все v15
  fixtures verifier-ы, compatibility smoke direct/worktree/parallel.
  Запускается только на границе потока/в CI, не внутри каждой задачи.

### 15.5. Automation workflow proof (не завершено)

- **Цель:** synthetic multi-step Git workflow с deterministic model doubles,
  production verifier/lifecycle/materializer; отдельно optional live proof.
- **Приёмка:** crash после submit/revision/accept не дублирует операции;
  predecessor inheritance/current-step scope, stale review, failed checks,
  pause/stop/resume/limits и refusal/retry final delivery покрыты. Full real
  Codex+OpenCode run отмечается только при отдельном фактическом запуске.
- **Source:** `tests/test_automation.py`, `tests/test_codex_client.py` (v17).
- **Зависит от:** поток 16, 15.2, 0B.5.

## Поток 16. Approved automatic plan execution (v17, не завершён)

Source: `automation.py`, `automation_checkout.py`, `codex_client.py`,
`docs/automatic-mode-plan.md`, `docs/automatic-plan.example.json` (v17).
Один проект, последовательный topological order; отдельный existing task и
worktree на шаг. Approval — явный launch-codex --auto --plan, scope/criteria/
limits не расширяются по модельному ответу. Автоматизация принудительно
использует worktree + task delivery_mode=manual; plan delivery=apply|manual
управляет только итоговой доставкой. Никакого auto commit/push/deploy.

### 16.1. Approved plan validation (завершено)

- **Контракт:** version1/known keys, 1..100 safe unique steps, acyclic known
  dependencies, relative authorized scopes, безопасные nonempty step/final
  test commands, acceptance criteria/profile. max_seconds 1..604800,
  max_revisions 1..20, codex_timeout 1..3600; type bool вместо int запрещён.
- **Source:** `automation.validate_plan:41-155`.
- **Приёмка:** invalid plan отвергается до создания run/state; нормализованный
  порядок детерминирован. Checks: invalid-plan fixtures. Depends on 0B.5,
  2.12, path/command policies.

- **Реализовано:** `bridge-automation::plan::validate_plan`; strict typed JSON,
  known fields, bounded strings/lists/limits, relative symlink-aware scopes,
  existing command policy, profile lookup и stable topological order. Invalid
  input отвергается до initialization state. Все 30 plan cases из pinned
  `runtime-automation-v17.json` проходят; отдельно проверены null/type/size
  границы, reserved final id и symlink escape. Ошибки не раскрывают plan text.

### 16.2. Run creation/binding (завершено)

- **Контракт:** project/config hash/workspace/origin fingerprint фиксируются;
  clean main repo, no unfinished tasks и worktree repo support обязательны.
  Под automation + admission lock создаётся run и final integration step с
  union scopes/approved final commands.
- **Source:** `automation.create_run:240-291`, `_binding`.
- **Приёмка:** concurrent creation допускает один unfinished run; drift
  config/main блокирует дальнейшие шаги. Checks: create/binding/drift cases.
  Depends on 16.1, 3.14, 7.17a, 7.16a.

- **Реализовано:** `bridge-automation::run::create_run`, `AutomationLock` и
  read-only `check_binding`; order automation → admission, guarded Rust-owned
  state и existing unique unfinished slot. Run фиксирует config SHA-256,
  project/workspace, canonical main common dir и Rust repository snapshot
  (HEAD/index/status/manifest); это Rust-owned document, не импорт Python run.
  Final step получает sorted union scopes и approved final commands. Clean
  supported main repo, отсутствие unfinished tasks, отсутствие nested/external
  scope проверяются до insert; binding повторно проверяется перед записью.
  Проверены concurrent creation, busy locks, stale config, main/index/content
  drift, linked/nested repositories, foreign state и symlink lock refusal.
  Targeted `cargo test --offline -p bridge-automation` (10 tests, включая 30
  pinned plan scenarios) и all-targets Clippy проходят. CLI launch/coordinator,
  модельные вызовы и inheritance остаются в 16.3–16.10.

### 16.3. Read-only Codex process adapter (завершено)

- **Контракт:** prepare/review schema, `codex exec` read-only, ignore user
  config, multi_agent disabled; argv без shell, private schema/result/log,
  timeout/cancellation/process-group termination, bounded log/result.
  Ответы строго валидируются: accept без findings, request_changes с findings.
- **Source:** `codex_client.py:20-180`.
- **Приёмка:** malformed/nonzero/timeout/cancel не допускают accept; model
  output не меняет scope/criteria/permissions и не считается trusted evidence.
- **Checks:** subprocess doubles/structured output tests. Depends on 0B.5,
  process ownership primitives. No real model invocation в unit suite.

- **Реализовано:** `bridge-automation::codex`; local CLI argv сверены с
  `codex exec --help`, structured output — с official non-interactive docs.
  Prepare/review schemas, ignore-user-config/read-only/multi_agent disabled,
  private nofollow schema/result/log вне checkout, capped combined stdout/stderr
  (8MB), bounded result (1MB), timeout/cancel и whole-session cleanup.
  Проверены все 16 pinned answer cases, malformed/nonzero/oversized/symlink
  results, descendant cleanup и bounded log flood на subprocess doubles.
  Реальные model calls в тестах не выполняются.

### 16.4. Inherited accepted checkout (завершено)

- **Контракт:** parent accepted + same workflow/base/common repo; artifact
  owner/entries/fingerprint проверяются до/после inheritance. Symlink
  ancestors/target conflicts отвергаются; итоговый manifest совпадает с parent.
  Baseline нового checkout снимается после inheritance. Текущий step scope
  проверяется отдельно от cumulative artifact/delivery scope; prompt сохраняет
  inherited accepted files.
- **Source:** `automation_checkout.inherit_checkout:8-57`,
  `worker._resolve_worktree_context`, `prompts._git_rules`, `delivery.run_delivery`.
- **Checks:** inherited files/tampering/scope/Git baseline tests. Depends on
  7.16c, 9.18a/c, 0B.5.

- **Реализовано:** shared `bridge-artifact` (прежние byte-complete artifact и
  materializer primitives без lifecycle dependency), worker inheritance до
  baseline capture, accepted/project/workflow/base/common-dir proofs,
  pinned fingerprint/artifact owner/entries/blobs checks до/после копирования,
  scope/target/symlink refusal. Delivery для managed tasks строит cumulative
  artifact от original main snapshot; worker baseline остаётся post-inheritance.
  Delivery/inheritance tests и полный bridge-worker suite прошли; loopback
  integration tests запускались с разрешённым локальным networking. Targeted
  all-targets Clippy чист. Тесты покрывают binary/delete/mode/symlink и восемь
  вариантов tampering/refusal, cumulative delivery и отдельный current-step scope.

### 16.5. Detached coordinator lifecycle (завершено)

- **Контракт:** automation.lock + process record/run/project/workspace binding,
  pid/start identity и launch lease; private supervisor log вне checkout.
  Concurrent launch/resume отказывает до изменения состояния run.
- **Source:** `automation.project_lock:295-316`, `launch:752-799`.
- **Checks:** duplicate supervisor/stale identity/failed spawn/crash fixtures.
  Depends on 3.14, 9.6, 16.2.

- **Реализовано:** `bridge-automation::lifecycle`; detached session, exact
  automation-worker argv, private supervisor.log/process.json, pid/start/boot
  identity и run/project/workspace binding. Durable spawn record публикуется
  под automation lock; child получает lifetime lock с bounded startup wait.
  Duplicate resume отказывает до control/status write; failed spawn сохраняет
  blocked/paused state. Terminal/foreign records fail closed; stale identity
  допускает explicit resume. Delivery resume сохраняет config guard, main
  partial state принадлежит materializer journal. Automation suite (17 tests)
  и all-targets Clippy прошли. CLI consumer добавляется в 16.9.

### 16.6. Internal MCP provenance и managed revisions (завершено)

- **Контракт:** inherit_task_id/fingerprint/run id входят в internal submit
  hash/snapshot; public MCP wrapper их не принимает. Parent accepted/same
  workflow/worktree проверяется. request_changes постороннего caller для
  managed task → automation_managed; coordinator использует pinned run id.
- **Source:** `mcp_server.submit_task_impl:2359-2744`,
  `request_changes_impl:2898-2937`.
- **Checks:** parent mismatch/public-wrapper/hash replay/managed revision cases.
  Depends on 8.14, 8.17, 8.18, 16.2, 16.4.

- **Реализовано:** in-process MCP application adapter без transport/MCP lock;
  trusted typed run identity отделена от public JSON arguments. Persisted
  submit intent фиксирует prepared task, cumulative scopes, approved commands,
  profile/workflow/parent; parent accepted/workflow/base/fingerprint проверяются.
  Run/parent/fingerprint входят в hash и snapshot; public wrapper их отвергает.
  Managed revision требует running control, точный caller run, phase и pending
  revision intent; automation view принудительно worktree/manual с approved limit.
  Проверены replay, public bypass, changed scope/commands/profile/workflow,
  pause/stop и foreign revision caller. Submission suite (13), automation suite
  (20), MCP regression suite и targeted all-targets Clippy прошли. Existing
  partial-delivery test теперь требует exact simulated_crash вместо любого error.

### 16.7. Durable sequential coordinator (завершено)

- **Контракт:** prepare→submit→active→revise/accept→accepted, затем deliver.
  Intent/request id сохраняется до submit/revision; existing request replay
  закрывает crash-before-task-id-save. Bounded elapsed/revision/Codex retries;
  unresolved question/permission/server/limit/model error → persisted blocked.
- **Source:** `automation.Coordinator:410-750`.
- **Checks:** crashes at phase boundaries, no duplicate outbound prompt,
  revision limit/invalid review/blocker fixtures. Depends on 16.3, 16.5,
  16.6, 16.8, 7.10b, verifier integration 7.11.

- **Реализовано:** `bridge-automation::coordinator`; durable intents и replay
  submit/revision/accept, последовательные checkout, approved scope/commands,
  read-only prepare/review и independent acceptance. Elapsed budget сохраняется,
  retries ограничены тремя вызовами, pause/stop проверяются при model call.
  Четыре coordinator tests проверяют crashes после submit/revision/accept,
  отсутствие duplicate task/round/accepted event, model retry и control/time limits.
  Targeted tests и all-targets Clippy прошли. Delivery consumer — в 16.10.

### 16.8. Independent acceptance gate (завершено)

- **Контракт:** approved step scope, immutable HEAD/index, exact current round,
  current passed verifier before=after=current fingerprint, no side effects,
  all approved commands successful. Saved positive review of same fingerprint
  required; public accept тоже проверяет managed run/control/phase.
- **Source:** `automation.acceptance_checks:338-371`,
  `automatic_acceptance_error:374-407`, `mcp_server._accept_awaiting_review`.
- **Приёмка:** stale/changed/missing/failed evidence не принимает задачу;
  accept только в running/control=run. Checks: MCP bypass/stale review/scope.
  Depends on 16.6, 7.11, 8.7. Opens 16.7.

- **Реализовано:** `bridge-worker::acceptance`; task/worktree/base/runtime/
  workflow binding, current-step scope, immutable HEAD/index, exact completed
  round и authoritative done/passed verifier before=after=current. Approved
  commands должны совпасть целиком и иметь exit=0 без timeout/side effects.
  Positive review связывает exact round/fingerprint; running/control=run и
  persisted accept phase обязательны. Public MCP использует тот же gate;
  последний guard вызывается внутри IMMEDIATE acceptance transaction. Plain
  storage acceptance не принимает managed task без explicit guarded API.
  Два новых tests покрывают 12 invalid evidence/control вариантов и idempotent
  success с реальным verifier; storage/manual/on_accept regressions и Clippy прошли.

### 16.9. CLI controls и recovery (завершено)

- **Контракт:** launch-codex --auto --plan, automation-status/pause/resume/stop,
  private automation-worker. Plan file bounded 1MB; --plan требует --auto.
  Pause сохраняет текущую фазу; resume явный после blocker/crash, stop
  завершает existing tasks через cooperative close. Coordinator cancellation
  прерывает read-only Codex call; terminal run не resume/не control.
- **Source:** `cli.py:575-591,1452-1526`, `Coordinator.tick/run`.
- **Checks:** detached terminal exit, control during review/startup, stop
  paused/blocked run, cooperative task close. Depends on 16.5, 16.7, 7.12.

- **Реализовано:** CLI `launch-codex --auto --plan`, metadata-only status,
  pause/resume/stop с optional exact run UUID, private automation-worker и
  повторно используемый detached worker spawner. Bounded no-follow plan reader
  не создаёт state при invalid input. Stop запускает cleanup supervisor для
  paused/blocked run, сохраняя stop control, и закрывает orphan submission intent.
  CLI tests проверяют flags, 1MB bound, readonly status, private worker identity,
  detached stop и terminal controls; coordinator tests — cancellation во время
  model call и cooperative orphan close. Реальный Codex в тестах не вызывается.

### 16.10. Final verification/delivery

- **Контракт:** final integration task проходит тот же verifier/review gate;
  accepted cumulative artifact относительно original main. Final fingerprint
  проверяется; existing materializer/apply под admission lock с crash resume.
  plan delivery=manual → ready, completed только после durable delivered.
- **Source:** `Coordinator._deliver:686-704`, `delivery.run_delivery`.
- **Checks:** final failure/drift/partial apply/retry/manual-ready cases.
  Depends on 16.7, 9.18c/d, 16.4. Opens 15.5, 12.16.

## Ограничения Python и честный статус

Ниже — граница между **fidelity contract**, **limitation** и **optional future
improvement**. Limitation не выдаётся за реализованное исправление.

- **Worktree external repos/submodules/LFS/sparse/nested/environment setup и
  cherry-pick/merge delivery отложены.** External absolute `allowed_paths` в
  worktree запрещены (`mcp_server.py:2259-2271`); submodules/LFS/sparse fail
  closed (`git_worktree.py:455-497`); env/dependency setup и
  cherry-pick/merge — `docs/worktree-execution-design.md` §9 «Инкремент 3
  (отложенный)» (`:1238-1262`).
- **A2A/full patch archival/auto merge/commit/push/deploy остаются deferred.**
  Прежнее «auto scheduler/auto-accept/auto-apply отсутствуют» относится к
  frozen v15: current v17 реализует approved sequential coordinator и v16
  on_accept delivery. Это opt-in, а не безусловный общий background scheduler;
  `deliver-task` по-прежнему отдельный CLI, не public MCP tool.
- **Production startup serialization.** Current v17 manager lock имеет
  bounded wait для worktree startup (60s); обычные start/status/stop остаются
  nonblocking. Staggered parallel smoke доказывает concurrent execution,
  concurrent-start lock tests — очередь startup, не simultaneous spawn.
- **Proof boundary.** Новые `test_delivery_on_accept`, `test_automation` и
  `test_codex_client` проверяют refusal/crash/recovery/model doubles. Python
  docs сообщают live Codex prepare и OpenCode worktree smoke; full workflow
  с обеими реальными моделями на пользовательском проекте не заявлен и в
  этой source-сверке не запускался. 0B.4/0B.5 исполнили выбранные isolated
  source scenarios и независимые expectations; это не live-model evidence.
- **Historical defaults и isolation не ослабевают:** `max_active_tasks=1` и
  `allow_parallel_writers=false` остаются default; `execution_mode='direct'`
  default; Python/Rust state не смешиваются.

## Порядок и готовность

1. **Завершённый baseline:** 0A, domain1.6/1.7, config2.10–2.12,
   storage3.12a–f по frozen v15. **0B.1 завершён** (v17 manifest).
   **0B.2 завершён** (87 config/permission cases).
   **0B.3 завершён** (4 SQLite delta fixtures и migration parity).
   **0B.4 завершён** (47 MCP delivery/recovery/claim scenarios).
   **0B.5 завершён** (77 runtime/automation cases).
   **7.13 завершён**; **7.14/7.15/8.18/7.18 завершены**; delta fixtures refresh завершён.
2. После завершённого worktree service блока 7.16 продолжить writer locks 7.17 и remaining
   consumers existing v15 (7.17, 8.12–8.17, 9.15–9.20, 12.13–12.15). Новые delivery/config/
   schema17 foundations 1.8, 2.13–2.14, 3.13–3.15 выполняются по dependencies;
   recovery corrections 7.10/8.20 и startup wait 9.7a включены в свои потоки.
3. On_accept 9.18e/8.19 зависит от existing crash-safe materializer и locks;
   автономный поток16 — от worktree/lifecycle/verifier/MCP/delivery. Не
   подменять эти prerequisites одним большим automation PR.
4. **Modern v7–v17 parity не завершён.** Rust runtime/storage target остаётся
   v17 после 3.13c; завершённые задачи не переименовываются в v17-реализацию.
   Question blocker service 7.7 завершён; следующий historical шаг — 7.8;
   для затронутых новым source контрактов сначала соответствующие 0B fixtures.

## Экономия контекста

- В задачу включаются только нужные fixtures и раздел документа.
- Отчёт содержит результат, файлы, checks и следующий dependency без пересказа
  всей архитектуры.
- Полная test matrix запускается на границе потока или в CI; внутри задачи —
  targeted tests и необходимые regressions.
- Рефакторинг общего слоя выполняется отдельной задачей до потребителей.
- Независимые потоки параллельны только после стабилизации общего контракта.
- Решение, влияющее на будущие задачи, фиксируется коротким ADR.

## Общий Definition of Done

- Python и Rust делят только канонический `projects.toml` для чтения (запись —
  от активной реализации при остановленном runtime, как принято в
  `docs/risks-and-decisions.md`); runtime state, SQLite/история, tokens, logs,
  locks/process records и endpoints раздельные. Rust ведёт fresh Rust-owned
  schema v15 и отдельную историю (additive upgrade — только Rust-owned legacy
  v6), Python state без импорта и без конвертации;
- MCP и CLI контрактно совместимы;
- crash recovery покрывает промежуточные статусы;
- security/verifier suites проходят;
- GUI настраивает проекты без раскрытия секретов;
- Codex/OpenCode работают во встроенном PTY;
- dashboard отзывчив и соответствует storage;
- установка и rollback документированы.

### 9.5c3. Safe MCP error context and revision-limit parking

- Implemented: public validation categories for flags/request/text, command index/reason and deduplicated secret categories; review/budget error status context. Revision limit atomically parks in needs_user with one event and no new round.
- Validation: 28 existing MCP regressions passed; new safe-error/limit scenario passed after fixture correction; four storage lifecycle checks including rollback on event failure passed.
- Remaining error parity: structured findings details, dirty scope evidence and corrupt-budget revision handling.

### 8.14a. Workflow submission and explicit activation

- Implemented: structural workflow normalization, atomic task/round/profile/checkout metadata, request hash/replay before referential probes, owned mode=ro dependency reads, linked-root validation, bounded transitive cycle check, source-compatible workflow/workflow_gate status fields. Explicit task_status activates through admission/lifecycle fences; startup leaves waiting tasks parked.
- Validation: full MCP 31 checks passed before final payload-key correction; three final workflow scenarios passed (local gate/replay/cycle + linked read-only/unlinked refusal); submission 13 and storage dependency 10 checks passed; targeted Clippy clean.

### 9.18a/b and 7.17d/9.18d service foundation

- Implemented `bridge-delivery`: accepted-only byte-complete manifest/blobs, binary files, delete, executable modes and symlink targets; bounded read-only commit blob reader; full ordered artifact/checkout revalidation and clean-main/ancestry/exact-base preflight. Artifact files are private and atomically fsynced. Admission remains held across writer-status/ledger and lifecycle/task-lock checks, independent of parallel-writer config downgrade.
- Checks: three integration scenarios passed (five object operations; dirty/drift/blob corruption/outside scope with unchanged main; accepted/writer reservation refusal before artifact writes); targeted Clippy clean.
- Remaining: journal/materializer/resume, CLI wiring, automatic on_accept wrapper.

### 9.18c. Journal, materializer and deliver-task CLI

- Implemented: fsynced ordered journal before applying; per-file atomic writes, raw symlink targets, deletes and mode changes; exact base/artifact resume independent of advisory applied markers; fixed third-state refusal without rollback; HEAD/index and complete final output verification before durable delivered. Dry-run uses the same resume preflight. `deliver-task --task T [--build|--dry-run|--apply]` defaults to validation and emits structured reports.
- Checks: six delivery integration scenarios passed, including thirteen injected durable-boundary interruptions and resume, third-state refusal/unchanged target, applied-marker revert/reapply, missing artifact and symlink parent refusal; CLI build/default validation/apply/repeat scenario passed; targeted Clippy clean.
- Limits: files are individually atomic, not an atomic multi-file transaction; interruption may leave mixed base/artifact paths; third states require operator recovery.

### 9.18e / 8.19. Frozen on-accept delivery

- Implemented automatic build-if-missing/apply/resume through the existing materializer; delivered repeats read saved state without locking or probing OpenCode. Project worker → task → admission lock order; admission held before stop/accept and through delivery. Accepted/event/reservation release are atomic; post-accept delivery refusal preserves accepted status and records a safe latest attempt. Status exposes pending/refused/applying/delivered and actual persisted state; private paths are never emitted.
- Checks: six MCP on-accept scenarios passed (first/repeated delivered, busy first accept preserves review, dirty-main refusal and retry without endpoint, partial journal resume, lost-ledger real writer gate, artifact I/O refusal and frozen policy despite manual current config); four storage lifecycle checks and targeted Clippy passed.

### 9.5c4. Frozen validation envelopes and one-round budget override

- Implemented safe structured-finding index/details, budget validation details, proven dirty/scope evidence and worktree absolute-path refusal fields. Budget error uses the source `budget` block. A corrupt budget blocks revision; explicit override records an exact current-revision audit in the same transaction as round creation. Only that audited round may execute with corrupt optional budget data; raw budget remains unchanged and the next revision is gated again. Other task fields remain strictly validated. Needs-user revision returns its persisted diagnostics.
- Checks: 29 frozen source validation envelopes passed without skips; 33 MCP regressions and 296 storage checks passed; atomic audit rollback and current-round-only authorization checked separately; targeted Clippy clean.
- Intentional security difference: transport/infrastructure errors retain fixed redacted labels instead of embedding Python's raw exception or foreign endpoint/path text. No source-wide error-string identity is claimed.

### 9.13c. Local stdio controller delegation

- Enabled controller local entries after standalone handlers/worker recovery/workflow/delivery integration. Absolute Rust executable/config/state argv is retained for primary and linked entries. Credentials are injected only for remote MCP entries; inherited MCP tokens and executor HTTP credentials are scrubbed while provider environment survives.
- Checks: ten controller runtime checks passed; nine CLI launch checks passed, including generated local entry starting the actual Rust MCP, initialize/tools-list/project-info and six delegated tool schemas. No real provider evidence is inferred from these fixtures.

### 9.18c follow-up: byte-exact Git index preservation

Read-only Git runner disables optional Git locks so `status` cannot rewrite the index stat cache. Delivery crash/resume checks now compare raw `.git/index` bytes as well as HEAD/index fingerprints. Six delivery scenarios (including all thirteen crash boundaries) and four bounded-runner checks passed.

### Итог согласованного блока — 2026-10-05

Recovery delivery_unknown/failed assistant, frozen status/error envelopes,
workflow/dependency activation, delivery artifacts/preflight/materializer/CLI,
on_accept и local stdio controller завершены и закоммичены по этапам. Исторические
записи «Remaining» в ledger описывают состояние на момент соответствующего
коммита; последующие записи закрывают эти зависимости.

Полный `cargo test --workspace` прошёл; после byte-exact index hardening
повторно прошли шесть delivery и четыре bounded Git runner checks. Targeted
all-targets Clippy чист. Реальный OpenCode/provider smoke прошёл один attempted
round, verifier, on_accept delivery и idempotent repeat; реальный OpenCode
подключился к local Rust MCP из generated controller config.
Подробные условия и границы evidence: [live-smoke.md](live-smoke.md).

Оставшиеся независимые потоки: расширенная live matrix 15.x (parallel, linked
projects, recovery/crash), runtime diagnostics/maintenance CLI 9.15–9.20,
automatic plan coordinator 16.x и GUI. Smoke одного worktree task не закрывает
эту матрицу и не доказывает полную parity со всем Python runtime.
