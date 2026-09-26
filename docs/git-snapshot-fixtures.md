# Fixtures snapshot и сравнения одного Git-репозитория `agent_bridge`

Машиночитаемый corpus: [fixtures/git-snapshot-cases.json](fixtures/git-snapshot-cases.json).
Это контрактные fixtures для будущих Rust-задач потока 4 (4.7–4.9) и подзадачи
0.5b-2a; они описывают наблюдаемое поведение существующего Python-кода
`git_snapshot` и `worker._repo_changes` для **одного** repository, а не целевую
реализацию. Rust здесь не реализуется.

Corpus намеренно детерминирован: без timestamps, секретов и абсолютных
machine-specific путей. JSON-ключи отсортированы, `cases` отсортированы по `id`.
Виртуальный Git-репозиторий один на case и описывается placeholder-ами; `setup`
задаёт начальное состояние, `before` — снимок до мутаций, `mutations` —
упорядоченный список изменений, `expect` — нормализованный результат сравнения.
Эквивалентные мутации сгруппированы в `variants`.

Corpus покрывает snapshot/comparison **одного repository** (0.5b-2a). External
repositories, multi-repository aggregation и qualify путей по абсолютным корням
относятся к подзадаче 0.5b-2b и здесь намеренно отсутствуют.

## Источники

Corpus построен по фактическому коду и тестам Python-репозитория `agent_bridge`
(только чтение), в первую очередь:

- `src/agent_bridge/git_snapshot.py` — единственный источник истины:
  `take_snapshot`, `status_porcelain`, `status_paths`, `index_fingerprint`,
  `manifest`, `_listed_files`, `_hash_file`, `changed_paths`, `committed_paths`,
  `history_descends_from`, `head`, `is_repo`, `_run`;
- `src/agent_bridge/worker.py` — `_repo_changes` задаёт форму и семантику
  per-repository результата: `changed_paths`, `committed_paths`,
  `scope_violations`, `git_policy_violations`, `baseline_dirty_paths`,
  `head_before`/`head_after` и коды `head_changed`, `index_changed`,
  `history_rewritten`, `not_a_git_repo`;
- `tests/test_git_snapshot.py` — подтверждение ветвей snapshot/comparison
  (`test_changed_paths`, `test_committed_paths_since_snapshot`,
  `test_executable_bit_change_is_detected`, `test_symlink_retarget_is_detected`,
  `test_ignored_file_is_not_dirty`, `test_snapshot_excludes_ignored`,
  `test_symlink_does_not_read_ignored_target`,
  `test_index_fingerprint_detects_intent_to_add`,
  `test_snapshot_records_dirty_paths_and_index`);
- `tests/test_worker.py` — контракт `_repo_changes` (`test_staging_without_permission_is_reported`,
  `test_commit_without_permission_is_reported`,
  `test_dirty_baseline_is_not_attributed_to_task`,
  `test_allowed_commit_of_dirty_baseline_is_reported`,
  `test_history_rewrite_is_reported_even_when_commit_allowed`,
  `test_scope_violation_reported`);
- `tests/test_mcp.py` и `fixtures/mcp-cases.json` — стабильные error-категории
  `not_a_git_repo` и `git_snapshot_failed` (submit envelope уже покрыт
  `mcp-cases.json`, здесь фиксируется только граница snapshot).

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
| `mutation_dsl` | описание упорядоченных операций `mutations` |
| `placeholder_conventions` | словарь placeholder → описание подстановки |
| `fingerprint_relations` | смысл `equal`/`changed` для HEAD/index/worktree |
| `git_policy_violation_categories` | стабильные коды `git_policy_violations` |
| `error_categories` | стабильные категории `snapshot_error` |
| `cases` | упорядоченный по `id` список case-ов |

Каждый case:

| Поле | Обязательность | Назначение |
| --- | --- | --- |
| `id` | всегда | стабильный идентификатор case |
| `operation` | всегда | `snapshot_compare` или `snapshot_error` |
| `setup` | всегда | начальное состояние единственного виртуального repository |
| `before` | для `snapshot_compare` | снимок до мутаций: `head`, `index_fingerprint`, `worktree_fingerprint` как placeholder-ы |
| `mutations` | всегда | упорядоченный список мутаций |
| `allowed_paths` | всегда | уже нормализованный scope (repository-relative) |
| `allow_commit` | всегда | значение `snapshot.allow_commit`; управляет `head_changed`/`index_changed` |
| `allow_dirty` | всегда | значение `snapshot.allow_dirty`; помечает baseline-dirty сценарии |
| `stage` | для `snapshot_error` | `repo_check` или `snapshot` |
| `expect` | всегда | нормализованный результат |
| `evidence` | всегда | ссылки на `модуль:функцию`, `tests/...::test` или `fixtures/...` |
| `variants` | опционально | эквивалентные мутации с **тем же** `expect` |

Для `snapshot_compare` `expect` содержит:

| Поле | Назначение |
| --- | --- |
| `changed_paths` | `git_snapshot.changed_paths`, repository-relative, sorted |
| `committed_paths` | `git_snapshot.committed_paths`, repository-relative, sorted |
| `scope_violations` | `scope_violations(sorted(set(changed) | set(committed)), allowed_paths)` |
| `git_policy_violations` | коды из `git_policy_violation_categories` |
| `baseline_dirty_paths` | `before.dirty_paths` как есть |
| `relations` | отношения `head`/`index`/`worktree` со значениями `equal`/`changed` |

Для `snapshot_error` `expect` содержит только `error_category`, а `before` и
`relations` отсутствуют: снимок не может быть построен, и сравнивать нечего.

`variants` — часть контракта: harness обязан проверить каждую variant так же, как
основной case. Variant задаёт только `description` и `mutations`; `setup`,
`before`, `allowed_paths`, `allow_commit` и `expect` наследуются от case.

## Setup DSL

`setup` описывает один виртуальный repository, который harness материализует до
снятия `before`:

| Ключ | Значение |
| --- | --- |
| `workspace` | `{git_repo}`; `git_repo=true` инициализирует `${WORKSPACE}` как Git worktree, `false` оставляет простой каталог |
| `files` | список `{path, content}`; `path` — абсолютное placeholder-выражение с корнем `${WORKSPACE}` |
| `symlinks` | список `{path, target}`; `target` — строка ссылки |
| `gitignore` | список паттернов `.gitignore`; файл `.gitignore` создаётся из них |
| `commits` | упорядоченный список `{message, add}`; каждый коммит стейджит ровно перечисленные repository-relative пути |
| `fault` | только `snapshot_error`: `{snapshot_command: "fail"}` — harness должен заставить git-команду снимка бросить `GitError` |

Пути, созданные `files`/`symlinks`/`gitignore`, но не попавшие ни в один
`commits[].add`, остаются untracked или ignored — так моделируются baseline-dirty
и ignored сценарии без отдельных операций.

## Mutation DSL

`mutations` — упорядоченный список операций, применяемых к worktree после
`before` и до сравнения:

| Операция | Аргументы | Смысл |
| --- | --- | --- |
| `write_file` | `path`, `content` | записать файл, создавая родительские каталоги; заменить при наличии |
| `delete_file` | `path` | удалить файл из worktree |
| `chmod_exec` | `path`, `mode?` | выставить executable-бит (по умолчанию `0755`) |
| `symlink_create` | `path`, `target` | создать symlink |
| `symlink_retarget` | `path`, `target` | заменить существующий symlink |
| `git_add` | `path` | `git add` текущего содержимого |
| `git_add_intent` | `path` | `git add -N`: меняет только index flags |
| `git_commit` | `message`, `add?` | `git add` перечисленных путей, затем новый commit |
| `git_amend` | `message`, `add?` | `git add`, затем `git commit --amend` (перезапись истории) |

## Placeholder conventions

Полный список — в `placeholder_conventions`. Ключевые:

- `${WORKSPACE}` — абсолютный путь единственного синтетического Git-репозитория;
- `${HEAD_BEFORE}` — commit hash снимка `before` (opaque placeholder);
- `${INDEX_FP_BEFORE}` — index fingerprint `before` (sha256 по
  `git ls-files --stage` и `git ls-files -v`);
- `${WORKTREE_FP_BEFORE}` — worktree fingerprint `before`: отображение
  `git_snapshot.manifest` (tracked + untracked non-ignored) в content/mode/link
  hashes; verifier дополнительно сворачивает его в компактный digest вместе со
  status.

Repository-relative пути (`module.py`, `src/ok.py`, `other/thing.py`) в
`mutations`, `allowed_paths` и `expect` записаны литералами: так проверяется
repository-relative семантика без привязки к машине.

## Операции

| Операция | Вызов и смысл |
| --- | --- |
| `snapshot_compare` | `before = take_snapshot(workspace)`; применить `mutations`; вычислить ровно то, что вычисляет `worker._repo_changes` для одного repository: `changed_paths`, `committed_paths`, `scope_violations`, `git_policy_violations`, `baseline_dirty_paths` и отношения fingerprints |
| `snapshot_error` | submit-time отказ до снимка: `not_a_git_repo` (`is_repo` false) или `git_snapshot_failed` (`GitError` внутри снимка); контракт — только категория |

`snapshot_compare` не вызывает MCP tools и не строит envelope: он изолирует
сравнение одного repository. `snapshot_error` намеренно узкий и фиксирует только
границу ошибок snapshot; полный submit envelope остаётся в `mcp-cases.json`.

## Fingerprint relations

`relations` выражают **отношение** after к before, а не значение:

- `head`: `equal`, если `git_snapshot.head` не изменился; иначе `changed`;
- `index`: `equal`, если `git_snapshot.index_fingerprint` не изменился; иначе
  `changed` (index включает staged entries и `ls-files -v` flags);
- `worktree`: `equal`, если `git_snapshot.manifest` не изменился; иначе
  `changed` — это же множество определяет `changed_paths`.

`changed_paths` пуст ровно тогда, когда `worktree == equal` и `committed_paths`
не добавляет путей.

## Git policy violations

`git_policy_violations` формируются `worker._repo_changes`:

- `history_rewritten` — `history_descends_from(base_head, current_head)` ложно;
  добавляется всегда, независимо от `allow_commit`;
- `head_changed` — `current_head != base_head` и `allow_commit is not True`;
- `index_changed` — `index_fingerprint` изменился и `allow_commit is not True`;
- `not_a_git_repo` — comparison-level `missing_code`, когда repository
  отсутствует или не является Git-репозиторием.

Порядок кодов в `expect` совпадает с порядком добавления в коде:
`history_rewritten`, затем `head_changed`, затем `index_changed`.

## Error categories

`error_categories` фиксируют стабильные категории `snapshot_error`:

- `not_a_git_repo` — workspace не является Git worktree; на уровне сравнения
  `worker._repo_changes` возвращает `git_policy_violations=["not_a_git_repo"]`
  с пустыми списками и `head_after=null`;
- `git_snapshot_failed` — git-команда снимка бросила `GitError`; detail не
  является контрактом.

Обе категории уже присутствуют в `fixtures/mcp-cases.json` как submit envelope;
здесь они включены только как граница snapshot и не дублируют полный envelope.

## Покрытие

- clean before/after;
- tracked content modification, untracked file, deletion;
- staged change (`git add`) и intent-to-add (`git add -N`) без движения HEAD;
- новый commit и движение HEAD;
- commit плюс оставшееся worktree-изменение;
- executable-bit/mode change;
- symlink target change (в том числе на dangling target);
- ignored file не входит в changed_paths и не хеширует содержимое цели через
  symlink;
- file-scope и directory-scope violations;
- baseline dirty path без изменений и с дополнительным изменением;
- task-created change, отличное от baseline;
- index change без HEAD movement;
- history rewrite (`--amend`);
- suppression git policy при `allow_commit=true`;
- error categories `not_a_git_repo` и `git_snapshot_failed`.

## Ограничения

- Только один repository на case; external repositories, trusted roots,
  multi-repository aggregation и qualify путей по абсолютным корням — это
  отдельная подзадача 0.5b-2b.
- Не включаются rename detection, conflicts/merge, submodules, worktrees,
  bare repositories и partial staging (`git add -p`).
- Не фиксируются human-readable `GitError`/`ValueError`-сообщения, timestamps,
  реальные commit hashes и machine-specific пути: контрактом являются только
  списки путей, коды и отношения.
- Corpus описывает временный виртуальный repository; операции никогда не
  выполняются в текущем Git-проекте.
- Один representative case на независимую ветвь; corpus не заменяет полную
  Python test suite и targeted unit-тесты.

## Использование в дифференциальных Python/Rust tests

1. Читать `cases` из JSON.
2. Создать временный каталог, материализовать `setup` (workspace, files,
   symlinks, gitignore, commits) и снять `before` через реализацию
   `take_snapshot`.
3. Для каждого case и каждой variant применить `mutations` по `mutation_dsl`.
4. Вычислить результат операции (`snapshot_compare` или `snapshot_error`) в
   Python- и Rust-реализации.
5. Сравнить `expect`: списки путей, `git_policy_violations`, `baseline_dirty_paths`
   и `relations`. Абсолютные пути и fingerprints нормализуются к placeholder-ам
   перед сравнением; literal error-тексты не сравниваются.
6. Для `snapshot_error` сравнить только `error_category`.

Такой harness воспроизводит corpus на текущей Python suite и позже становится
общим differential-раннером Python/Rust для snapshot/comparison одного
repository.
