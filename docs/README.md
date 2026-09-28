# Переписывание agent-bridge на Rust

Этот комплект документов описывает целевую Rust-реализацию `agent-bridge` и
графическое приложение на GPUI. Это план миграции, а не описание уже
реализованного продукта.

## Цель

- сохранить гарантии безопасности и совместимость CLI, MCP и SQLite;
- перейти к поставке единым Rust-бинарником;
- добавить настройку проектов через GUI;
- объединить основной Codex/OpenCode TUI слева и dashboard задач справа.

Одномоментная замена Python не предполагается: Rust-компоненты вводятся
поэтапно и принимаются после дифференциальных проверок.

## Изоляция реализаций

Python- и Rust-версии никогда не используют общую рабочую SQLite БД или общий
runtime state. У каждой реализации собственные state root, SQLite-файлы, locks,
PID/ownership records, token-файлы, логи и endpoints/порты. Rust всегда создаёт и
использует собственную пустую БД и отдельную историю задач; импорт, копирование
или перенос Python state/history в Rust не поддерживается и не планируется, а
Python state не получает записей от Rust. Дифференциальные fixtures остаются
эталоном Python-семантики и не являются общей рабочей БД.

Работа разбивается на небольшие автономные задачи. Каждая задача должна иметь
узкий список файлов, один основной результат, короткий набор проверок и не
требовать загрузки в контекст всего проекта. Правила декомпозиции и очередь
задач приведены в [плане реализации](implementation-plan.md).

## Разделы

1. [Архитектура](architecture.md)
2. [Интерфейс](interface.md)
3. [Системные требования](system-requirements.md)
4. [План реализации](implementation-plan.md)
5. [Тестирование и миграция](testing-and-migration.md)
6. [Риски и решения](risks-and-decisions.md)
7. [Существующий контракт](existing-contract.md)
8. [Manifest контракта](contract-manifest.json)
9. [Fixtures конфигурации](config-fixtures.md)
10. [Corpus конфигурации](fixtures/config-cases.json)
11. [Fixtures MCP](mcp-fixtures.md)
12. [Corpus MCP](fixtures/mcp-cases.json)
13. [Fixtures SQLite](sqlite-fixtures.md)
14. [Manifest SQLite](fixtures/sqlite/expected.json)
15. [Fixtures политики команд](command-policy-fixtures.md)
16. [Corpus политики команд](fixtures/command-policy-cases.json)
17. [Fixtures политики путей](path-policy-fixtures.md)
18. [Corpus политики путей](fixtures/path-policy-cases.json)
19. [Fixtures snapshot/сравнения Git](git-snapshot-fixtures.md)
20. [Corpus snapshot/сравнения Git](fixtures/git-snapshot-cases.json)
21. [Fixtures мульти-репозиторной агрегации](multi-repository-fixtures.md)
22. [Corpus мульти-репозиторной агрегации](fixtures/multi-repository-cases.json)

## Базовые ограничения

- `projects.toml` остаётся единственным источником конфигурации проектов; запись
  в него допускается только от активной реализации при остановленных runtime,
  поэтому конкурентной записи не возникает.
- Python и Rust никогда не используют общую рабочую `state.sqlite` или общий
  runtime state: у каждой реализации свои state root, БД, locks,
  PID/ownership records, token-файлы, логи и endpoints/порты.
- Rust начинает с новой пустой БД и отдельной истории; Python history в Rust не
  появляется, а Python state не получает записей от Rust.
- Имена CLI-команд и контракт шести MCP tools сохраняются.
- Неоднозначные security-сценарии обрабатываются fail-closed.
- Python и Rust worker/MCP не запускаются одновременно для одного проекта.
- Dashboard первого GUI-релиза остаётся read-only.
