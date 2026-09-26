# Архитектура

## Принципы

1. Доменная логика не зависит от UI, CLI и MCP transport.
2. CLI, MCP и GPUI используют один сервисный слой.
3. Привязка процесса к project, workspace и endpoints неизменяема.
4. Worker остаётся отдельным процессом для изоляции и recovery.
5. Переходы состояния проверяются моделью и выполняются транзакционно.
6. Security-critical код сохраняет fail-closed семантику Python-версии.

## Общая схема

```text
GPUI ────────────┐
CLI ─────────────┼── application services ── domain
MCP stdio/HTTP ──┘            │                 │
                              ├── SQLite        │
                              ├── OpenCode HTTP │
                              ├── Git/processes │
                              └── worker ───────┘
```

## Cargo workspace

```text
crates/
  bridge-domain/       Task, Round, статусы и переходы
  bridge-config/       projects.toml, credentials, project env
  bridge-storage/      SQLite schema, migrations, repositories
  bridge-opencode/     HTTP client и OpenAPI compatibility
  bridge-git/          snapshots, allowed paths, repository discovery
  bridge-policy/       shell и permission policy
  bridge-verifier/     commands, fingerprints, side effects
  bridge-worker/       implementation, revision, recovery
  bridge-mcp/          stdio и authenticated HTTP MCP
  bridge-runtime/      locks, pidfd, start/status/stop
  bridge-terminal/     PTY и terminal model
  bridge-ui/           GPUI views и application state
  agent-bridge-cli/    итоговый бинарник
```

Это границы ответственности, а не обязательное число crates: пакеты можно
объединить, если разделение не улучшает независимое тестирование.

## Доменная модель

Статусы задаются закрытым `enum`:

```text
implementing -> awaiting_review -> revising -> awaiting_review -> accepted
       |               |              |
       +----------> needs_user <-------+
       +----------> failed
       +----------> delivery_unknown
```

`Task`, `Round`, `Verification`, `RepositorySnapshot` и `UserAction` получают
типизированные поля и версионируемую сериализацию. Недопустимый переход должен
отклоняться до записи в SQLite.

## Хранилище

- Сохраняются путь и формат `state.sqlite`.
- Сохраняются WAL, foreign keys, busy timeout и `BEGIN IMMEDIATE`.
- `PRAGMA user_version` меняется только отдельной миграцией.
- Идемпотентность обеспечивается `request_id` и хешем payload.
- GUI читает SQLite через `bridge-storage`, а не через MCP.
- Один проект имеет не более одной незавершённой задачи.

## Процессы

Worker запускается подпроцессом того же бинарника:

```text
agent-bridge worker --project ID --task TASK_ID --round N
```

`start/status/stop` сохраняют process ownership records, Linux start time,
pidfd и locks. Остановка не затрагивает вручную запущенные процессы.

## OpenCode и MCP

OpenCode adapter изолирует health/OpenAPI checks, sessions, `prompt_async`,
messages, permissions, questions и one-time replies.

Сохраняются MCP tools:

- `project_info`;
- `submit_task`;
- `task_status`;
- `request_changes`;
- `accept_task`;
- `close_task`.

Поддерживаются stdio и authenticated Streamable HTTP на localhost. Compact и
`verbose=true` ответы, `wait_seconds <= 300`, recovery side effects и ранний
возврат при смене статуса должны быть совместимы.

## Security boundaries

- canonical paths и symlink confinement проверяются перед доступом;
- абсолютные allowed paths допустимы только в доверенных внешних Git repos;
- недоказуемо безопасный shell-синтаксис отклоняется;
- `git add`, `git commit`, `git push` не разрешаются автоматически;
- секреты не попадают в argv, UI, SQLite, логи и ошибки;
- project/workspace/endpoint нельзя изменить MCP-аргументом.
