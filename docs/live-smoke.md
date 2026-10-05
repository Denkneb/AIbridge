# Реальный OpenCode/provider smoke — 2026-10-05

Проверен полный путь standalone worktree task: Rust stdio MCP → Rust worker →
реальный OpenCode/provider → verifier → review → frozen `on_accept` delivery →
повторное принятие. Результат — passed. Машинный отчёт без секретов и private
runtime paths: [live-smoke-2026-10-05.json](fixtures/runtime/live-smoke-2026-10-05.json).

## Условия

- OpenCode **1.18.34**, provider `opencode-go`, model `minimax-m2.7`.
- Одноразовый Git-проект и отдельный fresh Rust-owned v17 state в `/tmp`.
- Отдельные OpenCode XDG data/config/cache/state directories; временная копия
  provider auth с mode 0600, приватный root 0700. Auth и HTTP password удалены
  после запуска; пользовательский auth не изменялся.
- Python runtime SQLite/history не читались, не импортировались и не менялись.
- Задача разрешала только `result.txt`: записать точные bytes `bridge-live-ok\n`.
  Проверка — заранее созданный `python3 verify.py`, сравнивающий содержимое.
- Worktree execution и `delivery_mode = "on_accept"`; основной checkout
  изменяется только при доставке принятого результата.

## Проверенные результаты

1. Настоящий Rust stdio MCP выполнил initialize и предоставил шесть tools.
2. `submit_task` создал worktree task и запустил worker с реальным provider.
3. Provider создал файл; verifier завершился exit 0, status `passed`, без
   изменения HEAD/index/worktree fingerprints во время проверки.
4. Задача перешла в `awaiting_review`; проверены diff и точные bytes результата.
5. `accept_task` доставил файл в основной checkout: `delivery_state=delivered`.
6. Повторный `accept_task` вернул тот же результат. SQLite подтвердил один
   round и одну attempted отправку: новый prompt не отправлялся.
7. Отдельный `launch-opencode` с generated local MCP config запустил настоящий
   `opencode mcp list`: `agent_bridge` connected, exit 0.

Проверка controller внутри сетевого sandbox остановилась на bootstrap загрузке
каталога моделей OpenCode. При запуске с доступом к сети завершилась успешно;
изменений MCP протокола для этого не потребовалось.

Первый пробный запуск с `gpt-6-luna` запросил чтение родительского `/tmp` и
остановился в `needs_user`. Доступ автоматически не разрешался. Успешный запуск
использовал `minimax-m2.7` и конкретную инструкцию создать относительный файл.

## Границы проверки

Это один реальный worktree pipeline и реальное подключение local controller.
Полная матрица 15.x — linked projects, concurrent writers, live recovery и
crash injection — этим запуском не закрыта. Resume после тринадцати durable
границ, third-state refusal, writer gates и сохранение сырых bytes Git index
проверены отдельными offline integration tests.

Полный `cargo test --workspace` прошёл после функциональных этапов. После
последующего исправления optional Git locks повторно прошли шесть delivery и
четыре bounded-runner checks. Targeted all-targets Clippy прошёл. Финальная
правка initialize instructions проверена существующим protocol suite.

Transport/infrastructure errors намеренно используют безопасные fixed labels;
проверка 29 frozen validation envelopes не означает идентичности всех сырых
Python error strings. Для delivery отдельные файлы заменяются атомарно, весь
набор файлов общей атомарной транзакцией не является: после прерывания возможна
смесь base/artifact файлов, которую доводит до результата журнал resume.
