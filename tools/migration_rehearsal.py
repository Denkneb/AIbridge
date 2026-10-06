#!/usr/bin/env python3
"""Disposable two-project Rust migration/lifecycle rehearsal and bounded service soak.

Uses fresh schema v17, real OpenCode/MCP services and no user projects or Python
runtime state. Production cutover still needs a selected project and downtime.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time


def port():
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        return listener.getsockname()[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bridge', type=Path, required=True)
    parser.add_argument('--opencode', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=10)
    parser.add_argument('--interval', type=float, default=1.0)
    args = parser.parse_args()
    if not 1 <= args.samples <= 1000 or not 0.1 <= args.interval <= 60:
        parser.error('invalid bounded soak settings')
    report = {'scope': 'disposable two-project v17 migration rehearsal and short service soak', 'outcome': 'failed', 'checks': {}, 'metrics': {'readiness_failures': 0, 'samples': 0, 'startup_probe_retries': 0}}
    with tempfile.TemporaryDirectory(prefix='bridge-migration-rehearsal-') as tmp:
        root = Path(tmp)
        root.chmod(0o700)
        config = root / 'projects.toml'
        state = root / 'rust-state'
        env = dict(os.environ, PATH=str(args.opencode.resolve().parent) + os.pathsep + os.environ['PATH'],
                   NO_PROXY='127.0.0.1,localhost', no_proxy='127.0.0.1,localhost')
        for key in ['HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME']:
            directory = root / key.lower()
            directory.mkdir(mode=0o700)
            env[key] = str(directory)
        for key in list(env):
            if key.startswith('AGENT_BRIDGE_MCP_TOKEN') or key in ['OPENCODE_CONFIG', 'OPENCODE_CONFIG_CONTENT', 'OPENCODE_SERVER_PASSWORD', 'OPENCODE_SERVER_USERNAME']:
                env.pop(key)
        ports = set()
        def distinct_port():
            while True:
                value = port()
                if value not in ports:
                    ports.add(value)
                    return value
        text = ''
        for project in ['pilot', 'next']:
            (root / project).mkdir()
            text += f'[projects.{project}]\nworkspace=' + json.dumps(str(root / project)) + f'\nopencode_url="http://127.0.0.1:{distinct_port()}"\nmcp_url="http://127.0.0.1:{distinct_port()}/mcp"\npassword_file=' + json.dumps(str(root / 'secrets' / (project + '-password'))) + '\nmcp_token_file=' + json.dumps(str(root / 'secrets' / (project + '-token'))) + '\nmax_rounds=3\nexecution_mode="worktree"\n'
        config.write_text(text)
        backup = root / 'projects.backup.toml'
        backup.write_bytes(config.read_bytes())
        def cli(command, project='pilot', check=True, target=state):
            c = [str(args.bridge.resolve()), command, '--project', project, '--config', str(config), '--state-root', str(target)]
            if command in ['status', 'doctor']:
                c.append('--json')
            result = subprocess.run(c, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=45)
            if check and result.returncode:
                raise RuntimeError(command + '_failed')
            return result
        def ready(project, settle=False):
            result = cli('status', project, check=False)
            if settle:
                for _ in range(3):
                    if result.returncode == 0:
                        break
                    report['metrics']['startup_probe_retries'] += 1
                    time.sleep(.5)
                    result = cli('status', project, check=False)
            report['metrics']['samples'] += 1
            if result.returncode:
                report['metrics']['readiness_failures'] += 1
                try:
                    data = json.loads(result.stdout)
                    servers = data['projects'][0]['servers']
                    report['readiness_failure'] = {kind: {key: servers[kind].get(key) for key in ['ready', 'managed', 'state']} for kind in ['opencode', 'mcp']}
                except (ValueError, KeyError):
                    report['readiness_failure'] = {'diagnostics_unavailable': True}
                raise RuntimeError('readiness_failed')
            data = json.loads(result.stdout)
            servers = data['projects'][0]['servers']
            if not all(servers[k]['ready'] and servers[k]['managed'] for k in ['opencode', 'mcp']):
                raise RuntimeError('unmanaged_service')
        def kill_record(record):
            data = json.loads(record.read_text())
            fd = os.pidfd_open(data['pid'])
            try:
                birth = Path(f"/proc/{data['pid']}/stat").read_text().rsplit(')', 1)[1].split()[19]
                if birth != data['start'] or Path('/proc/sys/kernel/random/boot_id').read_text().strip() != data['boot_id']:
                    raise RuntimeError('crash_identity_changed')
                signal.pidfd_send_signal(fd, signal.SIGKILL)
            finally:
                os.close(fd)
        try:
            report['checks']['config_backup'] = backup.read_bytes() == config.read_bytes()
            report['checks']['readonly_preflight'] = cli('doctor', check=False).returncode != 0 and not state.exists()
            foreign = root / 'foreign-fixture'
            (foreign / 'pilot').mkdir(parents=True)
            marker = foreign / 'pilot/.agent-bridge-state.json'
            marker.write_text('{"implementation":"python"}')
            frozen = marker.read_bytes()
            report['checks']['foreign_owner_refused'] = cli('setup', check=False, target=foreign).returncode != 0 and marker.read_bytes() == frozen and not (foreign / 'pilot/state.sqlite').exists()
            started = time.monotonic()
            cli('setup')
            dbpath = state / 'pilot/state.sqlite'
            with sqlite3.connect(f'file:{dbpath}?mode=ro', uri=True) as db:
                report['checks']['fresh_v17_empty'] = db.execute('PRAGMA user_version').fetchone()[0] == 17 and db.execute('SELECT count(*) FROM tasks').fetchone()[0] == 0
                report['checks']['rust_owner'] = db.execute("SELECT value FROM meta WHERE key='runtime_owner'").fetchone()[0] == 'rust'
            report['checks']['private_credentials'] = all(p.stat().st_mode & 0o777 == 0o600 for p in (root / 'secrets').iterdir())
            report['checks']['private_namespace'] = (state / 'pilot').stat().st_mode & 0o777 == 0o700
            cli('start')
            ready('pilot', settle=True)
            report['metrics']['pilot_start_seconds'] = round(time.monotonic() - started, 3)
            records = {kind: (state / f'pilot/{kind}.process.json').read_bytes() for kind in ['opencode', 'mcp']}
            cli('start')
            report['checks']['idempotent_start'] = all((state / f'pilot/{kind}.process.json').read_bytes() == data for kind, data in records.items())
            kill_record(state / 'pilot/mcp.process.json')
            deadline = time.monotonic() + 15
            while cli('status', check=False).returncode == 0:
                if time.monotonic() > deadline:
                    raise RuntimeError('crash_not_observed')
                time.sleep(.1)
            report['checks']['crash_detected'] = True
            cli('stop')
            cli('start')
            ready('pilot', settle=True)
            report['checks']['restart_after_crash'] = True
            soak = time.monotonic()
            for _ in range(args.samples):
                ready('pilot')
                time.sleep(args.interval)
            report['metrics']['soak_seconds'] = round(time.monotonic() - soak, 3)
            cli('stop')
            report['checks']['rollback_services_stopped'] = not any((state / f'pilot/{kind}.process.json').exists() for kind in ['opencode', 'mcp'])
            config.write_bytes(backup.read_bytes())
            report['checks']['rollback_config_exact'] = config.read_bytes() == backup.read_bytes()
            cli('setup', 'next')
            cli('start', 'next')
            ready('next', settle=True)
            cli('stop', 'next')
            report['checks']['sequential_second_project'] = True
            report['outcome'] = 'passed' if all(report['checks'].values()) else 'failed'
        except (OSError, RuntimeError, ValueError, KeyError, sqlite3.Error, subprocess.SubprocessError) as exc:
            report['failure'] = str(exc) if isinstance(exc, RuntimeError) else type(exc).__name__
        finally:
            for project in ['pilot', 'next']:
                try:
                    cli('stop', project, check=False)
                except (OSError, RuntimeError, subprocess.SubprocessError):
                    pass
            for record in state.rglob('*process.json'):
                try:
                    kill_record(record)
                except (OSError, RuntimeError, ValueError, KeyError):
                    pass
        args.output.write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report), flush=True)
        return int(report['outcome'] != 'passed')


if __name__ == '__main__':
    raise SystemExit(main())
