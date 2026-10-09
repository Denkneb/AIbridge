#!/usr/bin/env python3
"""Run the smoke-feature desktop in a disposable project and actual X11 WebView.

No providers or user state are used. Supply a desktop-smoke build. Optional
--sysroot enables dependencies unpacked under /tmp instead of system installs.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--desktop', type=Path, required=True)
    parser.add_argument('--sysroot', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--screenshot', type=Path)
    parser.add_argument('--display-backend', choices=['x11', 'wayland'], default='x11')
    parser.add_argument('--weston', type=Path, help='headless Weston executable for Wayland')
    parser.add_argument('--live-tui', action='store_true', help='launch installed Codex/OpenCode in the actual pane with isolated homes')
    parser.add_argument('--ibus', action='store_true', help='use a private IBus input method for native keyboard checks')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='aibridge-desktop-smoke-') as tmp:
        root = Path(tmp)
        main = root / 'main'
        main.mkdir()
        subprocess.run(['git', '-C', str(main), 'init', '-b', 'main'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(['git', '-C', str(main), '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', '-c', 'core.hooksPath=/dev/null', 'commit', '--allow-empty', '-m', 'fixture'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(['git', '-C', str(main), 'branch', 'feature'], check=True)
        subprocess.run(['git', 'init', '--bare', str(root / 'remote.git')], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(['git', '-C', str(main), 'remote', 'add', 'origin', str(root / 'remote.git')], check=True)
        config = root / 'projects.toml'
        config.write_text('[projects.proof]\nworkspace=' + json.dumps(str(main)) +
                          '\nopencode_url="http://127.0.0.1:4199"\npassword_file=' +
                          json.dumps(str(root / 'password')) + '\nmax_rounds=3\n')
        env = dict(os.environ)
        if args.live_tui:
            env['AIBRIDGE_DESKTOP_LIVE_TUI'] = '1'
        if args.live_tui or args.ibus:
            for key in ['HOME', 'CODEX_HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME']:
                directory = root / key.lower()
                directory.mkdir(mode=0o700)
                env[key] = str(directory)
            for key in list(env):
                if key.startswith('AGENT_BRIDGE_MCP_TOKEN') or key in ['OPENCODE_SERVER_PASSWORD', 'OPENCODE_SERVER_USERNAME', 'OPENCODE_CONFIG', 'OPENCODE_CONFIG_CONTENT', 'CODEX_THREAD_ID']:
                    env.pop(key)
            env['AIBRIDGE_CLI'] = str(Path('target/debug/agent-bridge').resolve())
        env['AIBRIDGE_DESKTOP_SMOKE_RESULT'] = str(root / 'result.json')
        if not args.live_tui:
            # Reproduce a failed console that leaves an owned, unresponsive
            # OpenCode daemon alive. Other CLI commands use the real bridge.
            fixture_cli = root / 'fixture-cli'
            fixture_cli.write_text('''#!/usr/bin/env python3
import json, os, pathlib, signal, subprocess, sys, tomllib
real_cli = ''' + repr(str(Path('target/debug/agent-bridge').resolve())) + '''
if sys.argv[1] != 'launch-opencode':
    os.execv(real_cli, [real_cli, *sys.argv[1:]])
def argument(name):
    return sys.argv[sys.argv.index(name) + 1]
project = argument('--project')
settings = tomllib.loads(pathlib.Path(argument('--config')).read_text())['projects'][project]
state = pathlib.Path(argument('--state-root')) / project
subprocess.run([real_cli, 'setup', *sys.argv[2:]], check=True, stdout=subprocess.DEVNULL)
child = subprocess.Popen([sys.executable, '-c', "import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(90)"], start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
raw = pathlib.Path(f'/proc/{child.pid}/stat').read_text().rsplit(') ', 1)[1].split()
endpoint = settings['opencode_url']
record = dict(pid=child.pid, start=raw[19], boot_id=pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip(), project_id=project, task_id='00000000-0000-0000-0000-000000000000', checkout=settings['workspace'], kind='opencode', port=int(endpoint.rsplit(':', 1)[1]), endpoint=endpoint)
path = state / 'opencode.process.json'
path.write_text(json.dumps(record)); path.chmod(0o600)
print(f'FAILED_CONSOLE_SERVER_PID:{child.pid}', flush=True)
sys.exit(1)
''')
            fixture_cli.chmod(0o700)
            env['AIBRIDGE_CLI'] = str(fixture_cli)
        if args.display_backend == 'x11':
            env['AIBRIDGE_DESKTOP_NATIVE_KEYS'] = str(Path(__file__).with_name('desktop_keyboard.py').resolve())
        # WebKit bubblewrap cannot create a nested sandbox inside this build
        # container. This override is scoped solely to this disposable smoke.
        env['WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS'] = '1'
        env['LIBGL_ALWAYS_SOFTWARE'] = '1'
        env['GDK_BACKEND'] = args.display_backend
        xvfb = 'Xvfb'
        if args.sysroot:
            xvfb = str(args.sysroot / 'usr/bin/Xvfb')
            env['LD_LIBRARY_PATH'] = os.pathsep.join([str(args.sysroot / 'usr/lib/x86_64-linux-gnu'), str(args.sysroot / 'usr/lib/x86_64-linux-gnu/weston')])
            env['WEBKIT_EXEC_PATH'] = str(args.sysroot / 'usr/lib/x86_64-linux-gnu/webkit2gtk-4.1')
        if args.display_backend == 'wayland':
            runtime = root / 'wayland'
            runtime.mkdir(mode=0o700)
            env['XDG_RUNTIME_DIR'] = str(runtime)
            env['WAYLAND_DISPLAY'] = 'bridge-proof'
            env['DISPLAY'] = ':194'
            weston = str(args.weston or (args.sysroot / 'usr/bin/weston' if args.sysroot else 'weston'))
            server_argv = [weston, '--backend=x11-backend.so', '--use-pixman', '--socket=bridge-proof', '--idle-time=0', '--width=1280', '--height=900', '--no-config', '--shell=kiosk-shell.so']
            if args.sysroot:
                env['WESTON_MODULE_MAP'] = ';'.join([
                    'x11-backend.so=' + str(args.sysroot / 'usr/lib/x86_64-linux-gnu/libweston-9/x11-backend.so'),
                    'kiosk-shell.so=' + str(args.sysroot / 'usr/lib/x86_64-linux-gnu/weston/kiosk-shell.so')])
            ready_path = runtime / 'bridge-proof'
        else:
            env['DISPLAY'] = ':193'
            server_argv = [xvfb, env['DISPLAY'], '-screen', '0', '1280x900x24', '-nolisten', 'tcp']
            ready_path = Path('/tmp/.X11-unix/X193')
        with (root / 'xvfb.log').open('w') as xlog, (root / 'desktop.log').open('w') as log:
            display_server = server = desktop = input_method = None
            try:
                if args.display_backend == 'wayland':
                    display_server = subprocess.Popen([xvfb, env['DISPLAY'], '-screen', '0', '1280x900x24', '-nolisten', 'tcp'], env=env, stdout=xlog, stderr=xlog)
                    deadline = time.monotonic() + 10
                    while not Path('/tmp/.X11-unix/X194').exists():
                        if display_server.poll() is not None or time.monotonic() > deadline:
                            raise RuntimeError('wayland_virtual_input_display_unavailable')
                        time.sleep(.05)
                server = subprocess.Popen(server_argv, env=env, stdout=xlog, stderr=xlog)
                deadline = time.monotonic() + 10
                while not ready_path.exists():
                    if server.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError('virtual_display_unavailable')
                    time.sleep(.05)
                if args.ibus:
                    env['IBUS_ADDRESS'] = 'unix:path=' + str(root / 'ibus.socket')
                    env['GTK_IM_MODULE'] = 'ibus'
                    env['AIBRIDGE_DESKTOP_IBUS_KEYS'] = '1'
                    input_method = subprocess.Popen(['ibus-daemon', '--single', '--xim', '--address', env['IBUS_ADDRESS']], env=env, stdout=log, stderr=log)
                    deadline = time.monotonic() + 10
                    while not (root / 'ibus.socket').exists():
                        if input_method.poll() is not None or time.monotonic() > deadline:
                            raise RuntimeError('private_ibus_unavailable')
                        time.sleep(.05)
                desktop = subprocess.Popen([str(args.desktop.resolve()), '--config', str(config), '--state-root', str(root / 'state')], env=env, stdout=log, stderr=log)
                deadline = time.monotonic() + (110 if args.live_tui else 50)
                while not (root / 'result.json').exists():
                    if desktop.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError('desktop_webview_smoke_unavailable')
                    time.sleep(.1)
                report = json.loads((root / 'result.json').read_text())
                report.update(scope=f'actual {args.display_backend} Tauri WebView, React, IPC and PTY; no real models', state_uninitialized=not (root / 'state').exists(), live_tui=args.live_tui)
                if args.screenshot and args.display_backend == 'x11':
                    subprocess.run(['import', '-window', 'root', str(args.screenshot.resolve())], env=env, check=True, timeout=5)
                code = desktop.wait(timeout=10)
                report['exit_code'] = code
                def process_alive(pid):
                    try:
                        return Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].split()[0] not in ('Z', 'X')
                    except FileNotFoundError:
                        return False
                exit_processes = report.get('checks', {}).pop('exit_processes', [])
                report['checks']['exit_cleans_active_terminal_tree'] = bool(exit_processes) and all(not process_alive(pid) for pid in exit_processes)
                report['passed'] = report['passed'] and report['checks']['exit_cleans_active_terminal_tree']
                args.output.write_text(json.dumps(report, indent=2) + '\n')
                print(json.dumps(report))
                return int(not report['passed'] or code != 0)
            except (RuntimeError, OSError, ValueError, subprocess.SubprocessError) as exc:
                # Fixed labels only. Debug fixture logs remain under /tmp.
                report = {'passed': False, 'failure': str(exc) if isinstance(exc, RuntimeError) else type(exc).__name__, 'exit_code': desktop.poll() if desktop else None}
                Path('/tmp/aibridge-desktop-smoke.log').write_bytes((root / 'desktop.log').read_bytes())
                Path('/tmp/aibridge-xvfb-smoke.log').write_bytes((root / 'xvfb.log').read_bytes())
                args.output.write_text(json.dumps(report, indent=2) + '\n')
                print(json.dumps(report))
                return 1
            finally:
                for process in (desktop, input_method, server, display_server):
                    if process and process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()


if __name__ == '__main__':
    raise SystemExit(main())
