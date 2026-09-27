# Системные требования

## Целевая платформа

Первый target — современный Linux x86_64, соответствующий текущей зависимости
runtime от pidfd и файловых блокировок.

Минимальная среда:

- Linux с pidfd support;
- Wayland или X11;
- GPU/драйвер, поддерживаемый GPUI renderer;
- `git`, `codex` и `opencode` в `PATH`;
- доступ к localhost endpoints проектов;
- Unix permissions и locking semantics.

Точные версии ядра, дистрибутивов и GPU фиксируются после prototype и CI.

## Сборка

- Stable Rust, закреплённый `rust-toolchain.toml`.
- Cargo lockfile хранится в репозитории.
- GPUI закрепляется точной версией или Git revision, поскольку он pre-1.0.
- В Linux включаются GPUI backends `wayland`, `x11` либо оба.
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

- GPUI/`gpui_platform` — UI;
- `tokio`, `reqwest` — async и HTTP;
- `serde`, `serde_json`, `toml` — данные;
- `rusqlite` или `sqlx` — SQLite;
- `axum` — HTTP transport;
- Rust MCP SDK, если он покрывает текущий контракт;
- `rustix`/`nix` — pidfd, PTY, signals и locks;
- `tracing`, `thiserror`, `proptest`.

Выбор SQLite crate, MCP SDK и terminal engine принимается отдельными spikes.

## Распространение

Цель — один бинарник `agent-bridge`. На первом этапе подходят архив с binary и
checksums либо пакет дистрибутива. AppImage/Flatpak оцениваются отдельно из-за
доступа к PTY, workspaces, процессам и внешним CLI.

## Нефункциональные требования

- SQLite, HTTP, Git и process waits не блокируют GUI.
- Секреты не попадают в telemetry и crash reports.
- Логи имеют retention/rotation policy.
- Headless CLI остаётся поддерживаемым.
- GUI crash не завершает detached OpenCode/MCP servers.
