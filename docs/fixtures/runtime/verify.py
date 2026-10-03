#!/usr/bin/env python3
"""v15 runtime parity on disposable fixture DBs; no network or process starts.

Snapshot expectations are explicit field projections (volatile age/size omitted).
Hook context and readiness envelopes are exact. DB bytes and fixture-tree files
are checked before/after every read; SQLite transient sidecars are excluded.
"""
import argparse
from contextlib import redirect_stdout, redirect_stderr
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import shutil
import sqlite3
import sys
import tempfile
from types import SimpleNamespace
from unittest import mock

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('security_verify', HERE.parent / 'security/verify.py')
security = importlib.util.module_from_spec(spec)
spec.loader.exec_module(security)
TS = '2026-01-01T00:00:00.000+00:00'
SECRET = 'RAW_TASK_SECRET_DO_NOT_EXPOSE'


def fingerprint(root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest() if p.is_file() else '<directory>'
            for p in root.rglob('*') if not p.name.endswith(('-wal', '-shm'))}


def setup(root, arg):
    from agent_bridge.config import ProjectConfig
    cfg = ProjectConfig('proj', root / 'workspace', '127.0.0.1', 19000,
                        root / 'password', 3, root / 'projects.toml', root / 'state',
                        mcp_url='http://127.0.0.1:19002/mcp',
                        allow_parallel_writers=arg.get('parallel', False),
                        execution_mode='worktree' if arg.get('worktree') or arg.get('parallel') else 'direct')
    cfg.workspace.mkdir()
    if arg.get('db') == 'missing':
        return cfg
    cfg.state_dir.mkdir(parents=True)
    if arg.get('db') == 'corrupt':
        cfg.db_path.write_bytes(b'invalid sqlite fixture')
        return cfg
    shutil.copyfile(HERE.parent / 'sqlite/empty-v15.sqlite', cfg.db_path)
    with sqlite3.connect(cfg.db_path) as conn:
        conn.execute('PRAGMA journal_mode=DELETE')
        if 'version' in arg:
            conn.execute('PRAGMA user_version=' + str(int(arg['version'])))
        for i, status in enumerate(arg.get('statuses', []), 1):
            task_id = arg.get('task_id', f'task-{i}')
            ts = arg.get('timestamp', TS)
            conn.execute('INSERT INTO tasks(task_id,project_id,workspace,status,task,allowed_paths,test_commands,created_at,updated_at,execution_mode) VALUES(?,?,?,?,?,?,?,?,?,?)',
                         (task_id, 'proj', str(cfg.workspace), status, SECRET, '[]', '[]', ts, ts, cfg.execution_mode))
            if status in ('implementing', 'revising', 'awaiting_review'):
                conn.execute('INSERT INTO active_writers VALUES(?,?,?,?,?)', (task_id,'proj','[]',ts,int(arg.get('parallel',False))))
            conn.execute('INSERT INTO rounds(task_id,project_id,round_number,request_id,payload_hash,kind,status,verifier_state,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?)',
                         (task_id,'proj',1,f'req-{i}','hash','implement','completed',arg.get('state'),ts,ts))
            if arg.get('worktree'):
                conn.execute('INSERT INTO worktrees(task_id,path,status,server_port,delivery_state) VALUES(?,?,?,?,?)', (task_id,str(root / 'checkout'),'ready',19001,'built'))
    return cfg


def observe(case, root):
    from agent_bridge import cli, diagnostics, mcp_server, runtime, storage
    op, arg, expected = case['operation'], case['input'], case['expect']
    if op == 'progress':
        return storage._safe_verifier_progress(arg['raw'])
    if op == 'phase':
        return mcp_server._inflight_phase(SimpleNamespace(verifier_state=arg['state']))
    cfg = setup(root, arg)
    before = fingerprint(root)
    if op == 'snapshot':
        snap = diagnostics.collect_project_snapshot(cfg, probe_opencode=False)
        result = {key: snap[key] for key in expected}
        assert snap['opencode_session'] is None
        assert SECRET not in json.dumps(snap)
    elif op == 'hook':
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(cli, '_load', return_value=cfg), mock.patch.object(storage.Storage, 'initialize', side_effect=AssertionError('hook initialized storage')) as initialize, mock.patch('socket.socket.connect', side_effect=AssertionError('hook accessed network')) as connect, redirect_stdout(out), redirect_stderr(err):
            code = cli.cmd_hook_status(argparse.Namespace())
        initialize.assert_not_called()
        connect.assert_not_called()
        assert not err.getvalue()
        payload = json.loads(out.getvalue()) if out.getvalue() else None
        if payload:
            assert set(payload) == {'hookSpecificOutput'}
            assert set(payload['hookSpecificOutput']) == {'hookEventName', 'additionalContext'}
            assert payload['hookSpecificOutput']['hookEventName'] == 'UserPromptSubmit'
        result = dict(exit_code=code, context=payload['hookSpecificOutput']['additionalContext'] if payload else None)
    elif op == 'status_json':
        out = io.StringIO()
        record = dict(state=arg['record'], managed=arg['record']=='live')
        # Health and ownership-record observations are deterministic doubles.
        # Report construction, worktree summary and DB snapshot use source code.
        with mock.patch.object(runtime, '_readonly_record_status', return_value=record), mock.patch.object(runtime, '_ready', return_value=arg['ready']), mock.patch.object(diagnostics, 'probe_opencode_session', return_value={'reachable':arg['ready'],'workspace_ok':arg['ready']}), redirect_stdout(out):
            code = runtime.status_all({'proj':cfg}, json_output=True)
        payload = json.loads(out.getvalue())
        assert set(payload) == {'schema_version', 'projects'}
        assert len(payload['projects']) == 1
        entry = payload['projects'][0]
        assert set(entry) == {'project_id','servers','ready','snapshot','worktree'}
        assert entry['project_id'] == 'proj'
        assert entry['snapshot']['error'] is None
        assert entry['worktree'] == {'present':False}
        for server in entry['servers'].values():
            assert server == dict(ready=expected['ready'], managed=expected['managed'], process_record=expected['record'])
        assert set(entry['servers']) == {'opencode','mcp'}
        result = dict(schema_version=payload['schema_version'],ready=entry['ready'],exit_code=code,managed=record['managed'],record=record['state'])
    else:
        raise ValueError('unknown operation')
    assert fingerprint(root) == before, 'read mutated fixture files'
    assert SECRET not in json.dumps(result)
    return result


def main():
    security.bootstrap()
    from agent_bridge import diagnostics, storage
    corpus = json.loads((HERE.parent / 'runtime-cases.json').read_text())
    assert corpus['source']['commit'] == security.PIN
    assert corpus['schema_version'] == storage.SCHEMA_VERSION
    assert corpus['diagnostics_schema_version'] == diagnostics.DIAGNOSTICS_SCHEMA_VERSION
    ids = [case['id'] for case in corpus['cases']]
    assert ids and len(ids) == len(set(ids))
    failures = []
    for case in corpus['cases']:
        try:
            with tempfile.TemporaryDirectory(prefix='ab-runtime-') as temp:
                result = observe(case, Path(temp))
            assert result == case['expect'], f"expected {case['expect']!r}, got {result!r}"
        except Exception as exc:
            failures.append(case['id'] + ': ' + str(exc))
    for failure in failures:
        print('FAIL ' + failure, file=sys.stderr)
    print(f'runtime: {len(ids)-len(failures)}/{len(ids)} cases passed (no skips)')
    return bool(failures)


if __name__ == '__main__':
    sys.dont_write_bytecode = True
    raise SystemExit(main())
