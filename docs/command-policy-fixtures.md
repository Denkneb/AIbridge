# Fixtures политики shell-команд `agent_bridge`

Машиночитаемый corpus: [fixtures/command-policy-cases.json](fixtures/command-policy-cases.json).
Это контрактные fixtures для будущих Rust-задач потока 4 (security и Git,
в первую очередь 4.1–4.3) и потока 5.1 (валидация test commands); они описывают
наблюдаемое поведение существующего Python-кода, а не целевую реализацию. Rust
здесь не реализуется.

Corpus намеренно детерминирован: без timestamps, секретов, реальных
tokens/passwords и абсолютных machine-specific путей. JSON-ключи отсортированы,
`cases` отсортированы по `id`. Команды заданы точными синтетическими строками
(placeholder-подстановка не применяется), поэтому один и тот же вход можно
подать в Python и Rust без материализации файловой системы. Один case описывает
одну независимую ветвь поведения; эквивалентные варианты сгруппированы в
`variants`, а не продублированы отдельными case-ами.

## Источники

Corpus построен по фактическому коду и тестам Python-репозитория
`/home/denis/Python/agent_bridge` (только чтение), в первую очередь:

- `src/agent_bridge/command_policy.py` — единственный источник истины:
  `bash_pattern_problem`, `has_shell_metacharacters`, `has_glob`,
  `split_command`, `tokens_problem`, `git_invocation_problem`,
  `env_invocation_problem`, `leading_assignments`;
- `src/agent_bridge/verifier.py` — `validate_test_commands` и требование
  непустого `argv` после leading assignments;
- `src/agent_bridge/worker.py` — `_permission_decision`, ветвь
  `permission == "bash"`;
- `tests/test_command_policy.py`, `tests/test_verifier.py` — подтверждение
  наблюдаемых значений и ветвей.

Версия-источник зафиксирована в поле `source.commit`. Python-репозиторий не
изменялся.

## Schema corpus

Верхний уровень:

| Поле | Назначение |
| --- | --- |
| `corpus_version` | версия формата corpus |
| `source` | commit, репозиторий и список модулей/tests |
| `contexts` | два контекста вызова: `permission` и `test_command` |
| `decision_model` | как из результата policy получается allow/deny в каждом контексте |
| `reason_categories` | стабильные идентификаторы причин deny и allow-причины permission |
| `cases` | упорядоченный по `id` список case-ов |

Каждый case:

| Поле | Обязательность | Назначение |
| --- | --- | --- |
| `id` | всегда | стабильный идентификатор case |
| `context` | всегда | `permission` или `test_command` |
| `command` | всегда | точный синтетический вход (строка) |
| `expectation` | всегда | `allow` или `deny` |
| `reason_category` | всегда | стабильная причина; `null` для allow в `test_command`, `configured` для allow в `permission` |
| `description` | всегда | краткое пояснение ветви |
| `evidence` | всегда | ссылки на `модуль:функцию` и/или `tests/...::test` |
| `variants` | опционально | эквивалентные входы с тем же `context`, `expectation` и `reason_category` |

`variants` — это часть контракта: harness обязан проверить каждый вариант так
же, как основной `command`. Human-readable `detail` из
`worker._permission_decision` в corpus не фиксируется: контрактом является
только машинный `reason`.

## Контексты и модель решения

| Контекст | Вызов | Allow | Deny |
| --- | --- | --- | --- |
| `permission` | `worker._permission_decision(config, perm)` при `permission="bash"`, `patterns=[command]`, `bash` в `auto_approve_permissions` | `reason="configured"` | `reason` = первый код policy |
| `test_command` | `verifier.validate_test_commands([command])` | пустой список проблем | `reason` = код policy или `missing_executable` |

Оба контекста используют один и тот же `command_policy.bash_pattern_problem`,
поэтому deny-коды совпадают; различаются только allow-причина и дополнительные
проверки уровня конверта. В частности, assignment-only вход (`FOO=bar`) в
`permission` разрешается, а в `test_command` отклоняется с
`missing_executable`, потому что verifier-у нечего запускать без shell.

## Категории решений

Полный список — в `reason_categories`. Ключевые:

- `unprovable_shell_syntax` — raw-строка содержит shell-метасимволы
  (`; | & < > \` $ ( ) { }`) или newline/CR; проверка выполняется до `shlex`,
  поэтому метасимвол внутри кавычек тоже отклоняется;
- `git_write_blocked` — разрешённая git-подкоманда `add`/`commit`/`push` после
  пропуска wrappers, path-prefix и global options;
- `unprovable_git_glob` — glob в позиции git-подкоманды;
- `unprovable_glob_command` — glob в обычном (не git) токене;
- `unprovable_wrapper_command` — `env -S`/`--split-string` (включая
  `--split-string=`, `-Sxxx` и безопасную по содержимому команду);
- `unsafe_shell_invocation` — shell-исполняемый файл без `-c`;
- `shell_command_missing` — `sh -c` без следующей строки команды;
- `empty_bash_pattern` — пустой или whitespace-only вход;
- `missing_executable` — только leading assignments (только `test_command`);
- `unparsable_bash_pattern` — NUL или `shlex` не разобрал строку.

## Покрытие

- разрешённые простые команды, read-only git, path-prefixed executable,
  leading assignments с executable;
- `git add`/`commit`/`push`, лишние пробелы, абсолютный путь к git,
  `-C`/`-c`/`--git-dir`/`--work-tree`/`--namespace` и glob-подкоманда;
- wrappers `env`, `sudo`, `command`, `nohup`, `nice`, `time`, `exec`,
  `env -S`/`--split-string` и безопасные `env -i`/`env FOO=bar`/`env --`;
- `eval`, `sh`/`bash`/`zsh`/`dash -c`, shell без `-c`, `sh -c` без строки;
- separators `;`, `&&`, `||`, pipe `|` и background `&`;
- redirects (`>`, `<`);
- command substitution `$(...)`, backticks, subshell и brace group;
- variable/brace expansion и newlines;
- пустые команды, whitespace, assignments-only (различие контекстов);
- quoted literals: безопасные пробелы, `=` и backslash внутри кавычек
  разрешаются, а метасимвол/glob внутри кавычек всё равно отклоняется, потому
  что policy сканирует raw-строку.

## Ограничения

- Corpus не включает path/symlink/external repository/Git snapshot policy — это
  отдельная подзадача 0.5b.
- Конверт permission (`missing_request_id`, `missing_permission`,
  `malformed_patterns`, `missing_bash_patterns`, `not_configured`,
  `unknown_permission`) и не-строковые/не-list входы verifier
  (`invalid_test_command`, `invalid_test_commands`) не входят: они уже покрыты
  `fixtures/mcp-cases.json` и относятся к конфигурации/транспорту, а не к
  разбору самой команды.
- Human-readable `detail`, `output_tail` и тексты сообщений не фиксируются.
- Corpus не заменяет полную Python test suite и targeted unit-тесты; он
  воспроизводит независимые ветви, а не все комбинации.

## Использование в дифференциальных Python/Rust tests

1. Читать `cases` из JSON.
2. Для каждого case и каждого элемента `variants` взять `context` и `command`.
3. В Python: для `permission` вызвать `worker._permission_decision` с
   синтетическим `perm` (как в `tests/test_command_policy.py::_approve`); для
   `test_command` — `verifier.validate_test_commands([command])`.
4. В Rust: вызвать соответствующий эквивалент policy-функции и конверта.
5. Сравнить `expectation` и `reason_category`; literal `detail` не сравнивается.
6. Для `allow` в `permission` проверить `reason == "configured"`; для `allow` в
   `test_command` — пустой список проблем.

Такой harness воспроизводит corpus на текущей Python suite и позже становится
общим differential-раннером Python/Rust.
