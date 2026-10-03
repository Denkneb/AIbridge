#!/usr/bin/env python3
"""Stdlib immutable inspection, exact rows/schema/predicate and file hashes."""
import argparse
import sqlite3
import sys
sys.dont_write_bytecode = True
from pathlib import Path
from contract import HERE, INDEX_SQL, digest, load_expected, normalized_sql, snapshot


def verify(directory=HERE):
    expected = load_expected(directory)
    paths = sorted(directory.glob('*.sqlite'))
    before = {p.name: digest(p) for p in paths}
    try:
        for name, spec in expected['databases'].items():
            actual = snapshot(directory / name)
            if actual != spec:
                raise ValueError(f'{name}: schema/rows differ from expectations')
            version = actual['version']
            meta = {r['key']: r['value'] for r in actual['rows']['meta']}
            if version != int(name.rsplit('-v', 1)[1].split('.')[0]) or meta.get('schema_version') != str(version):
                raise ValueError('fixture version markers mismatch')
            if version >= 16:
                column = next(c for c in actual['schema']['tasks']['columns'] if c['name'] == 'delivery_mode')
                if column != {'name': 'delivery_mode', 'type': 'TEXT', 'notnull': 1, 'dflt_value': "'manual'", 'pk': 0}:
                    raise ValueError('delivery default/type/nullability changed')
            if version == 17:
                index = actual['indexes']['ux_automation_unfinished']
                if index['sql'] != normalized_sql(INDEX_SQL) or index['unique'] != 1 or index['partial'] != 1 or index['xinfo'][0]['cid'] != -2:
                    raise ValueError('automation expression/index predicate mismatch')
                if actual['rows']['automation_runs']:
                    raise ValueError('initial automation table must be empty')
            owner = {r['key']: r['value'] for r in actual['rows']['meta']}.get('runtime_owner')
            if owner != ('rust' if name.startswith('owned-') else None):
                raise ValueError('fixture ownership mismatch')
    finally:
        if before != {p.name: digest(p) for p in paths}:
            raise ValueError('fixture bytes changed during read-only verification')
    return expected


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--fixtures', type=Path, default=HERE)
    args = parser.parse_args()
    try:
        verify(args.fixtures.resolve())
    except (ValueError, KeyError, OSError, sqlite3.Error) as exc:
        print(f'FAIL {exc}', file=sys.stderr)
        return 1
    print('ok: 4 delta fixtures inspected immutable/read-only; exact schema, rows, delivery default and automation predicate; hashes unchanged')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
