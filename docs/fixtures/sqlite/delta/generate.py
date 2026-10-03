#!/usr/bin/env python3
"""Independent deterministic DDL/rows; never imports the Python reference.

Writes only the delta directory. Historical fixtures are read, never rewritten.
"""
import sys
sys.dont_write_bytecode = True
from contextlib import closing
import ast
import json
import sqlite3

from contract import HERE, PIN, DELIVERY_SQL, AUTOMATION_SQL, INDEX_SQL, snapshot

TIME = '2026-01-01T00:00:00.000+00:00'


def insert(conn, table, record):
    conn.execute(f"INSERT INTO {table} ({','.join(record)}) VALUES ({','.join('?' for _ in record)})", tuple(record.values()))


def history(conn):
    for task_id, mode, status in [('task-direct', 'direct', 'needs_user'), ('task-worktree', 'worktree', 'accepted')]:
        insert(conn, 'tasks', dict(task_id=task_id, project_id='proj', workspace='/fixture/workspace', status=status, task='Synthetic migration contract', allowed_paths='["module.py"]', test_commands='["python -m unittest"]', created_at=TIME, updated_at=TIME, execution_mode=mode, budget_json='{"max_cost_usd":1}', workflow_id='flow-1'))
        insert(conn, 'rounds', dict(task_id=task_id, project_id='proj', round_number=1, request_id='req-'+task_id, payload_hash='hash-'+task_id, kind='initial', status='needs_user' if mode == 'direct' else 'complete', attempted=1, response='Synthetic result', result_json='{"changed_paths":["module.py"]}', checkpoint_json='{"version":1}', created_at=TIME, updated_at=TIME))
        insert(conn, 'events', dict(id=11 if mode == 'direct' else 29, task_id=task_id, round_number=1, kind=status, message='Synthetic history', created_at=TIME))
    insert(conn, 'active_writers', dict(task_id='task-direct', project_id='proj', scopes_json='["module.py"]', created_at=TIME, parallel=0))
    insert(conn, 'worktrees', dict(task_id='task-worktree', path='/fixture/state/proj/worktrees/task-worktree', status='created', delivery_state='none', baseline_json='{"head":"'+'a'*40+'"}', created_at=TIME))
    insert(conn, 'worktree_quarantine', dict(entry_id='quarantine-1', original_path='/fixture/orphan', reason='synthetic orphan', found_at=TIME, status='quarantined'))


def main():
    tree = ast.parse((HERE.parent / 'generate.py').read_text())
    ddl = next(ast.literal_eval(n.value) for n in tree.body if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'V15_SCHEMA_SQL' for t in n.targets))
    expected = {'fixture_version': 1, 'source_commit': PIN, 'databases': {}, 'ownership_marker_template': {'implementation': 'rust', 'format_version': 1, 'project_id': 'proj', 'state_root': '${ISOLATED_RUST_STATE}'}}
    for name, version, owned in [('fresh-v17.sqlite', 17, False), ('owned-v15.sqlite', 15, True), ('owned-v16.sqlite', 16, True), ('owned-v17.sqlite', 17, True)]:
        path = HERE / name
        if path.exists():
            path.unlink()
        with closing(sqlite3.connect(path)) as conn, conn:
            conn.executescript(ddl)
            if owned:
                history(conn)
                insert(conn, 'meta', {'key': 'runtime_owner', 'value': 'rust'})
                insert(conn, 'meta', {'key': 'fixture_metadata', 'value': 'preserve-me'})
            if version >= 16:
                conn.execute(DELIVERY_SQL)
                if owned:
                    conn.execute("UPDATE tasks SET delivery_mode='on_accept' WHERE task_id='task-worktree'")
            if version == 17:
                conn.execute(AUTOMATION_SQL)
                conn.execute(INDEX_SQL)
            conn.execute(f'PRAGMA user_version={version}')
            insert(conn, 'meta', {'key': 'schema_version', 'value': str(version)})
        expected['databases'][name] = snapshot(path)
    (HERE / 'expected.json').write_text(json.dumps(expected, ensure_ascii=False, indent=2, sort_keys=True)+'\n')
    print('generated 4 isolated delta fixtures; historical files untouched')


if __name__ == '__main__':
    main()
