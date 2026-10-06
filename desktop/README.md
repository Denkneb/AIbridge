# AIbridge desktop

Tauri 2 + React/TypeScript/Vite, with xterm.js and a Rust PTY service. The
headless CLI and `bridge-desktop` services build without GTK or WebView.

## Build and run

On Debian/Ubuntu install `pkg-config`, `libgtk-3-dev` and
`libwebkit2gtk-4.1-dev` (Tauri's Linux build dependencies). Node 22.12+ and the
repository's Rust toolchain are required.

```sh
cargo build --offline -p agent-bridge-cli
npm ci --prefix desktop
npm run --prefix desktop build
cargo build --manifest-path desktop/src-tauri/Cargo.toml
./desktop/src-tauri/target/debug/aibridge-desktop \
  --config /absolute/path/projects.toml \
  --state-root /absolute/path/rust-state
```

A missing config opens an empty project list without writing files. Use
Settings → New → Preview → Save to create the first project, then Setup to
create private credentials and isolated Rust state. Existing project tables
and comments are preserved. Saved credentials are never loaded into React;
new values are cleared after preview. Save refuses active tasks, service
records and busy controllers. Stop them explicitly before changing settings.
Each changed config has a sibling backup. Individual config/credential writes
are atomic; they are not a single multi-file transaction.

For development, start `npm run dev` in `desktop` and run the Tauri target with
`--no-default-features` so it loads the Vite dev URL. Production builds embed
`desktop/dist` and prohibit remote content through CSP. The window has no
arbitrary shell, filesystem or SQL IPC. The native directory picker is the only
enabled dialog permission.

## Terminal and dashboard

The terminal opens fixed Shell, OpenCode or Codex profiles. OpenCode uses the
existing Rust controller; task attachment uses the proven task checkout/session
router. Codex uses `agent-bridge launch-codex` with process-local primary/linked MCP
wiring, durable delegation rules, read-only sandbox and bounded status hooks.
Both controllers hold the shared controller fence until their child exits.
Switching project/tab or resizing does not recreate an existing terminal.
Open/Attach explicitly replaces it. Output queues and input chunks are bounded;
output stays ordered and xterm preserves UTF-8 across chunks. Closing the window
terminates and reaps its terminal processes before application exit. Managed
project services still use the explicit Start/Stop lifecycle.

Dashboard reads owned Rust state only, aggregates configured trusted linked
projects, and supports Active/All, search, status filters and global pagination.
DB/WAL change stamps skip unchanged snapshots, with a full refresh every 30
seconds. Rows are virtualized, task selection is retained by ID, and refreshes do not
launch workers or recover tasks. Details show verifier results, repository
scope, findings, checkpoints, dependency edges, writer reservations/waiting count, saved usage and budget gates.
Usage includes all saved rounds even when the details display is capped at 100.
Delivery is shown separately from acceptance, and automation ready/completed
states remain distinct. Private paths, logs and suspected secrets are omitted.

## Checks

```sh
cargo test --offline -p bridge-desktop
cargo clippy --offline -p bridge-desktop --all-targets -- -D warnings
AIBRIDGE_DESKTOP_SMOKE=1 npm run --prefix desktop build
cargo build --manifest-path desktop/src-tauri/Cargo.toml --features desktop-smoke
python3 tools/desktop_smoke.py \
  --desktop desktop/src-tauri/target/debug/aibridge-desktop \
  --output /tmp/desktop-proof.json --screenshot /tmp/desktop-proof.png
```

The smoke build adds a test-only WebView script. It uses disposable config and
workspace, no providers, and tests React rendering, real IPC, credential
redaction, PTY input/resize, settings rendering and terminal persistence across
rerender. Its sandbox override is scoped to the smoke child only. Production
launches do not enable this script or write smoke reports.

If build dependencies are extracted instead of installed, set `PATH`,
`PKG_CONFIG_SYSROOT_DIR` and `PKG_CONFIG_PATH` to that sysroot when building,
and pass `--sysroot /tmp/aibridge-sysroot` to the smoke runner. The repository
never requires the developer's `/tmp` sysroot for ordinary installed builds.

On this container's Debian WebKit build, `WEBKIT_EXEC_PATH` alone does not
relocate helper processes. The recorded rootless proof used a temporary copy
of WebKit with its compiled helper prefix redirected to extracted helpers.
The runner does not perform that patch automatically; installed dependencies
use their normal system paths. X11 was verified; Wayland and live provider
TUI compatibility remain separate checks.

Terminal input is serialized in 4 KiB chunks with a 1 MiB pending cap and bounded
retry when the Rust queue is full. Ctrl+C remains SIGINT; Ctrl+Shift+C/V use the
WebView clipboard, with explicit errors if clipboard access is unavailable.
Project switches preserve the bound session; Open/Attach replace it explicitly.
The smoke frontend build exposes its xterm instance only for compatibility checks;
ordinary Vite builds remove this test hook. The actual WebView proof covers ANSI,
alternate screen, wide/combining Unicode, selection, bracketed paste, 12,000-character
paste without byte loss, and bounded scrollback. OS clipboard and mouse/provider
interaction require separate live checks.

“Во внешнем терминале” uses the installed `x-terminal-emulator -e` with the same
fixed profile and project binding, without a shell command string. Missing terminal
support produces an explicit error. External windows have their own lifecycle.
Interactive `launch-codex` needs no `--auto`; `--auto --plan PATH` retains approved
workflow behavior. MCP credentials stay in child environment, never argv or frontend.
Codex hooks remain subject to Codex's ordinary hook trust flow.
