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
arbitrary shell, filesystem or SQL IPC. Native directory/file pickers use the only enabled dialog permission.

## Remove a project

Settings → **Удалить проект** previews removal of the selected saved registration.
**Подтвердить удаление** removes only its table from `projects.toml` and writes a
sibling backup. The workspace, credentials, runtime state and task history remain
available; other project tables are preserved. Removing the last project leaves
a valid empty `[projects]` table. Cancel and stale config previews cannot remove
anything. Re-registering the same project ID/workspace uses the retained state.

Removal refuses live services, controllers, executors, unfinished task rows and
nonterminal automation (including paused or blocked runs). Stop automation and
services, close controller tabs, and finish or close remaining tasks first.
Readers linked to the removed registration and projects sharing credentials use
the existing config-edit fences; unrelated active projects do not block removal.
The backend checks these conditions again under locks when applying the review.

## Automatic approved plans

Open **Автоматизация**, upload a JSON plan or paste the plan prepared during your
Codex discussion. **Проверить план** validates it using the same policy as
`launch-codex --auto --plan`, displays tasks, scopes, acceptance criteria,
dependencies, checks, delivery and effective limits. **Утвердить и запустить**
starts the exact reviewed document through the CLI's detached supervisor.
Editing the plan, replacing/cancelling its review, or changing project configuration
invalidates approval. The review is consumed before launch to prevent a repeated
IPC request from submitting it twice. After a launch error, refresh status before
preparing another review; a durable run may already exist and can be resumed.

The latest run refreshes every three seconds, with per-step progress, review
findings, revision counts and blocker codes. Pause, resume and stop always target
the displayed run UUID. Pause is cooperative and waits for the current operation;
stop uses the existing cleanup lifecycle. Closing the window leaves the detached
supervisor running. Saved per-project Codex launch variables apply to start and
resume. Invalid variables cannot prevent pause or stop.

A clean supported main Git repository and no unfinished tasks are required.
The plan's `delivery` controls final behavior: `apply` applies the cumulative
result automatically; `manual` leaves it ready for separate `deliver-task --apply`
using the final task UUID shown in the run. This UI uses the existing worktree
workflow and does not create intermediate Git commits.

## Per-project OpenCode settings

Settings exposes optional `opencode_model` (executor), Rust-specific
`opencode_controller_model` (interactive `bridge-controller`), and
`opencode_env_file` (executor/service environment). Model IDs use `provider/model`;
blank values remove overrides. A task profile's frozen model takes precedence
for execution; the controller override is written into its generated agent config
and does not change executor or Codex models. The env path can be typed or selected
with the native file picker. It resolves relative to `projects.toml`; preview and
save verify the existing private mode-0600 file without sending its values to React.

After saving a project, its OpenCode editor explicitly loads `opencode.json` or
`opencode.jsonc` from the saved workspace. It supports creating a missing file,
editing text, JSON/JSONC and bridge-reserved-key validation, review and explicit
save. JSONC comments are preserved verbatim. Both load-to-preview and
preview-to-save detect changed files; config bindings are checked again on save.
Manager/admission/worker/controller fences block edits to an active project and
projects whose controllers link to it through trusted roots or share credential
files. Independent projects
can keep running while another project is edited or added. Both current and
proposed links are checked, including newly registered trusted workspaces. The
shared config-file lock and stale-preview check still serialize file writes.
Config-edit errors name the affected project and distinguish running task executors,
worker/controller locks, live services, state ownership and filesystem failures.
Exited service records are checked by PID start time and boot ID and do not block
saves; records and processes are not deleted or stopped by saving settings.
Existing files receive sibling backups. Paths are limited to those two filenames
inside the configured project, with symlink refusal and a 1 MiB size limit.
The editor loads the selected JSON text only on request; bridge password/token
values and env-file contents remain outside the frontend. The syntax check does
not replace OpenCode's complete schema/provider validation. In worktree mode,
project configuration must also be present in the task checkout. Restart services
and reopen controllers after changing their configuration.

## Terminal and dashboard

The project dropdown shows a service status beside every project: active, partially
started, stopped, checking or unavailable. Read-only diagnostics refresh all
projects every five seconds after each sweep, with at most four simultaneous
queries, and immediately after lifecycle actions. A project whose MCP is ready
and whose main OpenCode is idle is active. The selected project uses the same
status snapshot for its adjacent dot; changing selection does not start services.

A status dot beside the selected project turns green when MCP is ready and
OpenCode is ready or idle, amber when required services are unavailable, and grey
when stopped. Starting an empty project or a project in worktree mode starts only MCP. Every five seconds MCP
stops the managed main OpenCode server if no unfinished tasks remain. Accepted
and closed tasks retain their history; review, failed, waiting and other unfinished
tasks keep the server available. Direct submissions and `console`/direct task
attachment start OpenCode on demand. Console leases prevent idle shutdown until
attachment exits, including external terminals. Codex controllers need only MCP
and do not keep an idle main server running. Worktree executors continue to use
their separate per-task servers and existing cleanup rules. Hovering the green
dot explains when OpenCode is idle. Stdio MCP also checks for idle shutdown after
responding to a tool request.
An outlined dot means checking or unavailable status; hover reveals the state.
Read-only Doctor checks refresh every five seconds and after lifecycle actions,
independently of dashboard polling. Switching projects cancels stale results.
Doctor also reports the specific OpenCode failure in the status-dot tooltip:
HTTP timeout, authentication, health, workspace identity or API compatibility.
A timeout with a proven live process reports that the process is running but
not responding; it does not identify a cause or prove a deadlock. Main-server
startup and console attachment retain these causes in their errors, and a process
that exits before becoming ready is reported separately. Existing services and
tasks are never stopped or replaced by these checks. No watcher-disable flag is
added automatically.

The terminal opens fixed Shell, OpenCode or Codex profiles. OpenCode uses the
existing Rust controller; task attachment uses the proven task checkout/session
router. “Codex без моста” starts `codex` directly in the project workspace,
using the user's ordinary Codex configuration. AIbridge adds no MCP wiring,
controller prompt, hooks or sandbox override in this mode. “Codex через мост”
uses `agent-bridge launch-codex` with process-local primary/linked MCP wiring,
durable delegation rules, read-only sandbox and bounded status hooks.
Codex launch variables accept `NAME=value` or `export NAME=value`, one per line,
with literal values (no shell expansion). The Save variables button and both
Codex launch buttons in either mode persist them per project at
`<state-root>/<project>/desktop-codex.env` with mode 0600. Opening the app restores
them; switching projects keeps drafts separate. Clear and save to remove overrides.
Invalid text is refused without echoing values; symlink and non-private files are
refused. Existing terminal sessions retain the environment from their launch.
The bridge Codex and OpenCode controllers hold a shared config-edit fence and separate exclusive
profile locks until their children exit, so the two controllers can coexist.
Each terminal tab owns a PTY and xterm buffer; hidden tabs keep draining output.
Open reuses a running Codex/OpenCode tab for the selected project and opens a new
Shell tab. Attach reuses the tab for that exact project/task. Closing a tab stops
only its process. Up to eight tabs across projects are supported. The tab list and
visible terminal belong to the selected project. Each project remembers its active
tab, selected program and environment draft; other PTYs remain mounted and keep
running with their output buffered in the background. Switching project/tab or
resizing preserves existing sessions. Output queues and input chunks are bounded;
output stays ordered and xterm preserves UTF-8 across chunks. Closing the window
terminates and reaps its terminal processes before application exit. Managed
project services still use the explicit Start/Stop lifecycle.

Dashboard reads owned Rust state for the selected project only, and supports Active/All, search, status filters and global pagination.
DB/WAL change stamps skip unchanged snapshots, with a full refresh every 30
seconds. Rows are virtualized, task selection is retained by ID, and refreshes do not
launch workers or recover tasks. List snapshots contain summaries and per-task
revision stamps; round JSON, usage and repository details are fetched only for
the selected task. An unchanged task keeps its open details during unrelated
dashboard refreshes. Switching selection cancels stale detail/page responses.
Details show verifier results, repository
scope, findings, checkpoints, dependency edges, writer reservations/waiting count, saved usage and budget gates.
Round history starts with the latest ten rounds; “Загрузить ещё раунды” fetches
ten older rounds using a cursor. If the task changes while a page is loading,
the page is rejected rather than appended to stale history. Usage includes all
saved rounds regardless of the displayed page; only the usage fields are read
from saved result JSON.
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
use their normal system paths. X11 and Wayland were verified with actual Codex/OpenCode TUI startup, input and
resize. TUI tests use private empty homes; model execution is verified separately
by `tools/live_automation_smoke.py`.

Terminal input is serialized in 4 KiB chunks with a 1 MiB pending cap and bounded
retry when the Rust queue is full. Ctrl+C remains SIGINT; Ctrl+Shift+C/V use native GTK clipboard IPC, with 1 MiB bounds, a two-second deadline and
explicit errors if clipboard access or the display input seat is unavailable.
GTK/WebKit IBus commits that emit `compositionend` without `compositionstart`
forward only the committed character, preserving ordinary IME composition and
preventing accumulated textarea text from being resent. The X11 smoke runner
injects physical Russian-layout keys on its disposable display; `--ibus` adds a
private IBus daemon, and `--ibus --live-tui` also checks input in the Codex tab.
These checks require `setxkbmap`, `libXtst`, and (for `--ibus`) `ibus-daemon`/`ibus`.
Project switches preserve all terminal tabs; Open/Attach selects or creates a tab.
The smoke frontend build exposes its xterm instance only for compatibility checks;
ordinary Vite builds remove this test hook. The actual WebView proof covers ANSI,
alternate screen, wide/combining Unicode, selection, bracketed paste, 12,000-character
paste without byte loss, and bounded scrollback. Native Unicode clipboard roundtrip and SGR mouse reporting also pass in the
actual WebView. Codex/OpenCode render and accept input/resize in both display
backends. Model execution has a separate live workflow proof.

“Во внешнем терминале” uses the installed `x-terminal-emulator -e` with the same
fixed profile and project binding, without a shell command string. Missing terminal
support produces an explicit error. External windows have their own lifecycle.
Interactive `launch-codex` needs no `--auto`; `--auto --plan PATH` retains approved
workflow behavior. MCP credentials stay in child environment, never argv or frontend.
Codex hooks remain subject to Codex's ordinary hook trust flow.

Wayland proof uses Weston with a virtual X11 input seat and Pixman renderer so
native GTK clipboard has a real seat. System libraries are unchanged. Installed
Weston/Xvfb or the optional extracted sysroot are needed for the smoke runner:

```sh
python3 tools/desktop_smoke.py --desktop /absolute/aibridge-desktop \
  --display-backend wayland --live-tui --output /tmp/wayland-proof.json
```

`--live-tui` uses private empty Codex/OpenCode homes to verify interactive startup,
input, resize and cleanup, including initial login/trust screens. It does not
perform a provider task. The separate live automation proof uses copied auth.

The compact terminal toolbar keeps program selection and Open visible. Font size,
Clear, and external terminal launch are in the “⋯” menu. Use the tab’s “×” to close
its session; it replaces the duplicate toolbar close button.

“Запустить” first prepares the selected project’s state and missing credentials,
then starts its services. Project configuration is edited through “Настройки”;
there is no separate setup button in the toolbar.

The app header combines project selection, service actions, navigation, and theme
in one row. The workspace path is shown on hover over the project selector.
Narrow windows can scroll the header horizontally to reach every action.

Settings includes “Ключи провайдеров OpenCode”: enter a variable name and a masked
value, save or replace it, or delete a saved name. Existing values are never
returned to the frontend. AIbridge creates a private 0600 per-project env file
under the config directory’s secrets folder and binds it automatically. Existing
external env variables are copied on first save; the original file is preserved.
Saving uses the project’s activity guard. Stop its executors, services and controllers
before saving, then start the services and reopen OpenCode. In opencode.json use
`{env:PROVIDER_API_KEY}` (or the corresponding variable name). Create and save a
new project first to enable its provider key editor.

Unfinished task statuses do not block per-project settings, OpenCode JSON, or
provider-key edits when execution is stopped. Saves hold admission, project and
per-task worker fences, plus controller and service-management fences. Task rows
and statuses are preserved. Whole-config CLI migration keeps its stricter check.

Controller sessions use the shared [task brief](../docs/delegation.md): inspect
relevant code before delegation, state requirements and scope, define observable
acceptance criteria, and supply verified test commands. Restart the controller
tab to load revised instructions; automated Codex prepare uses the same brief.

The project header shows its current Git branch (or detached HEAD) and polls
local and cached remote branches every five seconds. Select a branch and press
“Переключить”; choosing a remote branch creates a local tracking branch without
fetching. Switching requires stopped project/related controllers, services and
workers, a clean tracked/untracked state, and no unfinished Git operation.
Projects sharing the same worktree are fenced together. Unfinished task rows
are preserved. No stash, force checkout, commit, push or project Git hooks are
run. Ignored files that would be overwritten and branches occupied by another
worktree are refused by Git. The current HEAD/workspace is rechecked before
switching; external branch changes require refreshing the selection.

Failed assistant tasks expose **Проверить продолжение сессии** in the task card.
After manually continuing the saved OpenCode session, this explicit action claims
observation of the original attempted round without another prompt, session or
revision. The worker checks session/workspace identity, runs the frozen verifier
and collects changes against the original baseline before `awaiting_review`.
Only `assistant_error` rounds with saved session/outbound identities qualify;
infrastructure errors, closed tasks and corrupt budgets cannot be reopened this
way. An unchanged failed session can fail again. Dashboard polling remains read-only,
and acceptance is a separate controller action. Untracked-file checksum changes
remain evidence of a changed baseline, not attribution of who changed those files.

Task cards also expose **Изменить статус** for unfinished tasks, and controllers
have `set_task_status(task_id, expected_status, status, reason)` for the same
operation. Every nonterminal label is available, including transitions outside
the ordinary state machine. Accepted/closed tasks cannot be changed and cannot
be selected as targets. Admission and worker fences reject concurrent execution;
a stale current status is rejected. Status, timestamp and an audit event with
from/to/reason commit atomically. Round data, verifier, result, sessions, baseline,
revision count and writer reservations are preserved. This explicit correction
neither sends prompts nor invents completion evidence. An `awaiting_review` label
alone cannot bypass the existing complete-round and review gates of acceptance.
