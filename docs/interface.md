# Интерфейс

## Выбранный стек

Целевой desktop UI: **React + TypeScript**, сборка **Vite**, оболочка **Tauri 2**.
Dashboard, настройки и navigation реализуются в React. **xterm.js** отображает
терминал; Rust backend создаёт PTY, запускает Codex/OpenCode и обрабатывает
input/output, resize и завершение процессов. Выбор стека принят 2026-10-04;
GUI пока не реализован.

Первый UI-прототип проверяет Tauri window, resizable terminal/dashboard split
и настоящую PTY session. До разработки полноценных форм и dashboard проверяются
ввод, Unicode, resize, поток вывода и process cleanup. Очередь backend-задач
сохраняется: ближайшая задача — 7.14.

## Основное окно

```text
┌──────────────────────────────────────────────────────────────────┐
│ Project ▾  Start Stop Doctor  Controller: Codex ▾  Settings      │
├────────────────────────────────┬─────────────────────────────────┤
│ Основной чат / PTY             │ Dashboard                       │
│                                │ Active | All | Search            │
│ codex или opencode TUI         │ status / task / round / updated  │
│                                │                                 │
│ ANSI/VT, scrollback, selection │ Details                         │
│ clipboard, mouse, resize       │ session / paths / verification  │
├────────────────────────────────┴─────────────────────────────────┤
│ project | workspace | OpenCode | MCP | worker | notifications    │
└──────────────────────────────────────────────────────────────────┘
```

Splitter сохраняет ширину панелей. Переключение проекта не уничтожает
работающий PTY молча: пользователь выбирает оставить, завершить или открыть
отдельное окно.

## Левая панель: основной чат

Первый полноценный релиз встраивает настоящий TUI через PTY:

- `codex` с теми же overrides, token env и инструкциями, что `launch-codex`;
- либо `opencode` в controller mode;
- cwd равен workspace выбранного проекта;
- секретные env передаются дочернему процессу, но не отображаются.

Собственный chat client поверх внутренних API не входит в первый релиз: иначе
придётся повторять streaming, tool calls, permissions, slash-команды и recovery.

Terminal view должен поддерживать alternate screen, ANSI/VT, Unicode и wide
glyphs, SIGWINCH, scrollback, selection, clipboard, bracketed paste, mouse
reporting, keyboard focus и graceful shutdown. До полной совместимости остаётся
кнопка открытия внешнего терминала.

xterm.js подключается к DOM через React adapter; PTY stream идёт из Rust через
Tauri channels. Размеры терминала передаются в backend для PTY resize/SIGWINCH.
Вывод не проходит через React state или HTML rendering; очереди ограничены,
порядок байтов и backpressure проверяются при большом объёме output.

## Правая панель: dashboard

Dashboard первого релиза read-only. Он показывает:

- задачи основного и связанных проектов;
- Active/All, поиск и фильтры проекта/статуса;
- task id, статус, раунд, update time и наличие session;
- описание, allowed paths и baseline dirty paths;
- blockers, tool errors, verification и repository violations;
- usage/model только в диагностических деталях.

React получает read-only DTO через Tauri IPC из Rust query service.
SQLite и HTTP операции выполняются вне UI thread. WAL notification допустим
как оптимизация, периодический refresh остаётся fallback. Список
виртуализируется, выбранная строка сохраняется по `task_id`.

Разрешено открыть/подключить session. Кнопки `accept`, `request changes` и
`close` требуют отдельной модели авторизации и не входят в первый релиз.

## Настройка проектов

Экран настроек поддерживает:

- создание, редактирование и клонирование проекта;
- выбор workspace;
- OpenCode/MCP endpoints и проверку портов;
- max rounds и model;
- пути password/token/env files без показа содержимого;
- auto-approve permissions и trusted external directories;
- `Validate`, `Setup`, `Start`, `Stop`, `Doctor`;
- preview изменений перед сохранением.

`projects.toml` остаётся каноническим. Сохранение атомарное: полная валидация,
временный файл с безопасными правами, `fsync`, rename. Удаление проекта не
удаляет workspace, state и credentials.

## UX и accessibility

- Полная навигация клавиатурой и видимый focus ring.
- Отдельный размер шрифта terminal/dashboard.
- Статус различается не только цветом.
- Ошибки копируются без секретов.
- Поддерживаются HiDPI и системная тема.
- Ошибка одной панели не закрывает приложение и работающий PTY.
