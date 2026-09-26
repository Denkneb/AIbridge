# Fixtures политики путей и symlink confinement `agent_bridge`

Машиночитаемый corpus: [fixtures/path-policy-cases.json](fixtures/path-policy-cases.json).
Это контрактные fixtures для будущих Rust-задач потока 4 (security и Git,
в первую очередь 4.1–4.3) и подзадачи 0.5b; они описывают наблюдаемое поведение
существующего Python-кода `git_snapshot`, а не целевую реализацию. Rust здесь не
реализуется.

Corpus намеренно детерминирован: без timestamps, секретов и абсолютных
machine-specific путей. JSON-ключи отсортированы, `cases` отсортированы по `id`.
Вся файловая система и Git-репозитории виртуальные: пути заданы
документированными placeholder-ами, а `setup` описывает, что harness должен
материализовать до вызова. Один case описывает одну независимую ветвь поведения;
эквивалентные входы сгруппированы в `variants`.

Corpus покрывает **только разрешение путей и symlink confinement** (подзадача
0.5b-1). Сравнение changed/status/index/HEAD, commit policy и агрегация
изменений по нескольким репозиториям — это отдельная подзадача 0.5b-2 и здесь
намеренно отсутствуют.

## Источники

Corpus построен по фактическому коду и тестам Python-репозитория
`agent_bridge` (только чтение), в первую очередь:

- `src/agent_bridge/git_snapshot.py` — единственный источник истины:
  `validate_allowed_paths`, `_validate_external_scope_path`, `repo_root_for`,
  `group_allowed_paths_by_repo`, `is_allowed`, `scope_violations`, `_hash_file`,
  `changed_paths`;
- `tests/test_git_snapshot.py` — подтверждение наблюдаемых ветвей
  (`test_validate_allowed_paths`, `test_symlink_escape_rejected`,
  `test_symlink_retarget_is_detected`, `test_dangling_symlink_target_is_hashed`,
  `test_scope_semantics`, `test_absolute_allowed_path_*`,
  `test_external_repo_root_outside_trusted_root_rejected`,
  `test_group_allowed_paths_by_repo`, `test_repo_root_for_path`);
- `tests/test_mcp.py` — конверт `invalid_allowed_paths`, подтверждающий, что
  абсолютный workspace path отклоняется с той же категорией.

Версия-источник зафиксирована в поле `source.commit`. Python-репозиторий не
изменялся.

## Schema corpus

Верхний уровень:

| Поле | Назначение |
| --- | --- |
| `corpus_version` | версия формата corpus |
| `source` | commit, репозиторий и список модулей/tests |
| `operations` | семантика операций, вызываемых на case |
| `setup_dsl` | описание ключей виртуального `setup` |
| `placeholder_conventions` | словарь placeholder → описание подстановки |
| `decision_categories` | смысл `allow`/`deny` |
| `reason_categories` | стабильные идентификаторы причин deny |
| `cases` | упорядоченный по `id` список case-ов |

Каждый case:

| Поле | Обязательность | Назначение |
| --- | --- | --- |
| `id` | всегда | стабильный идентификатор case |
| `operation` | всегда | `validate_allowed_paths`, `group_allowed_paths_by_repo`, `scope_match` или `symlink_identity` |
| `setup` | всегда | виртуальные workspace/trusted roots/files/symlinks/repos |
| `input` | всегда | `allowed_paths` и параметры операции (`external_roots`, `probe`, `symlink`/`before_target`/`after_target`) |
| `expectation` | всегда | `allow` или `deny` |
| `expect` | для `allow`, а также для `scope_match`/`symlink_identity` | нормализованный scope, группировка, verdict или список нарушений |
| `reason_category` | всегда | стабильная причина; `null` для `allow` |
| `description` | всегда | краткое пояснение ветви |
| `evidence` | всегда | ссылки на `модуль:функцию` и/или `tests/...::test` |
| `variants` | опционально | эквивалентные входы с той же `expectation` и `reason_category` |

`variants` — часть контракта: harness обязан проверить каждый вариант так же, как
основной `input`. Human-readable текст `ValueError` в corpus не фиксируется:
контрактом является только `reason_category`.

## Setup DSL

`setup` описывает виртуальную файловую систему, которую harness материализует
перед вызовом. Ключи:

| Ключ | Значение |
| --- | --- |
| `workspace` | `{git_repo}`; объявляет корень `${WORKSPACE}` и инициализирует его как Git-репозиторий, если `git_repo=true` |
| `trusted_roots` | список абсолютных placeholder-путей, передаваемых как `external_roots`; пустой список — trusted-каталогов нет |
| `repositories` | список `{root, git_repo}`; `git_repo=true` — корень инициализируется как Git-репозиторий с коммитом |
| `entries` | список объектов: `dir`, `file` или `symlink` (`path`, для symlink ещё `target` и `confinement`) |

`confinement` symlink-а принимает значения `in_scope` (target остаётся внутри
workspace), `escape` (target существует вне workspace), `dangling` (target
отсутствует, но лексически внутри workspace) и `dangling_escape` (target
отсутствует и лежит вне workspace).

## Placeholder conventions

Полный список — в `placeholder_conventions`. Ключевые:

- `${WORKSPACE}` — синтетический главный workspace (всегда Git-репозиторий);
- `${TRUSTED_DIR}` — trusted external root, содержащий `${TRUSTED_REPO}`,
  `${TRUSTED_REPO_OTHER}` и `${TRUSTED_PLAIN}`;
- `${TRUSTED_REPO}`, `${TRUSTED_REPO_OTHER}` — Git-репозитории под trusted root;
- `${TRUSTED_PLAIN}` — каталог под trusted root, не входящий ни в один
  Git-репозиторий;
- `${OUTSIDE_DIR}`, `${OUTSIDE_REPO}` — каталог и Git-репозиторий вне trusted
  roots;
- `${PARENT_REPO}` и `${TRUSTED_SUBDIR}` — Git-репозиторий и вложенный trusted
  root, чей корень репозитория лежит выше trusted root;
- `${WORKSPACE_LINK}`, `${WORKSPACE_ESCAPE_LINK}`, `${TRUSTED_ESCAPE_LINK}` —
  symlink-и для confinement-ветвей.

Относительные входы (`module.py`, `src/`, `link-in/file.py`, `escape/`) записаны
литералами: так проверяется семантика разрешения относительно workspace без
привязки к машине.

## Операции

| Операция | Вызов и смысл |
| --- | --- |
| `validate_allowed_paths` | `validate_allowed_paths(workspace, allowed_paths, external_roots)`: возвращает нормализованный scope или бросает `ValueError` |
| `group_allowed_paths_by_repo` | `group_allowed_paths_by_repo(...)`: отображает каждый путь на канонический корень содержащего репозитория; trusted roots не учитываются |
| `scope_match` | `is_allowed`/`scope_violations` на одном нормализованном scope и одном candidate path |
| `symlink_identity` | один symlink снимается `take_snapshot`, затем сравнивается `changed_paths` после retarget; изолирует идентичность target-строки |

`group_allowed_paths_by_repo` не выполняет валидацию и не нормализует entries:
относительные пути попадают в ключ `${WORKSPACE}` как есть, абсолютные — в ключ
своего репозитория; абсолютный путь вне Git-репозитория даёт
`not_git_repository`.

`scope_match` и `symlink_identity` намеренно узкие: они проверяют чистый
предикат пути и идентичность symlink-а и **не** покрывают общее сравнение
changed/status/index/HEAD (0.5b-2).

## Категории решений и причин

`decision_categories`:

- `allow` — операция вернула нормализованный scope, группировку или
  не-нарушающий verdict, не бросив исключение;
- `deny` — операция упала fail-closed с `ValueError`; сравнивается только
  `reason_category`, не текст сообщения.

`reason_categories`:

- `invalid_allowed_paths_entry` — entry не строка или пустая строка;
- `backslash_in_path` — entry начинается с `\` или содержит `\`;
- `parent_traversal` — entry содержит компонент `..`;
- `empty_or_dot_path` — entry сводится к пустой строке или `.` (включая `/`);
- `absolute_workspace_path` — абсолютный entry разрешается внутри workspace;
  проверяется до trusted-root lookup, исправление — относительный путь;
- `outside_trusted_roots` — абсолютный entry разрешается вне всех trusted roots,
  в том числе через symlink;
- `not_git_repository` — абсолютный entry не входит ни в один Git-репозиторий;
- `external_repo_root_outside_trusted` — путь внутри trusted root, но корень
  содержащего репозитория лежит вне trusted roots;
- `workspace_escape` — относительный entry разрешается вне workspace через
  symlink (лексический `..` ловится раньше как `parent_traversal`);
- `scope_violation` — candidate path не покрыт ни одним нормализованным scope
  (файл — точное совпадение, каталог — строгий префикс);
- `symlink_retarget_detected` — target symlink-а изменился между snapshot и
  comparison; идентичность — строка target, а не содержимое цели.

## Покрытие

- относительный файл и directory scope внутри workspace, включая нормализацию
  trailing slash;
- пустой список и некорректные/пустые entries (пустая строка, не-строка,
  backslash, `..`, `.`/`/`);
- абсолютный workspace path (файл и каталог) и parent traversal;
- missing path semantics: отсутствующий файл и каталог разрешаются лексически и
  остаются workspace-relative, различаясь только trailing slash;
- symlink, остающийся внутри scope;
- symlink escape (относительный и абсолютный);
- dangling symlink внутри и вне scope;
- symlink retarget detection между snapshot и comparison (обычный и dangling);
- absolute path внутри trusted external Git repository (файл и каталог);
- путь вне trusted roots;
- trusted root, который не является Git repository;
- external repo root/child containment;
- grouping allowed paths by repository только на уровне path resolution
  (relative+external, два внешних репозитория, root+child, пустой список,
  не-Git путь).

## Ограничения

- Не включаются changed/status/index/HEAD comparison, commit policy и
  multi-repository change aggregation — это отдельная подзадача 0.5b-2. Cases
  `scope_match` и `symlink_identity` изолируют только предикат пути и
  идентичность symlink-а.
- Не включается MCP-конверт `invalid_allowed_paths` и прочие envelope-проверки:
  они уже покрыты `fixtures/mcp-cases.json`.
- Не фиксируются human-readable `ValueError`-сообщения, timestamps и реальные
  локальные пути.
- Symlink loop (`Path.resolve()` бросает `RuntimeError`, а не `ValueError`)
  намеренно не включён: это отдельная Python-специфичная ветвь без стабильной
  deny-категории.
- Один representative case на независимую ветвь; corpus не заменяет полную
  Python test suite и targeted unit-тесты.

## Использование в дифференциальных Python/Rust tests

1. Читать `cases` из JSON.
2. Создать временный fixture-каталог и материализовать `setup` по `setup_dsl`,
   подставив placeholder-ы из `placeholder_conventions`.
3. Для каждого case и каждого элемента `variants` подставить placeholder-ы в
   `input` и вызвать `operation` в Python-реализации и в Rust-реализации.
4. Для `deny` проверить одинаковую `reason_category`; literal message не
   сравнивается.
5. Для `allow` сравнить `expect` (нормализованный scope, `repo_grouping`,
   verdict или список нарушений); абсолютные пути нормализуются к placeholder-ам
   перед сравнением.
6. Для `symlink_identity` сравнить `changed_paths` и `violations` из `expect`.

Такой harness воспроизводит corpus на текущей Python suite и позже становится
общим differential-раннером Python/Rust.
