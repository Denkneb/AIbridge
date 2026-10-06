# Переключение проекта на Rust v17

Репетиция на одноразовых проектах доступна в `tools/migration_rehearsal.py`.
Она проверяет fresh v17, ownership, приватные credentials, настоящий lifecycle
OpenCode/MCP, падение MCP, restart, rollback и последовательный запуск проектов.
Действующий проект выбирается отдельно: нужны ID, канонический `projects.toml`,
путь к Python CLI и отдельный абсолютный Rust state root вне workspace.

## Подготовка и остановка

1. Закончить либо явно остановить активные задачи, automation и controller TUI.
2. Сделать резервную копию канонического `projects.toml` и сохранить его права.
   Credentials резервируются отдельно в приватном хранилище, без вывода содержимого.
3. Остановить Python runtime выбранного проекта его собственной командой `stop`.
   Проверить, что старые endpoints свободны и старые процессы завершились.
   Остальные проекты продолжают использовать свою реализацию.
4. Сохранить Python установку и её отдельный state для отката. Rust не открывает,
   не копирует и не импортирует Python SQLite/history.

Одновременный Python/Rust worker либо MCP для одного проекта не допускается.
Редактировать общий TOML можно только при остановленных runtime/controllers и
отсутствии активных задач; Rust config-edit fences дополнительно проверяют это.

## Fresh Rust state и проверка

Ниже переменные задаёт оператор. `RUST_BRIDGE_BIN` — абсолютный путь к Rust CLI,
`BRIDGE_CONFIG` — канонический TOML, `RUST_STATE_ROOT` — новый отдельный namespace.

```bash
"$RUST_BRIDGE_BIN" setup --project "$PROJECT_ID" \
  --config "$BRIDGE_CONFIG" --state-root "$RUST_STATE_ROOT"
"$RUST_BRIDGE_BIN" start --project "$PROJECT_ID" \
  --config "$BRIDGE_CONFIG" --state-root "$RUST_STATE_ROOT"
"$RUST_BRIDGE_BIN" doctor --json --project "$PROJECT_ID" \
  --config "$BRIDGE_CONFIG" --state-root "$RUST_STATE_ROOT"
"$RUST_BRIDGE_BIN" status --json --project "$PROJECT_ID" \
  --config "$BRIDGE_CONFIG" --state-root "$RUST_STATE_ROOT"
```

Setup создаёт пустой Rust-owned **v17**, marker и недостающие private credentials.
Повторный setup сохраняет credentials. Чужой owner и symlink bindings отклоняются.
Doctor/status должны подтвердить ready и managed для обоих сервисов. Короткий
startup probe можно повторить в пределах нескольких секунд; ошибки устойчивого
readiness нельзя скрывать повторными проверками.

В пилоте выполнить небольшую заранее согласованную задачу с узким scope и
read-only verifier. Проверить diff, принять результат и отдельно проверить delivery.
Для GUI открыть desktop с теми же config/state-root; проверить выбранный workspace,
dashboard и Codex/OpenCode TUI. Смена вкладки/проекта сохраняет текущую session;
Open/Attach явно заменяет её. Закрытие окна завершает embedded PTY, main services
останавливаются отдельной командой Stop.

## Crash/recovery drill

В одноразовой репетиции runner проверяет identity и использует pidfd для
преднамеренного падения собственного MCP child. На рабочем проекте этот drill
проводится в согласованном окне после завершения задач.

Проверить, что status замечает падение; выполнить Stop, затем Start и повторить
Doctor. Для task/coordinator crash использовать отдельные fixture proofs и
explicit recovery/resume, проверяя отсутствие повторной отправки prompt.
Не отправлять сигнал процессу по непроверенному PID и не удалять locks вручную.

## Откат

1. Явно остановить Rust automation/controllers/tasks и main services.
2. Восстановить backup TOML, если он менялся, после освобождения config fences.
3. Запустить сохранённую Python реализацию с её исходным отдельным state.
4. Проверить Python readiness собственными средствами.

Rust history остаётся в Rust namespace. Обратный перенос state/history отсутствует.
Восстановление файла TOML в репетиции проверяется по точным bytes; восстановление
Python runtime на действующем проекте отмечается только после реального запуска.

## Обкатка и последовательный перевод

Короткий воспроизводимый запуск:

```sh
python3 tools/migration_rehearsal.py \
  --bridge /absolute/agent-bridge --opencode /absolute/opencode \
  --samples 60 --interval 1 --output /tmp/migration-rehearsal.json
```

Для действующего пилота нужен согласованный период наблюдения; рекомендуемый
начальный период — один рабочий день с обычной нагрузкой. Собирать readiness
failures и startup retries раздельно, failures/needs_user/delivery_unknown,
успешные проверки и delivery, отсутствие повторных prompt, утечки процессов,
расход budgets, latency и рост logs/state. Короткая репетиция устанавливает
работоспособность процедуры; длительная эксплуатационная стабильность требует
наблюдения рабочего проекта.

Следующий проект переводится по той же процедуре после успешного пилота и
проверенного отката. Для каждого проекта записываются окно остановки, версия CLI,
config/state binding и результат проверок. Массовый `--all` не заменяет приёмку
каждого переключения.

## Решение об архивировании Python

На текущем этапе Python сохраняется как независимый fallback. Архивирование
рассматривается после перевода выбранных проектов, согласованного периода
наблюдения и реального rollback rehearsal. Исходники и state/history не удаляются
в рамках этой работы. Фактический переход рабочего проекта пока ожидает выбора
ID/config; закрытые disposable proofs перечислены в implementation plan.
