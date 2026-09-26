# Существующий внешний контракт `agent_bridge`

Этот документ фиксирует внешний контракт уже реализованного Python-проекта
`/home/denis/Python/agent_bridge` как базу для Rust-миграции. Он не описывает
целевую архитектуру и не является планом: единственный машинно-читаемый
источник — [contract-manifest.json](contract-manifest.json).

## Назначение

Контракт нужен, чтобы будущие Rust-задачи опирались на фактическое поведение
Python-кода, а не на README или догадки. Manifest намеренно детерминирован:
без timestamps, секретов, реальных tokens/passwords и нестабильных
task/session/message ID. JSON-ключи отсортированы; массивы отсортированы там,
где порядок не является частью контракта.

## Структура manifest

| Раздел | Содержимое |
| --- | --- |
| `manifest_version`, `source` | версия формата manifest и версия Python-пакета |
| `scope` | что зафиксировано как совместимый контракт и что намеренно не зафиксировано |
| `cli` | имя программы, точка входа, общие options, команды, exit codes |
| `mcp` | имя сервера, шесть tools с параметрами, transports, коды ошибок |
| `domain` | task/round статусы, доказуемые переходы, recoverable failure, worker error codes |
| `storage` | `PRAGMA user_version`, migrations, tables, columns, indexes, invariants |
| `config` | расположение `projects.toml`, ключи проектов и правила валидации |
| `paths` | config/state/runtime пути и файлы состояния |
| `runtime` | entry points, locks и process ownership records |
| `opencode_api` | HTTP API OpenCode, от которого зависит adapter |
| `environment` | имена service env vars и список tuning overrides (не контракт) |

## Источники

Manifest построен по фактическому коду и тестам Python-репозитория, в первую
очередь:

- `src/agent_bridge/cli.py` — CLI-команды, exit codes, wiring Codex/OpenCode;
- `src/agent_bridge/mcp_server.py` — шесть MCP tools, параметры и коды ошибок;
- `src/agent_bridge/mcp_http.py` — authenticated Streamable HTTP transport;
- `src/agent_bridge/storage.py` — schema version, tables, indexes, invariants,
  статусы и атомарные переходы;
- `src/agent_bridge/worker.py` — task/round transitions, worker error codes;
- `src/agent_bridge/config.py` — `projects.toml` keys и правила валидации;
- `src/agent_bridge/runtime.py` — start/status/stop, locks, process records;
- `src/agent_bridge/opencode_client.py` — обязательные операции OpenCode API;
- `tests/` — подтверждение наблюдаемых кодов ошибок и переходов.

Поле `evidence` у каждого перехода ссылается на модуль и функцию, а не на номер
строки, чтобы ссылка не устаревала при рефакторинге.

## Публичные границы контракта

Совместимыми считаются:

- имена и опции CLI-команд, а также exit codes;
- имена, параметры и коды ошибок шести MCP tools;
- stdio и authenticated HTTP transports и runtime entry points;
- статусы task/round и доказуемые переходы между ними;
- расположение SQLite, `PRAGMA user_version`, tables, ключевые columns,
  indexes и invariants;
- расположение `projects.toml`, ключи проектов и правила валидации;
- config/state/runtime пути;
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
  manifest как `implementation_overrides_not_frozen`).

## Ограничения

- Документ не включает секреты, реальные tokens/passwords и timestamps.
- Неизвестные или не доказанные значения в manifest помечены явно; переходы,
  которые не сохраняются в storage (например, промежуточный `observing` при
  recovery `needs_user`), в списки не добавлены.
- Rust-код в рамках задачи 0.1 не реализуется.
