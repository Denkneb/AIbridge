# Системные требования

## Целевая платформа

Первый target — современный Linux x86_64, соответствующий текущей зависимости
runtime от pidfd и файловых блокировок.

Минимальная среда:

- Linux с pidfd support;
- Wayland или X11;
- WebKitGTK и системные библиотеки, требуемые Tauri 2 на Linux;
- `git`, `codex` и `opencode` в `PATH`;
- доступ к localhost endpoints проектов;
- Unix permissions и locking semantics.

Точные версии ядра, дистрибутивов, WebView и требования к GPU фиксируются после
desktop prototype и CI. Системные пакеты сверяются с
[Tauri prerequisites](https://v2.tauri.app/start/prerequisites/).

## Сборка

- Stable Rust, закреплённый `rust-toolchain.toml`.
- Cargo lockfile хранится в репозитории.
- Tauri 2 закрепляется через Cargo lockfile; React, TypeScript, Vite и xterm.js —
  через frontend package manifest и lockfile.
- Для frontend development/build нужны Node.js и выбранный package manager;
  их версии фиксируются при UI prototype вместе с версиями frontend packages.
- Vite собирает статические assets для Tauri WebView; dev server используется
  только при разработке. Production desktop не требует отдельного Node server.
- Headless CLI не зависит от Tauri/WebView и frontend build.
- Build packages документируются после первого CI build на поддерживаемых ОС.

## Runtime layout

Python сохраняет существующий XDG layout, Rust получает отдельный namespace:

```text
# Python (существующий, не меняется)
~/.config/agent-bridge/projects.toml
~/.config/agent-bridge/secrets/
~/.local/state/agent-bridge/<project_id>/state.sqlite
~/.local/state/agent-bridge/<project_id>/*.log

# Rust (изолированный)
~/.config/agent-bridge-rs/secrets/
~/.local/state/agent-bridge-rs/<project_id>/state.sqlite
~/.local/state/agent-bridge-rs/<project_id>/*.log
```

`projects.toml` остаётся общим каноническим файлом конфигурации: Rust читает
его read-only в период сосуществования и не пишет, пока существует Python
runtime. Запись допускается только от активной реализации при остановленных
runtime, поэтому конкурентной записи не возникает.

`--config` указывает на общий `projects.toml`. `--state-root` по умолчанию
различается: `$XDG_STATE_HOME/agent-bridge` для Python и
`$XDG_STATE_HOME/agent-bridge-rs` для Rust. Явно переданный `--state-root`
принимается только если он не указывает на state другой реализации: иначе Rust
fail-closed отказывается запускаться по ownership/format marker. GUI не создаёт
альтернативную базу project settings.

## Предлагаемые зависимости

- Tauri 2 — desktop window, WebView и IPC;
- React + TypeScript — dashboard, settings и UI state;
- Vite — frontend development/build;
- xterm.js — terminal rendering/input, Rust PTY adapter — process lifecycle;
- `tokio`, `reqwest` — async и HTTP;
- `serde`, `serde_json`, `toml` — данные;
- `rusqlite` или `sqlx` — SQLite;
- `axum` — HTTP transport;
- Rust MCP SDK, если он покрывает текущий контракт;
- `rustix`/`nix` — pidfd, PTY, signals и locks;
- `tracing`, `thiserror`, `proptest`.

Выбор MCP SDK, Rust PTY crate и способ передачи terminal stream уточняются
отдельными spikes. Frontend стек и terminal renderer выбраны; их совместимость
с Codex/OpenCode проверяется первым desktop prototype.

## Распространение

Headless поставка — бинарник `agent-bridge`. Desktop поставка — Tauri application
package с Rust executable и собранными frontend assets; имя desktop executable
фиксируется при prototype. Backend services общие для CLI и desktop.
Для CLI подходят архив с binary/checksums либо пакет дистрибутива.
Desktop Linux packages и WebView dependencies проверяются отдельно;
AppImage/Flatpak оцениваются с учётом доступа к PTY, workspaces, процессам
и внешним CLI.

## Нефункциональные требования

- SQLite, HTTP, Git и process waits не блокируют GUI.
- Секреты не попадают в telemetry и crash reports.
- Логи имеют retention/rotation policy.
- Headless CLI остаётся поддерживаемым.
- GUI crash не завершает detached OpenCode/MCP servers.
