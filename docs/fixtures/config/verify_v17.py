#!/usr/bin/env python3
"""Pinned v17 config/permission source parity for 0B.2, without runtime state.

Imports the actual reference functions with bytecode disabled. Every config,
file, symlink and Git repo is synthetic and confined to a temporary directory.
SQLite connections and network sockets are forbidden while observing cases.
"""
from __future__ import annotations

import argparse
from dataclasses import FrozenInstanceError, replace
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import subprocess
import sys
import tempfile
from unittest.mock import patch

PIN = 'e52a46158cbeb4f3ae35063d395c05ea0ce144bc'
REFERENCE = Path(os.environ.get('AGENT_BRIDGE_REFERENCE', '/home/denis/Python/agent_bridge')).resolve()
CORPUS = Path(__file__).resolve().parents[1] / 'config-permission-v17.json'
ERRORS = {
    'delivery_type': r'delivery_mode must be a string',
    'delivery_value': r'delivery_mode must be one of',
    'delivery_whitespace': r'delivery_mode must not have surrounding whitespace',
    'delivery_requires_worktree': r'delivery_mode="on_accept" requires',
    'state_type': r'auto_approve_state_directory must be a boolean',
    'state_root': r"cannot scope the filesystem root '/'",
    'state_wildcard': r'without OpenCode wildcard characters',
}
OPERATIONS = {'config', 'immutable', 'no_state_creation', 'controller', 'worker', 'boundaries'}
SETUPS = {'escape_link', 'safe_link', 'broken_link', 'loop_link'}


def source_guard():
    head = subprocess.check_output(['git', '-C', str(REFERENCE), 'rev-parse', 'HEAD'], text=True).strip()
    if head != PIN or subprocess.check_output(['git', '-C', str(REFERENCE), 'status', '--porcelain'], text=True).strip():
        raise ValueError('reference HEAD/clean-tree differs from pinned contract')


def bootstrap():
    sys.dont_write_bytecode = True
    python = REFERENCE / '.venv/bin/python'
    if python.is_file() and os.environ.get('_AB_V17_CONFIG_VERIFY') != '1':
        env = dict(os.environ, PYTHONDONTWRITEBYTECODE='1', _AB_V17_CONFIG_VERIFY='1')
        os.execve(str(python), [str(python), *sys.argv], env)
    source_guard()
    sys.path.insert(0, str(REFERENCE / 'src'))
    import agent_bridge
    if Path(agent_bridge.__file__).resolve().parent != (REFERENCE / 'src/agent_bridge').resolve():
        raise ValueError('reference package imported from another checkout')
    from agent_bridge import config, worker, opencode_launcher, git_snapshot, storage
    if storage.SCHEMA_VERSION != 17:
        raise ValueError('reference schema differs from v17')
    return config, worker, opencode_launcher, git_snapshot


def validate(corpus):
    if corpus.get('corpus_version') != 1 or corpus.get('schema_version') != 17 or corpus.get('source', {}).get('commit') != PIN:
        raise ValueError('corpus version/source pin mismatch')
    cases = corpus.get('cases')
    if not isinstance(cases, list) or not cases:
        raise ValueError('corpus must contain cases')
    ids = [c['id'] for c in cases]
    if ids != sorted(ids) or len(ids) != len(set(ids)):
        raise ValueError('case ids must be sorted and unique')
    placeholders = set(corpus['placeholder_conventions'])
    for case in cases:
        if set(case) - {'id', 'operation', 'expect', 'toml_extra', 'state_root', 'override_state_root', 'override_approval', 'permission', 'setup'}:
            raise ValueError('unknown case field')
        if case.get('operation') not in OPERATIONS or not isinstance(case.get('expect'), dict):
            raise ValueError('unknown operation or missing expectation')
        if set(case.get('setup', [])) - SETUPS:
            raise ValueError('unknown filesystem setup')
        if 'error' in case['expect'] and case['expect']['error'] not in ERRORS:
            raise ValueError('unknown error category')
        payload = json.dumps(case)
        if set(re.findall(r'\$\{[A-Z_]+\}', payload)) - placeholders:
            raise ValueError('undeclared placeholder')
        if '/home/' in payload or '/Users/' in payload:
            raise ValueError('machine-specific fixture path')
    if {c['operation'] for c in cases} != OPERATIONS:
        raise ValueError('operation coverage missing')
    return cases


def substitute(value, mapping):
    if isinstance(value, str):
        for token, path in mapping.items():
            value = value.replace(token, str(path))
        if re.search(r'\$\{[A-Z_]+\}', value):
            raise ValueError('unresolved placeholder')
        return value
    if isinstance(value, list):
        return [substitute(v, mapping) for v in value]
    if isinstance(value, dict):
        return {substitute(k, mapping): substitute(v, mapping) for k, v in value.items()}
    return value


def fixture(root, case):
    mapping = {name: root / directory for name, directory in [('${WORKSPACE}', 'workspace'), ('${STATE}', 'state'), ('${CUSTOM}', 'custom-state'), ('${EXTERNAL}', 'external'), ('${OUTSIDE}', 'outside')]}
    mapping['${TMP}'] = root
    for directory in mapping.values():
        directory.mkdir(exist_ok=True)
    for directory in [mapping['${STATE}'] / 'proj', mapping['${STATE}'] / 'repo', mapping['${CUSTOM}'] / 'proj', mapping['${EXTERNAL}'] / 'sub', root / 'state-sibling']:
        directory.mkdir(exist_ok=True)
    (mapping['${STATE}'] / 'proj/fixture.txt').write_text('synthetic fixture\n')
    (mapping['${EXTERNAL}'] / 'file.py').write_text('synthetic external fixture\n')
    for setup in case.get('setup', []):
        link, target = {'escape_link': ('escape', mapping['${OUTSIDE}']), 'safe_link': ('alias', mapping['${STATE}'] / 'proj'), 'broken_link': ('broken', mapping['${STATE}'] / 'absent'), 'loop_link': ('loop', mapping['${STATE}'] / 'loop')}[setup]
        (mapping['${STATE}'] / link).symlink_to(target, target_is_directory=True)
    template = '[projects.proj]\nworkspace="${WORKSPACE}"\nopencode_url="http://127.0.0.1:4101"\npassword_file="fixture.password"\nmax_rounds=3\n'
    template += case.get('toml_extra', '') + '\n'
    if case['operation'] == 'boundaries':
        template += '\n[projects.state]\nworkspace="${STATE}/repo"\nopencode_url="http://127.0.0.1:4102"\npassword_file="state.password"\nmax_rounds=3\n'
        template += '\n[projects.external]\nworkspace="${EXTERNAL}"\nopencode_url="http://127.0.0.1:4103"\npassword_file="external.password"\nmax_rounds=3\n'
        for repo in [mapping['${STATE}'] / 'repo', mapping['${EXTERNAL}']]:
            subprocess.run(['git', 'init', '-q', str(repo)], check=True, capture_output=True)
    path = root / 'projects.toml'
    path.write_text(substitute(template, mapping))
    return path, mapping


def observe(case, root, modules):
    config, worker, launcher, git_snapshot = modules
    path, mapping = fixture(root, case)
    state_root = substitute(case.get('state_root', '${STATE}'), mapping)
    cfg = config.load_project(path, 'proj', state_root=state_root)
    if 'override_state_root' in case:
        cfg = replace(cfg, state_root=Path(substitute(case['override_state_root'], mapping)), auto_approve_state_directory=case['override_approval'])
    original_external = cfg.auto_approve_external_directories
    op = case['operation']
    if op in {'config', 'no_state_creation'}:
        result = {'execution_mode': cfg.execution_mode, 'delivery_mode': cfg.delivery_mode, 'auto_approve_state_directory': cfg.auto_approve_state_directory, 'external_roots': [str(p) for p in cfg.auto_approve_external_directories]}
        if op == 'no_state_creation' and Path(state_root).exists():
            raise AssertionError('config loading created state directory')
    elif op == 'immutable':
        result = {}
        for attribute, key in [('delivery_mode', 'delivery_mode_immutable'), ('auto_approve_state_directory', 'state_approval_immutable')]:
            try:
                setattr(cfg, attribute, 'on_accept' if attribute == 'delivery_mode' else True)
            except FrozenInstanceError:
                result[key] = True
            else:
                result[key] = False
    elif op == 'controller':
        generated = launcher.build_controller_config(cfg, [], '/synthetic/agent-bridge')
        result = {'permission': generated['agent'][launcher.CONTROLLER_AGENT_NAME]['permission'], 'subagent_depth': generated['subagent_depth'], 'external_roots': [str(p) for p in cfg.auto_approve_external_directories]}
    elif op == 'worker':
        approve, info = worker._permission_decision(cfg, substitute(case['permission'], mapping))
        result = {'approve': approve, 'reason': info['reason']}
    elif op == 'boundaries':
        approve, _ = worker._permission_decision(cfg, {'id': 'fixture', 'permission': 'external_directory', 'patterns': [str(mapping['${STATE}'] / 'repo')]})
        try:
            git_snapshot.validate_allowed_paths(cfg.workspace, [str(mapping['${STATE}'] / 'repo/file.py')], external_roots=cfg.auto_approve_external_directories)
        except ValueError as error:
            if 'outside trusted directories' not in str(error):
                raise
            denied = 'outside_trusted_directories'
        else:
            denied = 'unexpectedly_allowed'
        external = git_snapshot.validate_allowed_paths(cfg.workspace, [str(mapping['${EXTERNAL}'] / 'file.py')], external_roots=cfg.auto_approve_external_directories) if cfg.auto_approve_external_directories else None
        result = {'permission_state': approve, 'external_roots': [str(p) for p in cfg.auto_approve_external_directories], 'linked_projects': [p.project_id for p in config.load_linked_projects(cfg)], 'state_git_scope': denied, 'external_git_scope': external}
    else:
        raise ValueError('unsupported operation')
    if cfg.auto_approve_external_directories != original_external:
        raise AssertionError('permission operation mutated external Git roots')
    if list(root.rglob('*.sqlite')):
        raise AssertionError('unexpected SQLite state file')
    return result, substitute(case['expect'], mapping)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--corpus', type=Path, default=CORPUS)
    args = parser.parse_args()
    try:
        corpus = json.loads(args.corpus.read_text())
        cases = validate(corpus)
        modules = bootstrap()
    except Exception as error:
        print('error: ' + str(error), file=sys.stderr)
        return 2
    failures = []
    counts = {op: 0 for op in sorted(OPERATIONS)}
    # Pure source functions may inspect synthetic paths but must never open DB/network.
    with patch.object(sqlite3, 'connect', side_effect=AssertionError('SQLite forbidden')), patch.object(socket, 'socket', side_effect=AssertionError('network forbidden')), patch.object(socket, 'create_connection', side_effect=AssertionError('network forbidden')):
        for case in cases:
            try:
                with tempfile.TemporaryDirectory(prefix='ab-config-permission-v17-', dir='/tmp') as temp:
                    try:
                        actual, expected = observe(case, Path(temp), modules)
                    except modules[0].ConfigError as error:
                        expected = case['expect']
                        if 'error' not in expected or not re.search(ERRORS[expected['error']], str(error)):
                            raise AssertionError('unexpected config error category') from error
                        actual = {'error': expected['error']}
                    if json.dumps(actual, sort_keys=True) != json.dumps(expected, sort_keys=True):
                        raise AssertionError('source outcome differs from corpus expectation')
                counts[case['operation']] += 1
            except Exception as error:
                failures.append(case['id'] + ': ' + type(error).__name__ + ': ' + str(error))
    try:
        source_guard()
    except Exception as error:
        failures.append('final source guard: ' + str(error))
    for failure in failures:
        print('FAIL ' + failure, file=sys.stderr)
    print(f'config/permission v17: {len(cases)-len(failures)}/{len(cases)} cases passed (no skips); ' + ', '.join(f'{op}={n}' for op, n in counts.items()))
    return 1 if failures else 0


if __name__ == '__main__':
    raise SystemExit(main())
