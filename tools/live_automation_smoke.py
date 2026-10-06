#!/usr/bin/env python3
"""Bounded two-step real Codex/OpenCode workflow in disposable Rust-owned state.

Auth inputs are copied privately, never printed or changed. The plan changes only
fixture text files; no user project or Python runtime state participates.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bridge', type=Path, required=True)
    parser.add_argument('--opencode', type=Path, required=True)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--opencode-auth', type=Path, required=True)
    parser.add_argument('--codex-auth', type=Path, required=True)
    parser.add_argument('--codex-model', default='gpt-6.1-sol')
    parser.add_argument('--opencode-model', default='opencode-go/minimax-m2.7')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    report = {'outcome': 'failed', 'scope': 'real two-step Codex prepare/review and OpenCode worktree workflow', 'checks': {}}
    with tempfile.TemporaryDirectory(prefix='bridge-live-automation-') as tmp:
        root = Path(tmp)
        root.chmod(0o700)
        workspace = root / 'main'
        workspace.mkdir()
        env = dict(os.environ, PATH=os.pathsep.join([str(args.codex.resolve().parent), str(args.opencode.resolve().parent), os.environ['PATH']]),
                   AB_ROUND_DEADLINE='180', AB_HTTP_TIMEOUT='10', AB_POLL_INTERVAL='0.25',
                   NO_PROXY='127.0.0.1,localhost', no_proxy='127.0.0.1,localhost',
                   GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_NOSYSTEM='1', GIT_OPTIONAL_LOCKS='0',
                   OPENCODE_CONFIG_CONTENT=json.dumps({'permission': {'external_directory': 'deny'}}))
        for key in ['HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME', 'CODEX_HOME']:
            directory = root / key.lower()
            directory.mkdir(mode=0o700)
            env[key] = str(directory)
        for key in list(env):
            if key.startswith('AGENT_BRIDGE_MCP_TOKEN') or key in ['OPENCODE_SERVER_PASSWORD', 'OPENCODE_SERVER_USERNAME', 'OPENCODE_CONFIG', 'CODEX_THREAD_ID']:
                env.pop(key)
        opencode_auth = Path(env['XDG_DATA_HOME']) / 'opencode/auth.json'
        opencode_auth.parent.mkdir(mode=0o700)
        opencode_auth.write_bytes(args.opencode_auth.read_bytes())
        opencode_auth.chmod(0o600)
        codex_auth = Path(env['CODEX_HOME']) / 'auth.json'
        codex_auth.write_bytes(args.codex_auth.read_bytes())
        codex_auth.chmod(0o600)
        (workspace / 'check.py').write_text("from pathlib import Path\nimport sys\nassert Path('one.txt').read_bytes()==b'start\\n'\nif sys.argv[1]=='final': assert Path('two.txt').read_bytes()==b'start-next\\n'\n")
        def execute(argv, timeout=30, check=True):
            result = subprocess.run(argv, cwd=workspace, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
            if check and result.returncode:
                raise RuntimeError('command_failed')
            return result
        for argv in [['git', 'init', '-q'], ['git', 'add', 'check.py'], ['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture']]:
            execute(argv)
        head = execute(['git', 'rev-parse', 'HEAD']).stdout
        index = (workspace / '.git/index').read_bytes()
        config = root / 'projects.toml'
        config.write_text('[projects.proof]\nworkspace=' + json.dumps(str(workspace)) + '\nopencode_url="http://127.0.0.1:4199"\npassword_file=' + json.dumps(str(root / 'password')) + '\nmax_rounds=3\nexecution_mode="worktree"\nopencode_model=' + json.dumps(args.opencode_model) + '\n')
        (root / 'password').write_text(os.urandom(24).hex())
        (root / 'password').chmod(0o600)
        steps = [
            {'id': 'one', 'task': "Create only relative one.txt with exact UTF-8 bytes start followed by newline. Do not commit. Do not read parent directories. check.py is a trusted read-only verifier; run python3 -B check.py one.", 'allowed_paths': ['one.txt'], 'test_commands': ['python3 -B check.py one'], 'acceptance_criteria': ['one.txt bytes are exactly start followed by newline']},
            {'id': 'two', 'depends_on': ['one'], 'task': "Read inherited relative one.txt. Create only relative two.txt with exact UTF-8 bytes start-next followed by newline. Do not modify one.txt or check.py, do not commit or read parent directories. Run python3 -B check.py final.", 'allowed_paths': ['two.txt'], 'test_commands': ['python3 -B check.py final'], 'acceptance_criteria': ['Inherited one.txt remains start followed by newline; two.txt is start-next followed by newline']},
        ]
        plan = root / 'plan.json'
        plan.write_text(json.dumps({'version': 1, 'goal': 'Prove accepted predecessor inheritance and cumulative delivery with real models', 'steps': steps, 'final_test_commands': ['python3 -B check.py final'], 'max_seconds': 600, 'codex_timeout': 180, 'codex_model': args.codex_model, 'max_revisions': 1, 'delivery': 'apply'}))
        base = ['--project', 'proof', '--config', str(config), '--state-root', str(root / 'state')]
        run = None
        try:
            launch = execute([str(args.bridge.resolve()), 'launch-codex', '--auto', '--plan', str(plan), *base])
            launched = json.loads(launch.stdout)
            run = launched['run_id']
            report['checks']['detached_launch'] = True
            deadline = time.monotonic() + 650
            last_phase = None
            while time.monotonic() < deadline:
                status = json.loads(execute([str(args.bridge.resolve()), 'automation-status', '--run', run, *base]).stdout)
                phase = (status.get('status'), status.get('phase'), status.get('index'))
                if phase != last_phase:
                    print(json.dumps({'progress': phase}), flush=True)
                    last_phase = phase
                if status['status'] in ['completed', 'ready', 'blocked', 'stopped', 'failed']:
                    report['status'] = status['status']
                    if status.get('blocker_code'):
                        report['blocker_code'] = status['blocker_code']
                    break
                time.sleep(1)
            else:
                raise RuntimeError('workflow_deadline')
            if report['status'] != 'completed':
                # Only safe classifications survive; model logs remain private.
                for log in (root / 'state/proof/automation').glob('*/codex/*.log'):
                    text = log.read_text(errors='replace')
                    for needle, label in [('401', 'model_authentication'), ('403', 'model_access'), ('quota', 'model_quota'), ('error sending request', 'model_transport')]:
                        if needle in text:
                            report['model_failure'] = label
                raise RuntimeError('workflow_not_completed')
            execute(['python3', '-B', 'check.py', 'final'])
            report['checks']['cumulative_bytes'] = True
            report['checks']['head_preserved'] = execute(['git', 'rev-parse', 'HEAD']).stdout == head
            report['checks']['index_preserved'] = (workspace / '.git/index').read_bytes() == index
            with sqlite3.connect(f"file:{root / 'state/proof/state.sqlite'}?mode=ro", uri=True) as db:
                rows = db.execute('SELECT status FROM tasks').fetchall()
                report['checks']['three_accepted_tasks'] = len(rows) == 3 and all(r[0] == 'accepted' for r in rows)
                report['round_count'] = db.execute('SELECT count(*) FROM rounds').fetchone()[0]
            report['outcome'] = 'passed' if all(report['checks'].values()) else 'failed'
        except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as exc:
            report['failure'] = str(exc) if isinstance(exc, RuntimeError) else type(exc).__name__
        finally:
            if run:
                try:
                    execute([str(args.bridge.resolve()), 'automation-stop', '--run', run, *base], timeout=10, check=False)
                except (OSError, RuntimeError, subprocess.SubprocessError):
                    pass
            # Kill only children whose private records still match /proc birth identity.
            for record in (root / 'state').rglob('*process.json'):
                try:
                    data = json.loads(record.read_text())
                    pid = int(data['pid'])
                    fd = os.pidfd_open(pid)
                    try:
                        stat = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
                        birth = stat[19]
                        boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
                        if str(data.get('start', '')) == birth and data.get('boot_id') == boot:
                            signal.pidfd_send_signal(fd, signal.SIGTERM)
                    finally:
                        os.close(fd)
                except (OSError, ValueError, KeyError):
                    pass
            opencode_auth.unlink(missing_ok=True)
            codex_auth.unlink(missing_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report), flush=True)
        return int(report['outcome'] != 'passed')


if __name__ == '__main__':
    raise SystemExit(main())
