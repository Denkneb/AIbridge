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
