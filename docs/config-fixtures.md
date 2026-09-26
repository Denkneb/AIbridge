# Fixtures конфигурации `agent_bridge`

Машиночитаемый corpus: [fixtures/config-cases.json](fixtures/config-cases.json).
Это контрактные fixtures для будущих Rust-задач потока 2 (конфигурация); они
описывают наблюдаемое поведение существующего Python-кода, а не целевую
реализацию. Rust здесь не реализуется.

Corpus намеренно детерминирован: без timestamps, секретов, реальных
tokens/passwords и абсолютных machine-specific путей. JSON-ключи отсортированы,
`cases` отсортированы по `id`. Все внешние пути заданы документированными
placeholder-ами.

## Источники

Corpus построен по фактическому коду и тестам Python-репозитория
`/home/denis/Python/agent_bridge` (только чтение), в первую очередь:

- `src/agent_bridge/config.py` — `projects.toml`, правила валидации, linked discovery;
- `src/agent_bridge/project_env.py` — чтение `opencode_env_file`;
- `src/agent_bridge/credentials.py` — семантика credential-файлов;
- `tests/test_config.py`, `tests/test_mcp_http.py`, `tests/test_project_env.py`.

Версия-источник зафиксирована в поле `source.commit`. Python-репозиторий не
изменялся.

## Schema corpus

Верхний уровень:

| Поле | Назначение |
| --- | --- |
| `corpus_version` | версия формата corpus |
| `source` | commit Python-репозитория и список модулей/tests |
| `placeholder_conventions` | словарь placeholder → описание подстановки |
| `error_categories` | стабильные идентификаторы категорий ошибок и их смысл |
| `cases` | упорядоченный по `id` список case-ов |

Каждый case:

| Поле | Обязательность | Назначение |
| --- | --- | --- |
| `id` | всегда | стабильный идентификатор case |
| `input_kind` | всегда | `projects_toml` или `opencode_env_file` |
| `operation` | всегда | `load_all_projects`, `load_project`, `load_linked_projects` или `load_project_env` |
| `rule` | всегда | стабильный идентификатор проверяемого правила |
| `expectation` | всегда | `valid` или `invalid` |
| `description` | опционально | краткое пояснение ветви |
| `project` | для `load_project`/`load_linked_projects` | запрашиваемый project id |
| `toml` | для `projects_toml` | минимальный TOML template |
| `content` / `content_hex` | для `opencode_env_file` | содержимое env-файла (hex — для не-UTF-8 байтов) |
| `setup` | для `opencode_env_file` | `kind` (`regular`/`symlink`/`directory`/`missing`), `mode`, `owner` |
| `error_category` | для `invalid` | стабильная категория из `error_categories` |
| `expect` | для `valid` | ожидаемые нормализованные значения (только проверяемые поля) |

`expect` не повторяет весь `ProjectConfig`: он содержит только поля, на которые
направлен case. Для `load_linked_projects` это `linked_project_ids`; для
`load_project_env` — `parsed`.

## Placeholder conventions

Полный список — в `placeholder_conventions`. Ключевые:

- `${CONFIG_DIR}` — каталог, содержащий `projects.toml`;
- `${WORKSPACE}`, `${OTHER_WORKSPACE}`, `${PARENT_DIR}` — существующие каталоги;
- `${WORKSPACE_LINK}`, `${TRUSTED_LINK}` — symlink на канонический target;
- `${TRUSTED_DIR}` — существующий trusted root;
- `${MISSING_DIR}`, `${FILE_PATH}`, `${ENV_FILE}` — отсутствующий путь,
  существующий файл и env-файл.

Относительные пути (`secrets/proj.password`, `ws`, `secrets/proj.env`) записаны
как литералы TOML: так проверяется семантика разрешения относительно каталога
конфигурации без привязки к машине.

## Покрытие

- project ID: паттерн и длина;
- workspace: существование, каталог, canonicalization, относительный путь,
  duplicate workspace;
- `opencode_url`: тип, scheme, host, credentials, query/fragment, path,
  отсутствующий порт, диапазон порта;
- `mcp_url` + `mcp_token_file`: парность, суффикс `/mcp`, endpoint,
  пустой token, token == password, дублирование token;
- port/endpoint conflicts: duplicate endpoint и коллизия MCP/OpenCode;
- `max_rounds`;
- `opencode_model`: тип, формат, whitespace;
- credential file uniqueness (token vs password, token across projects);
- `opencode_env_file`: path semantics и syntax/safety содержимого
  (comments, first `=`, NUL, имена, duplicate, protected names, UTF-8, mode,
  owner, symlink, directory, missing);
- `auto_approve_permissions`;
- `auto_approve_external_directories` и отклонение `/`;
- linked project discovery: canonical exact match, детерминированная сортировка,
  unregistered trusted dir, containing parent dir.

Defensive fail-closed ветви `load_linked_projects` (ambiguous mapping, broken
sibling project) не выражаются через `projects.toml`: глобальное правило
уникальности workspace делает их недостижимыми без подмены загрузчика, поэтому
они намеренно не включены в corpus и остаются unit-тестами Python.

## Использование в дифференциальных Python/Rust tests

1. Читать `cases` из JSON.
2. Создать временный fixture-каталог и материализовать placeholder-ы и
   `setup` (для env-файлов — права `0600`, владелец, symlink/directory).
3. Подставить placeholder-ы в `toml` (или взять `content`/`content_hex`).
4. Вызвать соответствующую `operation` в Python-реализации и в Rust-реализации.
5. Для `invalid` проверить, что обе реализации отклонили вход с той же
   `error_category`; literal message не является частью контракта и не
   сравнивается.
6. Для `valid` сравнить нормализованные поля из `expect`.

Такой harness воспроизводит corpus на текущей Python suite и позже становится
общим differential-раннером Python/Rust.
