#!/usr/bin/env python3
"""Pinned source migration parity, exclusively on synthetic temporary copies.

Does not prove Rust v16/v17 ownership enforcement: Rust remains target v15.
The reference has no Rust sidecar guard; only marker preservation is tested here.
"""
import sys
sys.dont_write_bytecode = True
from contextlib import closing, contextmanager
import copy
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import tempfile
from types import SimpleNamespace
from unittest.mock import patch

from contract import HERE, PIN, digest, snapshot
from verify import verify

REFERENCE = Path(os.environ.get('AGENT_BRIDGE_REFERENCE', '/home/denis/Python/agent_bridge')).resolve()


def source_guard():
    head = subprocess.check_output(['git', '-C', str(REFERENCE), 'rev-parse', 'HEAD'], text=True).strip()
    clean = subprocess.check_output(['git', '-C', str(REFERENCE), 'status', '--porcelain'], text=True).strip()
    if head != PIN or clean:
        raise ValueError('reference HEAD/clean-tree differs from pinned contract')


def bootstrap():
    python = REFERENCE / '.venv/bin/python'
    if python.is_file() and os.environ.get('_AB_V17_SQLITE_VERIFY') != '1':
        os.execve(str(python), [str(python), *sys.argv], dict(os.environ, PYTHONDONTWRITEBYTECODE='1', _AB_V17_SQLITE_VERIFY='1'))
    source_guard()
    sys.path.insert(0, str(REFERENCE / 'src'))
    from agent_bridge import storage
    if Path(storage.__file__).resolve() != REFERENCE / 'src/agent_bridge/storage.py' or storage.SCHEMA_VERSION != 17:
        raise ValueError('unexpected source module/schema')
    return storage


def check(condition, label):
    if not condition:
        raise ValueError(label)
    print('PASS ' + label)


def constraints(path):
    with closing(sqlite3.connect(path)) as conn, conn:
        conn.execute('PRAGMA foreign_keys=ON')
        def add(run_id, status, control=None):
            if control is None:
                conn.execute("INSERT INTO automation_runs(run_id,status,document,created_at,updated_at) VALUES (?,?, '{}','fixed','fixed')", (run_id, status))
            else:
                conn.execute("INSERT INTO automation_runs VALUES (?,?,?,'{}','fixed','fixed')", (run_id, status, control))
        for status in ('completed', 'ready', 'stopped'):
            add(status+'-1', status)
            add(status+'-2', status)
        check(conn.execute('SELECT DISTINCT control FROM automation_runs').fetchall() == [('run',)], 'automation control default run')
        for status in ('running', 'paused', 'failed', 'unknown'):
            add('active', status)
            try:
                add('second', 'running')
            except sqlite3.IntegrityError:
                pass
            else:
                raise ValueError('second unfinished automation run admitted')
            conn.execute("UPDATE automation_runs SET status='ready' WHERE run_id='active'")
            add('second', 'running')
            conn.execute("DELETE FROM automation_runs WHERE run_id IN ('active','second')")
        check(True, 'partial uniqueness excludes exactly completed/ready/stopped; release by update')
        for sql in ("INSERT INTO automation_runs VALUES ('completed-1','completed','run','{}','x','x')", "INSERT INTO automation_runs VALUES ('null-status',NULL,'run','{}','x','x')"):
            try:
                conn.execute(sql)
            except sqlite3.IntegrityError:
                pass
            else:
                raise ValueError('automation PK/NOT NULL not enforced')
        check(True, 'automation PK and NOT NULL enforced')


def main():
    try:
        storage = bootstrap()
        expected = verify()
        fixtures = list(HERE.glob('*.sqlite')) + list(HERE.parent.glob('*.sqlite'))
        hashes = {str(p): digest(p) for p in fixtures}
        with tempfile.TemporaryDirectory(prefix='aibridge-sqlite-v17-') as temporary:
            root = Path(temporary).resolve()
            original_connect = sqlite3.connect
            def confined_connect(database, *args, **kwargs):
                # Source storage can connect only inside this temporary root.
                path = Path(database).resolve()
                if not path.is_relative_to(root):
                    raise AssertionError('source SQLite connection outside isolated fixture copies')
                return original_connect(database, *args, **kwargs)
            @contextmanager
            def source_only():
                with patch.object(sqlite3, 'connect', confined_connect), patch.object(socket, 'socket', side_effect=AssertionError('network forbidden')), patch.object(socket, 'create_connection', side_effect=AssertionError('network forbidden')):
                    yield
            fresh = root / 'fresh' / 'state.sqlite'
            with source_only():
                storage.Storage(SimpleNamespace(db_path=fresh, project_id='proj', allow_parallel_writers=False)).initialize()
            check(snapshot(fresh) == expected['databases']['fresh-v17.sqlite'], 'fresh initialize exactly matches v17 fixture')
            for name in ('owned-v15.sqlite', 'owned-v16.sqlite', 'owned-v17.sqlite'):
                directory = root / name.removesuffix('.sqlite')
                directory.mkdir()
                path = directory / 'state.sqlite'
                shutil.copyfile(HERE / name, path)
                marker = directory / '.agent-bridge-state.json'
                marker.write_text(json.dumps({**expected['ownership_marker_template'], 'state_root': str(root)}))
                marker_hash = digest(marker)
                before = snapshot(path)
                instance = storage.Storage(SimpleNamespace(db_path=path, project_id='proj', allow_parallel_writers=False))
                with source_only():
                    instance.initialize()
                after = snapshot(path)
                wanted = copy.deepcopy(before['rows'])
                for row in wanted['meta']:
                    if row['key'] == 'schema_version':
                        row['value'] = '17'
                if before['version'] < 16:
                    for row in wanted['tasks']:
                        row['delivery_mode'] = 'manual'
                wanted.setdefault('automation_runs', [])
                check(after['rows'] == wanted, name + ': every history row/id and ownership metadata preserved; legacy manual / explicit on_accept retained')
                check(after['schema'] == expected['databases']['fresh-v17.sqlite']['schema'] and after['indexes'] == expected['databases']['fresh-v17.sqlite']['indexes'], name + ': additive target schema/indexes exact')
                with source_only():
                    instance.initialize()
                check(snapshot(path) == after and digest(marker) == marker_hash, name + ': reinitialize idempotent and sidecar unchanged')
                constraints(path)
            # Fail after the delivery column and automation table have been added.
            for name in ('owned-v15.sqlite', 'owned-v16.sqlite'):
                path = root / ('rollback-' + name)
                shutil.copyfile(HERE / name, path)
                before = snapshot(path)
                def failing_connect(database, *args, **kwargs):
                    conn = confined_connect(database, *args, **kwargs)
                    conn.set_authorizer(lambda action, arg1, arg2, db, trigger: sqlite3.SQLITE_DENY if action == sqlite3.SQLITE_CREATE_INDEX and arg1 == 'ux_automation_unfinished' else sqlite3.SQLITE_OK)
                    return conn
                with source_only(), patch.object(sqlite3, 'connect', failing_connect):
                    try:
                        storage.Storage(SimpleNamespace(db_path=path, project_id='proj', allow_parallel_writers=False)).initialize()
                    except sqlite3.DatabaseError as exc:
                        check('authorized' in str(exc), name + ': failure injected at automation index creation')
                    else:
                        raise ValueError('injected migration unexpectedly succeeded')
                check(snapshot(path) == before, name + ': rollback restores DDL, history, metadata and both version markers')
                with source_only():
                    storage.Storage(SimpleNamespace(db_path=path, project_id='proj', allow_parallel_writers=False)).initialize()
                check(snapshot(path)['version'] == 17, name + ': retry after rollback succeeds')
            path = root / 'unsupported.sqlite'
            shutil.copyfile(HERE / 'owned-v17.sqlite', path)
            with closing(sqlite3.connect(path)) as conn, conn:
                conn.execute('PRAGMA user_version=18')
            before = snapshot(path)
            with source_only():
                try:
                    storage.Storage(SimpleNamespace(db_path=path, project_id='proj', allow_parallel_writers=False)).initialize()
                except storage.StorageError:
                    pass
                else:
                    raise ValueError('future schema accepted')
            check(snapshot(path) == before, 'unsupported version rejected without logical mutation')
        source_guard()
        check(hashes == {str(p): digest(p) for p in fixtures}, 'all delta and historical fixture bytes unchanged')
        print('ok: v17 source parity; fresh, additive 15/16 upgrades, idempotence, constraints, rollback; no skips')
        return 0
    except (ValueError, OSError, sqlite3.Error, AssertionError) as exc:
        print('FAIL ' + str(exc), file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
