# Fixtures мульти-репозиторной агрегации `agent_bridge`

Машиночитаемый corpus: [fixtures/multi-repository-cases.json](fixtures/multi-repository-cases.json).
Это контрактные fixtures для будущих Rust-задач подзадачи 0.5b-2b; они описывают
наблюдаемое поведение существующего Python-кода для задачи, затрагивающей основной
workspace и один или несколько внешних доверенных Git-репозиториев. Rust здесь не
реализуется.

Corpus **не дублирует** низкоуровневые mutation/snapshot cases: каждое
per-repository поле `result` — это уже готовый результат `worker._repo_changes` для
одного repository, семантика которого определена в
[git-snapshot-fixtures.md](git-snapshot-fixtures.md) и corpus
[fixtures/git-snapshot-cases.json](fixtures/git-snapshot-cases.json). Данный corpus
описывает только агрегацию этих per-repository результатов в общий task result, а
также выбор затронутых внешних репозиториев при submit и формы `task_status`.

Corpus детерминирован: без timestamps, секретов, request/task ids и абсолютных
machine-specific путей. JSON-ключи отсортированы, `cases` отсортированы по `id`.
Абсолютные корни, пути, HEAD и fingerprints записаны placeholder-ами.

## Источники

Corpus построен по фактическому коду и тестам Python-репозитория `agent_bridge`
(только чтение), в первую очередь:

- `src/agent_bridge/worker.py` — `_collect_changes` задаёт агрегацию: главный
  workspace первым, внешние репозитории по возрастанию canonical root, qualified
  плоские списки, `task_changed_paths` только из main, `baseline_dirty_paths` только
  из main; `_repo_changes` задаёт форму и семантику per-repository результата;
  `_merge_qualified` — `sorted(set(target) | set(extra))`;
- `src/agent_bridge/git_snapshot.py` — `group_allowed_paths_by_repo` (relative →
  main, absolute → repository по `repo_root_for`), `take_external_snapshots`
  (snapshot-ятся только затронутые external repositories, отсортированные по root),
  `validate_allowed_paths`/`_validate_external_scope_path` (канонизация и
  fail-closed проверки доверенных корней), `scope_violations`;
- `src/agent_bridge/mcp_server.py` — `submit_task_impl` (выбор репозиториев, слияние
  `baseline_dirty_paths`, `dirty_workspace`/`dirty_paths_outside_scope`),
  `_repositories_view`, `_has_repository_violations`, `_minimal_task_result`
  (compact), `_verbose_task_result` (verbose), `_single_repository_view`
  (backward compatibility);
- `tests/test_git_snapshot.py` — multi-repository scope
  (`test_group_allowed_paths_by_repo`, `test_take_external_snapshots_skip_main_workspace`,
  `test_absolute_allowed_path_inside_trusted_external_repo`, reject-ветви);
- `tests/test_worker.py` — агрегация (`test_collect_changes_covers_external_repository`,
  `test_collect_changes_qualifies_identical_relative_paths`,
  `test_external_commit_outside_scope_is_reported`,
  `test_external_head_and_index_controlled_without_allow_commit`,
  `test_external_history_rewrite_is_reported`,
  `test_missing_external_repo_is_reported`, `test_two_repository_task_end_to_end`);
- `tests/test_mcp.py` — submit-выбор и слияние baseline
  (`test_submit_snapshots_affected_external_repository`,
  `test_submit_allows_covered_dirty_external_repo`), формы `task_status`
  (`test_task_status_verbose_reports_repositories`,
  `test_task_status_compact_surfaces_external_scope_violation`,
  `test_task_status_compact_omits_repositories_without_violations`,
  `test_old_persisted_result_without_repositories_still_supported`);
- `fixtures/git-snapshot-cases.json` — уже определённая single-repository семантика,
  на которую ссылается поле `derived_from` каждого repository input;
- `fixtures/mcp-cases.json` — error-контракт submit
  (`submit-error-dirty-external-repo`, `submit-error-dirty-external-outside-scope`,
  `submit-error-dirty-workspace`, `submit-error-dirty-paths-outside-scope`); здесь он
  не повторяется.

Версия-источник зафиксирована в поле `source.commit`. Python-репозиторий не
изменялся.

## Schema corpus

Верхний уровень:

| Поле | Назначение |
| --- | --- |
| `corpus_version` | версия формата corpus |
| `source` | commit, репозиторий, модули/tests и ссылка на single-repository corpus |
| `operations` | семантика операций `aggregate_repositories`, `submit_baseline`, `task_status_view` |
| `repository_input_dsl` | описание полей одного repository input |
| `aggregation_rules` | порядок, qualification, слияние и какие поля берутся только из main |
| `path_qualification` | правила workspace-relative vs absolute-qualified путей |
| `view_rules` | compact/verbose/backward-compatibility для `task_status` |
| `placeholder_conventions` | словарь placeholder → описание подстановки |
| `git_policy_violation_categories` | стабильные коды `git_policy_violations` (включая external) |
| `normalization_rules` | нормализация и запрет нестабильных значений |
| `cases` | упорядоченный по `id` список case-ов |

Каждый case:

| Поле | Обязательность | Назначение |
| --- | --- | --- |
| `id` | всегда | стабильный идентификатор case |
| `operation` | всегда | `aggregate_repositories`, `submit_baseline` или `task_status_view` |
| `description` | всегда | что проверяет case |
| `evidence` | всегда | ссылки на `модуль:функцию`, `tests/...::test` или `fixtures/...` |
| `repositories` | для `aggregate_repositories` | main первым, затем внешние repository inputs |
| `task` | для `aggregate_repositories`/`submit_baseline` | `allowed_paths`, `allow_commit`, `allow_dirty` |
| `trusted_roots` | для `submit_baseline` | сконфигурированные доверенные корни |
| `main` | для `submit_baseline` | `baseline_dirty_paths` главного workspace |
| `candidates` | для `submit_baseline` | доверенные external repositories и их baseline |
| `based_on` | для `task_status_view` | id `aggregate_repositories` case, результат которого персистится |
| `stored_result` | для backward-compat | старый плоский результат без `repositories` |
| `snapshot` | для backward-compat | `head`/`dirty_paths` submit-снимка для fallback |
| `status` | для `task_status_view` | статус задачи (`awaiting_review`) |
| `verbose` | для `task_status_view` | значение `task_status(verbose=...)` |
| `expect` | всегда | нормализованный ожидаемый результат |

Для `aggregate_repositories` `expect` — это точный результат
`worker._collect_changes`: `repositories`, `task_changed_paths`, `changed_paths`,
`committed_paths`, `scope_violations`, `git_policy_violations`,
`baseline_dirty_paths`, `head_before`, `head_after`.

Для `submit_baseline` `expect` содержит `snapshotted_external_roots`,
`baseline_dirty_paths` (или `null` при ошибке), `error` и `error_paths`.

Для `task_status_view` `expect` содержит `repositories_present` и `repositories`
(`null`, если ключ отсутствует).

## Repository input DSL

Один repository input описывает repository и **готовый** per-repository результат;
низкоуровневые мутации не повторяются:

| Поле | Значение |
| --- | --- |
| `root` | canonical repository root placeholder: `${MAIN_WORKSPACE}` или внешний root |
| `role` | `main` для workspace, `external` для доверенного внешнего repository |
| `derived_from` | ссылки на `git-snapshot-cases.json:<id>`, задающие single-repository семантику результата |
| `baseline` | `head`, `index_fingerprint`, repository-relative `dirty_paths` (как `take_snapshot`) |
| `allowed_paths` | нормализованный scope этого repository: workspace-relative для main, absolute-qualified для external |
| `result` | вывод `worker._repo_changes`: repository-relative `changed_paths`, `committed_paths`, `scope_violations`, `git_policy_violations`, `head_before`, `head_after` |

`result` — это single-repository результат, а не пересчёт мутаций. Он уже включает
scope-проверку: для external репозитория qualified пути сравнивались с его
absolute allowed entries, а обратно в `result` записаны repository-relative пути.

## Операции

| Операция | Вызов и смысл |
| --- | --- |
| `aggregate_repositories` | По `task.allowed_paths`, `allow_commit` и объявленным repository inputs вычислить ровно `worker._collect_changes`: main entry первым, внешние отсортированы по canonical root, qualified плоские списки, `repositories` с добавленным `root` и `baseline_dirty_paths` |
| `submit_baseline` | Воспроизвести выбор репозиториев и слияние baseline в `submit_task_impl`: snapshot-ятся только external repositories, на которые ссылается абсолютный allowed path (sorted by root); `baseline_dirty_paths` — sorted union main-relative и root-qualified external dirty; без `allow_dirty` непустой union даёт `dirty_workspace` |
| `task_status_view` | Персистировать агрегированный результат `based_on` case (или взять `stored_result`) и вычислить `mcp_server._task_result` с заданным `verbose`; compact включает `repositories` только при violations, verbose — всегда |

`aggregate_repositories` и `submit_baseline` не вызывают MCP tools; полный submit
envelope и error-категории остаются в `mcp-cases.json`.

## Семантика агрегации

`worker._collect_changes`:

1. `repositories[0]` — main workspace; для него `allowed_paths` берутся только
   relative, `missing_code = "not_a_git_repo"`.
2. `external_repositories` сортируются по строке `root`; для каждого
   `allowed_paths` берутся только absolute, `missing_code = "external_repo_missing"`,
   а scope-пути квалифицируются префиксом root.
3. Плоские списки строятся `_merge_qualified` (`sorted(set(...))`):
   - `changed_paths` = main + `<root>/<path>` для каждого external changed;
   - `committed_paths` = main + `<root>/<path>` для каждого external committed;
   - `scope_violations` = main + `<root>/<path>` для каждого external scope violation;
   - `git_policy_violations` = main коды как есть + `<root>:<code>` для каждого external кода.
4. `task_changed_paths` — **ровно** `changed_paths` main workspace, без external.
5. `baseline_dirty_paths` — **ровно** `baseline_dirty_paths` main; объединённый
   main+external список относится к submit (`submit_baseline`).
6. `head_before`/`head_after` — **ровно** значения main; HEAD каждого external
   остаётся только в его repository entry.

Порядок `repositories`: main всегда первый, внешние — по возрастанию строки root.
Тот же порядок использует `git_snapshot.take_external_snapshots` при построении
snapshot, поэтому persisted `external_repositories` и агрегированный `repositories`
согласованы.

Так как absolute-qualified внешние пути начинаются с `/`, при лексикографической
сортировке они идут перед workspace-relative путями (`${EXTERNAL_ROOT_A}/lib.py`
раньше `module.py`).

## Qualification и нормализация путей

- Main repository: пути остаются workspace-relative литералами и никогда не
  квалифицируются абсолютно.
- External repository: путь записывается как `<canonical absolute root>/<path>`.
  `worker._repo_changes` для external сначала квалифицирует
  repository-relative `changed`/`committed` префиксом root, сравнивает с absolute
  allowed entries и затем снимает префикс обратно в repository-relative для
  repository entry. Поэтому одинаковые относительные пути в разных репозиториях
  никогда не смешиваются.
- `git_policy_violations` external кодируются `<root>:<code>`; `scope_violations` —
  `<root>/<path>`.
- Placeholder-ы абсолютных путей нормализуются перед сравнением; literal
  machine-specific путей в corpus нет.

## Baseline dirty при submit

`submit_task_impl` формирует `baseline_dirty` (main, relative) и `external_dirty`
(`<root>/<path>` для каждого затронутого external). Результат submit содержит
`sorted(baseline_dirty + external_dirty)`. Если объединение непусто и `allow_dirty`
не `True` — `dirty_workspace`; если dirty-путь не покрыт scope своего repository —
`dirty_paths_outside_scope`. Эти error-ветви уже покрыты `mcp-cases.json`; corpus
0.5b-2b проверяет успешный merged baseline и выбор только затронутых репозиториев.

## `task_status`: compact, verbose, backward compatibility

- `_verbose_task_result` (и прямой `_task_result(full=True)`) всегда содержит
  `repositories` — полный per-repository список; для старых результатов без ключа
  `_single_repository_view` синтезирует одну запись из плоских полей.
- `_minimal_task_result` (compact, `task_status` по умолчанию) добавляет
  `repositories` только если `_has_repository_violations` истинно, то есть у хотя бы
  одного repository непусты `scope_violations` или `git_policy_violations`. Иначе
  ключ отсутствует.
- Backward compatibility: persisted результат без `repositories` отображается на
  одну main-запись, поэтому структурированные потребители всегда видят один и тот
  же shape.

## Placeholder conventions

Полный список — в `placeholder_conventions`. Ключевые:

- `${MAIN_WORKSPACE}` — абсолютный корень главного Git workspace;
- `${TRUSTED_ROOT}` — доверенный внешний каталог;
- `${EXTERNAL_ROOT_A}`/`${EXTERNAL_ROOT_B}` — абсолютные корни внешних репозиториев;
- `${HEAD_MAIN_BEFORE}`/`${HEAD_MAIN_AFTER}`,
  `${HEAD_EXT_A_BEFORE}`/`${HEAD_EXT_A_AFTER}`,
  `${HEAD_EXT_B_BEFORE}`/`${HEAD_EXT_B_AFTER}` — opaque HEAD placeholder-ы; значение
  `*_AFTER` равно `*_BEFORE`, когда HEAD не двигался;
- `${INDEX_FP_MAIN_BEFORE}`, `${INDEX_FP_EXT_A_BEFORE}`, `${INDEX_FP_EXT_B_BEFORE}` —
  index fingerprints baseline (sha256 по `git ls-files --stage` и `git ls-files -v`).

Repository-relative пути (`module.py`, `lib.py`, `gen_a.py`, …) записаны литералами:
так проверяется qualification без привязки к машине.

## Git policy violations

`git_policy_violations` формируются `worker._repo_changes` на репозиторий:

- `history_rewritten` — `history_descends_from(base_head, current_head)` ложно;
  добавляется всегда, независимо от `allow_commit`;
- `head_changed` — `current_head != base_head` и `allow_commit is not True`;
- `index_changed` — `index_fingerprint` изменился и `allow_commit is not True`;
- `not_a_git_repo` — main workspace отсутствует или не Git-репозиторий;
- `external_repo_missing` — external repository отсутствует или не Git-репозиторий.

Порядок кодов внутри repository entry: `history_rewritten`, затем `head_changed`,
затем `index_changed`. В плоском списке external коды квалифицируются
`<root>:<code>` и сортируются.

## Покрытие

- только основной workspace без external repository;
- один внешний repository;
- два внешних repository;
- независимые `changed_paths` и `committed_paths` для каждого repo;
- workspace-relative пути main и absolute-qualified пути external;
- одинаковый относительный путь в main и external не смешивается;
- per-repository scope violations;
- per-repository `git_policy_violations` (`head_changed`, `index_changed`);
- dirty external baseline и объединённый `baseline_dirty_paths`;
- external repository без изменений;
- trusted repository, не упомянутый `allowed_paths`, не snapshot-ится и не попадает
  в результат;
- детерминированный порядок repositories;
- top-level `task_changed_paths`/`changed_paths`/`committed_paths` aggregation,
  включая случай, когда меняется только external и `task_changed_paths` пуст;
- compact `task_status` показывает repositories только при violations;
- compact `task_status` без violations опускает repositories;
- verbose `task_status` показывает полный repositories list;
- backward compatibility результата без `repositories`.

## Ограничения

- Только агрегация и формы `task_status`; низкоуровневые mutation/snapshot-ветви не
  дублируются и остаются в `git-snapshot-cases.json`.
- Submit error-категории (`dirty_workspace`, `dirty_paths_outside_scope`,
  `not_a_git_repo`, `git_snapshot_failed`) остаются в `mcp-cases.json`; здесь
  фиксируется только успешный merged baseline и выбор затронутых репозиториев.
- Не включаются rename detection, conflicts/merge, submodules, worktrees, bare
  repositories и partial staging.
- Не фиксируются human-readable `GitError`/`ValueError`-сообщения, timestamps,
  реальные commit hashes, request/task ids и machine-specific пути: контрактом
  являются только списки путей, коды, отношения и порядок.
- Один representative case на независимую aggregation branch; corpus не заменяет
  полную Python test suite и targeted unit-тесты.

## Использование в дифференциальных Python/Rust tests

1. Читать `cases` из JSON.
2. Для `aggregate_repositories`: собрать task snapshot из объявленных repository
   inputs (per-repository `result` берётся как готовый вывод `_repo_changes`) и
   вычислить `_collect_changes`; сравнить `expect` (списки путей, коды, порядок
   `repositories`, `task_changed_paths`, `baseline_dirty_paths`, HEAD main).
3. Для `submit_baseline`: материализовать main и candidate repositories, вызвать
   submit-логику выбора и слияния; сравнить `snapshotted_external_roots`,
   `baseline_dirty_paths` и `error`.
4. Для `task_status_view`: персистировать результат `based_on` (или `stored_result`)
   и сравнить наличие/форму `repositories` для compact и verbose.
5. Нормализовать абсолютные пути, HEAD и fingerprints к placeholder-ам перед
   сравнением; literal error-тексты не сравнивать.

Такой harness воспроизводит corpus на текущей Python suite и позже становится общим
differential-раннером Python/Rust для мульти-репозиторной агрегации.
