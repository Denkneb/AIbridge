# Runtime/readiness fixtures v15 (0A.6)

`fixtures/runtime-cases.json` фиксирует reference schema v15 и
`DIAGNOSTICS_SCHEMA_VERSION=1` от read-only Python HEAD
`86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a`.

29 cases проверяют safe phase `agent|verifying`, фильтрацию verifier progress
до `state`/`command_index`/`command_count`, diagnostics на missing/empty/unsupported
БД, full active-writer set с waiting count, accepted worktree summary и
readiness `status --json` с exit codes. Hook проверяется через настоящий
`cmd_hook_status`: точный context, parallel routing, fail-open на missing,
corrupt/unsupported state, injection/invalid timestamp и отсутствие context
для terminal task. Raw task text не попадает в вывод.

```bash
python3 docs/fixtures/runtime/verify.py
```

Verifier копирует только committed `empty-v15.sqlite` в временный каталог и
добавляет синтетические rows. Python runtime DB/history не читаются. Проверяются
HEAD/schema и отсутствие изменений в файлах/каталогах при read; transient
SQLite `-wal`/`-shm` исключены. Hook не вызывает storage initialization или
socket connect. Reference bytecode отключён.

Snapshot expectations — явные проекции стабильных полей; age и размеры БД не
замораживаются. Hook context и JSON envelopes проверяются точно. Readiness
health и результаты наблюдения process records заменены детерминированными
doubles; report construction, diagnostics SQL и worktree summary исполняются
из reference source. Это контрактные fixtures, не live-service smoke.

Публичные compact `task_status` phase/progress и fallback command count уже
покрыты target-v15 MCP corpus (`task-status-compact-phase-agent` и
`task-status-compact-phase-verifying`). Rust runtime этим refresh не меняется.

## Runtime/automation delta v17 (0B.5 завершён)

Отдельный corpus — [fixtures/runtime-automation-v17.json](fixtures/runtime-automation-v17.json),
verifier — [fixtures/runtime/verify_v17.py](fixtures/runtime/verify_v17.py).
Reference HEAD `e52a46158cbeb4f3ae35063d395c05ea0ce144bc`, schema17.
Historical runtime corpus/verifier сохраняют v15 pin. Rust implementation и
его target v15 не меняются.

**77 cases без skips:** 30 plan fixtures, 16 prepare/review answer fixtures,
1 runtime bounds fixture, 1 detached launch lease с fake child identity,
1 durable blocked run с unavailable model double и 28 selected source integration
scenarios (16 automation, 3 runtime manager-lock, 5 worktree startup, 4 adapter).
Corpus содержит независимые input/expectations, а не захваченные результаты.
JSON детерминирован, ids sorted/unique; generated UUID/PID/time/port не замораживаются.

Plan fixtures проверяют defaults и upper bounds, strict integer versus boolean,
unknown fields/version, goals/tasks/criteria, relative scope/traversal, unsafe
commands, step identities, topological ordering, cycles/self/unknown dependencies,
unknown profiles, final checks, delivery mode и model name. Valid cases вызывают
actual `validate_plan`; invalid — actual `create_run` и обязаны отвергаться **до
создания SQLite/state**. Bytes уже созданных synthetic config/workspace files
сравниваются до/после. `base_plan` хранится в corpus; `changes` применяются по
путям вида `steps.0.allowed_paths`.

Answer fixtures вызывают actual `validate_answer`: prepare принимает ровно task,
review — decision/summary/findings; unknown/extra fields, empty/type/length
violations и inconsistent accept/request_changes отвергаются. Long strings
заданы компактно `{repeat_text,count}` и раскрываются с ограниченным размером.
Проверяется точный valid object или стабильная error category/substring.

Integration runner использует **actual pinned source pytest scenarios**;
независимые expectations дополнительно проверяют persisted status/phase,
task/round inventory, delivery states и engine call counts, runtime start/lock
success/error counts, число distinct ports/stand-in children и adapter result/
exception. Source assertions отдельно проверяют filesystem, inherited baseline,
review fingerprints, точную последовательность engine calls и cleanup.
Это 28 выбранных сценариев, а не запуск всей Python suite.

Покрытые automation сценарии:

- fix→review revision→accept→consumer с наследованием accepted checkout→final
  integration→delivery; main HEAD/index сохраняются;
- resume из submit/active/accept и crashes после submit/revision/accept не
  создают повторные tasks/rounds; durable intents сохраняются;
- current-step scope уже cumulative delivery scope; main drift и predecessor
  tampering запрещают дальнейшее выполнение;
- failed verification запрещает acceptance; fingerprint binds review к actual
  checkout/round (managed-action refusal дополнительно покрыт 0B.4);
- delivery refusal сохраняет accepted result для retry; pause/resume сохраняет
  progress; stop без dispatched tasks даёт stopped; второй run/coordinator запрещён;
- manual delivery завершает run как **ready**, final accepted worktree остаётся
  недоставленным; **completed** — только после delivered final result;
- launch записывает process binding/lease, repeated launch/resume с live fake
  identity не создаёт второй child; `_spawn_process`/`_identity` здесь doubles,
  project lock, process-record write и validation — actual source;
- три failed prepare attempts дают durable `blocked`/`CodexError`, tasks не
  создаются. Только retry sleep заменён; run/persistence/locking реальны.

Runtime defaults: readiness 20s, worktree startup manager wait 60s, обычный
manager lock `wait=0`. Проверены default immediate refusal, bounded wait после
release, timeout без spawn/record/port и release частично acquired multi-root
locks. Три concurrent task starts имеют три distinct ports и корректные live
records; два starts одного task — один child/record. Короткий startup критический
участок сериализуется; после него несколько stand-in children остаются живыми
одновременно. Это не доказательство simultaneous spawn или real model concurrency.

Adapter scenarios используют actual `CodexClient.call`, но `codex` — **созданный
pytest script под temporary directory**. Guard проверяет его путь и shebang,
readonly sandbox, ignore-user-config, disabled multi_agent и synthetic
workspace/schema/output paths. Проверены structured prepare result/private file
permissions, timeout/cancellation с завершением process group и nonzero exit.
Имя source test упоминает invalid JSON, но его тело в этом pin проверяет code7;
runtime malformed-output coverage этим конкретным сценарием не заявляется.
Strict object/answer rejection проверяется отдельно golden fixtures.

Все config/password files, Git repos/worktrees, SQLite, artifacts, locks и
process records создаются в собственном temporary directory. SQLite/Git cwd
проверяются на этот каталог; socket **connections** запрещены. Реальные модели,
OpenCode server и detached coordinator subprocess не запускаются. Разрешённые
subprocesses — local Git, synthetic `python3 -B module.py|consumer.py`/`false`,
known Python sleep stand-ins и temporary fake Codex executable. Stand-in children
учитываются, а любой оставшийся child завершается в finally. Worktree lifecycle
проверяет настоящие PID identity/pidfd/stop paths на sleep children. Локальный
socket bind используется только для проверки доступности порта, без server/network
connection; environment должен разрешать эти probes и pidfd primitives.

```sh
python3 docs/fixtures/runtime/verify_v17.py
```

В restricted sandbox, запрещающем создание socket, этот набор требует отдельного
разрешения на запуск вне sandbox: 3 worktree tests сначала упёрлись в socket
`PermissionError`, затем разрешённый isolated rerun прошёл **77/77**. Ограничение
среды не маскируется skip или подменой port allocation. Pytest plugins/cache и
bytecode отключены. Bootstrap/source pin/strict JSON helpers переиспользуются из
0B.4 verifier; historical verifier не импортируется. HEAD/clean-tree и hashes
corpus/SQLite проверяются до/после, в том числе при отрицательном запуске.

Шесть negative harness checks на temporary corpus копиях отвергли stale pin,
duplicate id, неверный plan default, неверный startup wait, попытку разрешить
accept с findings и ложный completed вместо manual ready. Manifest и historical
SQLite verifier также прошли. Rust tests не перезапускались: Rust-код не менялся.

### Граница live evidence

Источник отчёта о live проверках — **Python `docs/automatic-mode-plan.md:67–73`**
на том же pin (README ссылается на этот документ). Он сообщает отдельную read-only
Codex prepare проверку и OpenCode worktree smoke. Полный workflow с review/revision
там проверен на real Git и production lifecycle/verifier/delivery с deterministic
model answers; совместный запуск обеих реальных моделей на пользовательском
проекте не выполнялся. Здесь выполнен только описанный isolated corpus. Ни live
smoke, ни real-model workflow в 0B.5 не запускались, современный Rust parity не
заявляется. Delta fixture refresh 0B завершён; следующий шаг — **7.13**.
