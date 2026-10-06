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
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='aibridge-desktop-smoke-') as tmp:
        root = Path(tmp)
        main = root / 'main'
        main.mkdir()
        config = root / 'projects.toml'
        config.write_text('[projects.proof]\nworkspace=' + json.dumps(str(main)) +
                          '\nopencode_url="http://127.0.0.1:4199"\npassword_file=' +
                          json.dumps(str(root / 'password')) + '\nmax_rounds=3\n')
        env = dict(os.environ)
        env['AIBRIDGE_DESKTOP_SMOKE_RESULT'] = str(root / 'result.json')
        # WebKit bubblewrap cannot create a nested sandbox inside this build
        # container. This override is scoped solely to this disposable smoke.
        env['WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS'] = '1'
        env['LIBGL_ALWAYS_SOFTWARE'] = '1'
        env['GDK_BACKEND'] = 'x11'
        xvfb = 'Xvfb'
        if args.sysroot:
            xvfb = str(args.sysroot / 'usr/bin/Xvfb')
            env['LD_LIBRARY_PATH'] = str(args.sysroot / 'usr/lib/x86_64-linux-gnu')
            env['WEBKIT_EXEC_PATH'] = str(args.sysroot / 'usr/lib/x86_64-linux-gnu/webkit2gtk-4.1')
        env['DISPLAY'] = ':193'
        with (root / 'xvfb.log').open('w') as xlog, (root / 'desktop.log').open('w') as log:
            server = subprocess.Popen([xvfb, env['DISPLAY'], '-screen', '0', '1280x900x24', '-nolisten', 'tcp'], env=env, stdout=xlog, stderr=xlog)
            desktop = None
            try:
                deadline = time.monotonic() + 10
                while not Path('/tmp/.X11-unix/X193').exists():
                    if server.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError('virtual_display_unavailable')
                    time.sleep(.05)
                desktop = subprocess.Popen([str(args.desktop.resolve()), '--config', str(config), '--state-root', str(root / 'state')], env=env, stdout=log, stderr=log)
                deadline = time.monotonic() + 50
                while not (root / 'result.json').exists():
                    if desktop.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError('desktop_webview_smoke_unavailable')
                    time.sleep(.1)
                report = json.loads((root / 'result.json').read_text())
                report.update(scope='actual X11 Tauri WebView, React, IPC and PTY; no real models', state_uninitialized=not (root / 'state').exists())
                if args.screenshot:
                    subprocess.run(['import', '-window', 'root', str(args.screenshot.resolve())], env=env, check=True, timeout=5)
                code = desktop.wait(timeout=10)
                report['exit_code'] = code
                args.output.write_text(json.dumps(report, indent=2) + '\n')
                print(json.dumps(report))
                return int(not report['passed'] or code != 0)
            except RuntimeError as exc:
                # Fixed labels only. Private fixture logs are not persisted.
                report = {'passed': False, 'failure': str(exc), 'exit_code': desktop.poll() if desktop else None}
                Path('/tmp/aibridge-desktop-smoke.log').write_bytes((root / 'desktop.log').read_bytes())
                Path('/tmp/aibridge-xvfb-smoke.log').write_bytes((root / 'xvfb.log').read_bytes())
                args.output.write_text(json.dumps(report, indent=2) + '\n')
                print(json.dumps(report))
                return 1
            finally:
                for process in (desktop, server):
                    if process and process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()


if __name__ == '__main__':
    raise SystemExit(main())
