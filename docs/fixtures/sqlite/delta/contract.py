"""Shared stdlib snapshot format for additive schema 16/17 fixtures."""
from contextlib import closing
import hashlib
import json
from pathlib import Path
import re
import sqlite3

HERE = Path(__file__).resolve().parent
PIN = 'e52a46158cbeb4f3ae35063d395c05ea0ce144bc'
DELIVERY_SQL = "ALTER TABLE tasks ADD COLUMN delivery_mode TEXT NOT NULL DEFAULT 'manual'"
AUTOMATION_SQL = "CREATE TABLE automation_runs (run_id TEXT PRIMARY KEY, status TEXT NOT NULL, control TEXT NOT NULL DEFAULT 'run', document TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)"
INDEX_SQL = "CREATE UNIQUE INDEX ux_automation_unfinished ON automation_runs((1)) WHERE status NOT IN ('completed','ready','stopped')"


def normalized_sql(value):
    return re.sub(r'\s+', '', value).rstrip(';').lower()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def snapshot(path):
    """Exact defaults, FK actions, index SQL/expression and raw history rows."""
    for suffix in ('-wal', '-shm', '-journal'):
        if Path(str(path) + suffix).exists():
            raise ValueError(f'fixture sidecar present: {path.name}{suffix}')
    with closing(sqlite3.connect(path.resolve().as_uri() + '?mode=ro&immutable=1', uri=True)) as conn:
        conn.row_factory = sqlite3.Row
        conn.execute('PRAGMA foreign_keys=ON')
        for pragma in ('integrity_check', 'quick_check'):
            if [tuple(r) for r in conn.execute('PRAGMA ' + pragma)] != [('ok',)]:
                raise ValueError(f'{path.name}: {pragma} failed')
        if conn.execute('PRAGMA foreign_key_check').fetchall():
            raise ValueError('foreign key violation')
        tables = [r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")]
        schema, rows = {}, {}
        for table in tables:
            schema[table] = {
                'columns': sorted([dict(r) for r in conn.execute(f'PRAGMA table_info({table})')], key=lambda r: r['name']),
                'foreign_keys': [dict(r) for r in conn.execute(f'PRAGMA foreign_key_list({table})')],
            }
            # Column ordinal can differ between fresh and additive layouts.
            for column in schema[table]['columns']:
                column.pop('cid')
            rows[table] = sorted([dict(r) for r in conn.execute(f'SELECT * FROM {table}')], key=lambda r: json.dumps(r, sort_keys=True))
        indexes = {}
        for name, table, sql in conn.execute("SELECT name,tbl_name,sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL ORDER BY name"):
            info = next(r for r in conn.execute(f'PRAGMA index_list({table})') if r['name'] == name)
            indexes[name] = {'table': table, 'unique': info['unique'], 'partial': info['partial'], 'sql': normalized_sql(sql.replace('IF NOT EXISTS ', '')), 'xinfo': [dict(r) for r in conn.execute(f'PRAGMA index_xinfo({name})')]}
        return {'version': conn.execute('PRAGMA user_version').fetchone()[0], 'schema': schema, 'indexes': indexes, 'rows': rows}


def load_expected(directory=HERE):
    expected = json.loads((directory / 'expected.json').read_text())
    if expected['source_commit'] != PIN or expected['fixture_version'] != 1:
        raise ValueError('expectation source/version mismatch')
    if set(expected['databases']) != {'fresh-v17.sqlite', 'owned-v15.sqlite', 'owned-v16.sqlite', 'owned-v17.sqlite'} or set(expected['databases']) != {p.name for p in directory.glob('*.sqlite')}:
        raise ValueError('fixture inventory mismatch')
    return expected
