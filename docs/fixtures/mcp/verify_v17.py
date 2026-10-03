#!/usr/bin/env python3
"""0B.4 independent response/spawn expectations over pinned source scenarios.

Source assertions additionally prove Git/SQLite/history/lock effects. No skips,
server, real model, detached worker or user runtime database is allowed.
"""
from __future__ import annotations
import sys
sys.dont_write_bytecode = True
import argparse
from contextlib import ExitStack
from functools import wraps
import hashlib
import json
import os
import re
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import threading
from unittest.mock import patch
from urllib.parse import unquote, urlparse

PIN = 'e52a46158cbeb4f3ae35063d395c05ea0ce144bc'
HERE = Path(__file__).resolve().parent
CORPUS = HERE.parent / 'mcp-delivery-recovery-v17.json'
REFERENCE = Path(os.environ.get('AGENT_BRIDGE_REFERENCE', '/home/denis/Python/agent_bridge')).resolve()
FILES = {'test_delivery_on_accept.py', 'test_mcp.py', 'test_storage.py', 'test_automation.py'}
TOOLS = {'accept_task': 'accept_task_impl', 'task_status': 'task_status_impl', 'request_changes': 'request_changes_impl', 'summary': '_task_result', 'submit_task': 'submit_task_impl'}


def source_guard():
    head = subprocess.check_output(['git', '-C', str(REFERENCE), 'rev-parse', 'HEAD'], text=True).strip()
    clean = subprocess.check_output(['git', '-C', str(REFERENCE), 'status', '--porcelain'], text=True).strip()
    if head != PIN or clean:
        raise ValueError('reference HEAD/clean-tree differs from pinned contract')


def bootstrap():
    python = REFERENCE / '.venv/bin/python'
    if python.is_file() and os.environ.get('_AB_V17_MCP_VERIFY') != '1':
        env = dict(os.environ, PYTHONDONTWRITEBYTECODE='1', PYTEST_DISABLE_PLUGIN_AUTOLOAD='1', PYTEST_ADDOPTS='', _AB_V17_MCP_VERIFY='1')
        os.execve(str(python), [str(python), *sys.argv], env)
    os.environ['PYTEST_DISABLE_PLUGIN_AUTOLOAD'] = '1'
    os.environ['PYTEST_ADDOPTS'] = ''
    source_guard()
    sys.path[:0] = [str(REFERENCE / 'src'), str(REFERENCE)]
    import pytest
    from agent_bridge import mcp_server, storage
    if Path(storage.__file__).resolve() != REFERENCE / 'src/agent_bridge/storage.py' or storage.SCHEMA_VERSION != 17:
        raise ValueError('unexpected source module/schema')
    return pytest, mcp_server, storage


def validate(corpus):
    if corpus.get('corpus_version') != 1 or corpus.get('schema_version') != 17 or corpus.get('source', {}).get('commit') != PIN:
        raise ValueError('corpus version/source mismatch')
    cases = corpus.get('cases')
    if not isinstance(cases, list) or len(cases) != 47:
        raise ValueError('corpus must retain all 47 delta scenarios')
    ids = [c['id'] for c in cases]
    nodes = [c['source_test'] for c in cases]
    if ids != sorted(ids) or len(set(ids)) != len(ids) or len(set(nodes)) != len(nodes):
        raise ValueError('case ids must be sorted and unique; scenarios unique')
    for case in cases:
        if set(case) - {'id', 'source_test', 'expect', 'setup'} or not {'id', 'source_test', 'expect'} <= set(case):
            raise ValueError('unknown/missing case field')
        file, node = case['source_test'].split('::')
        if file not in FILES or not re.fullmatch(r'test_[a-z0-9_]+(?:\[[A-Za-z0-9_]+\])?', node):
            raise ValueError('unrecognized source scenario')
        if case.get('setup', {}) not in ({}, {'server_probe': 'healthy'}):
            raise ValueError('unknown scenario setup')
        expect = case['expect']
        if set(expect) - {'responses', 'spawn_attempts', 'redact_state_paths'} or type(expect['spawn_attempts']) is not int or expect['spawn_attempts'] < 0:
            raise ValueError('invalid expectation fields/spawn count')
        if not expect['responses'] or set(expect['responses']) - (set(TOOLS) | {'claim', 'release'}):
            raise ValueError('unknown/missing response tool')
        for patterns in expect['responses'].values():
            if not isinstance(patterns, list) or not patterns:
                raise ValueError('response patterns must be nonempty lists')
            for pattern in patterns:
                if not pattern or set(pattern) - {'values', 'exact', 'absent', 'exception'}:
                    raise ValueError('invalid response pattern')
                if 'exception' in pattern and set(pattern) != {'exception'}:
                    raise ValueError('exception pattern cannot include response')
    if {c['source_test'].split('::')[0] for c in cases} != FILES:
        raise ValueError('scenario coverage missing')
    return cases


def field(value, key):
    for part in key.split('.'):
        if not isinstance(value, dict) or part not in value:
            return False, None
        value = value[part]
    return True, value


def equal(left, right):
    return json.dumps(left, sort_keys=True) == json.dumps(right, sort_keys=True)


def matches(record, pattern):
    if 'exception' in pattern:
        return record == pattern
    if 'exception' in record:
        return False
    value = record['response']
    if 'exact' in pattern and not equal(value, pattern['exact']):
        return False
    for key, wanted in pattern.get('values', {}).items():
        exists, actual = field(value, key)
        if not exists or not equal(actual, wanted):
            return False
    return all(not field(value, key)[0] for key in pattern.get('absent', []))


def observations_match(actual, patterns):
    # Match a multiset: concurrent claim return order is deliberately irrelevant.
    # Exact patterns first avoid greedy broad predicates consuming narrow ones.
    if len(actual) != len(patterns):
        return False
    remaining = list(actual)
    for pattern in sorted(patterns, key=lambda p: -len(json.dumps(p))):
        index = next((i for i, r in enumerate(remaining) if matches(r, pattern)), None)
        if index is None:
            return False
        remaining.pop(index)
    return True


def run(pytest, mcp, storage, cases, root):
    class Observer:
        def __init__(self):
            self.cases = {c['source_test']: c for c in cases}
            self.collected = set()
            self.checked = set()
            self.skipped = []
            self.lock = threading.RLock()
            self.records = {}
            self.spawns = []

        def key(self, item):
            return Path(item.path).name + '::' + item.name

        def pytest_collection_modifyitems(self, items):
            self.collected = {self.key(item) for item in items}
            if self.collected != set(self.cases) or len(items) != len(self.cases):
                raise ValueError('collected scenarios differ from corpus')

        def pytest_runtest_logreport(self, report):
            if report.skipped:
                self.skipped.append(report.nodeid)

        def ensure_spawn_spy(self):
            with self.lock:
                current = mcp.spawn_worker
                if getattr(current, '_delta_spawn_spy', False):
                    return
                @wraps(current)
                def spy(*args, **kwargs):
                    with self.lock:
                        self.spawns.append((args[1], args[2]))
                    return current(*args, **kwargs)
                spy._delta_spawn_spy = True
                mcp.spawn_worker = spy

        def wrapper(self, label, original):
            @wraps(original)
            def wrapped(*args, **kwargs):
                self.ensure_spawn_spy()
                try:
                    result = original(*args, **kwargs)
                except BaseException as exc:
                    with self.lock:
                        self.records.setdefault(label, []).append({'exception': type(exc).__name__})
                    raise
                if label == 'claim':
                    value = {'present': result is not None}
                    if result is not None:
                        value.update(status=result.status, round_number=result.round_number)
                else:
                    value = result
                with self.lock:
                    self.records.setdefault(label, []).append({'response': json.loads(json.dumps(value))})
                return result
            return wrapped

        @pytest.hookimpl(wrapper=True)
        def pytest_runtest_call(self, item):
            key = self.key(item)
            case = self.cases[key]
            self.records, self.spawns = {}, []
            original_spawn = mcp.spawn_worker
            with ExitStack() as stack:
                for label, function in TOOLS.items():
                    stack.enter_context(patch.object(mcp, function, self.wrapper(label, getattr(mcp, function))))
                for label, function in [('claim', 'claim_needs_user_recovery'), ('release', 'release_needs_user_recovery')]:
                    stack.enter_context(patch.object(storage.Storage, function, self.wrapper(label, getattr(storage.Storage, function))))
                if case.get('setup', {}).get('server_probe') == 'healthy':
                    stack.enter_context(patch.object(mcp, '_check_server', lambda cfg: None))
                try:
                    yield
                finally:
                    mcp.spawn_worker = original_spawn
                expect = case['expect']
                for tool, patterns in expect['responses'].items():
                    actual = self.records.get(tool, [])
                    assert observations_match(actual, patterns), f"{case['id']}: {tool} response projection/count mismatch"
                assert len(self.spawns) == expect['spawn_attempts'], f"{case['id']}: spawn attempts {len(self.spawns)} != {expect['spawn_attempts']}"
                if expect.get('redact_state_paths'):
                    config = item.funcargs['config']
                    payload = json.dumps(self.records)
                    assert str(config.state_dir) not in payload, 'private state directory leaked'
                    events = storage.Storage(config).list_events('task-oa')
                    assert str(config.state_dir) not in json.dumps(events), 'private path leaked to persisted events'
                self.checked.add(key)

    observer = Observer()
    connect, popen = sqlite3.connect, subprocess.Popen
    def within(path):
        return Path(path).resolve().is_relative_to(root)
    def confined_connect(database, *args, **kwargs):
        name = os.fspath(database)
        path = unquote(urlparse(name).path) if name.startswith('file:') else name
        if not within(path):
            raise AssertionError('SQLite connection outside synthetic temporary fixtures')
        return connect(database, *args, **kwargs)
    def confined_popen(command, *args, **kwargs):
        if isinstance(command, (str, bytes)) or kwargs.get('shell'):
            raise AssertionError('unexpected shell/process launch')
        argv = [os.fspath(arg) for arg in command]
        cwd = kwargs.get('cwd')
        if Path(argv[0]).name == 'git':
            location = argv[argv.index('-C') + 1] if '-C' in argv else cwd
            if location is None and 'init' in argv:
                location = argv[-1]
            if location is None or not within(location):
                raise AssertionError('Git operation outside synthetic temporary fixtures')
        elif argv in (['python3', '-B', 'module.py'], ['python3', '-B', 'consumer.py']):
            if cwd is None or not within(cwd):
                raise AssertionError('synthetic test outside temporary workspace')
        else:
            raise AssertionError('unexpected process: only local Git and synthetic -B Python checks allowed')
        return popen(command, *args, **kwargs)
    with ExitStack() as stack:
        stack.enter_context(patch.object(sqlite3, 'connect', confined_connect))
        stack.enter_context(patch.object(subprocess, 'Popen', confined_popen))
        for name in ('connect', 'connect_ex'):
            stack.enter_context(patch.object(socket.socket, name, side_effect=AssertionError('network forbidden')))
        stack.enter_context(patch.object(socket, 'create_connection', side_effect=AssertionError('network forbidden')))
        stack.enter_context(patch.dict(os.environ, {'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': '/dev/null'}))
        nodes = [str(REFERENCE / 'tests' / case['source_test']) for case in cases]
        result = pytest.main(['-q', '--tb=short', '-c', '/dev/null', '-o', 'addopts=', '-p', 'no:cacheprovider', '--rootdir', str(REFERENCE), '--basetemp', str(root / 'pytest'), *nodes], plugins=[observer])
    if result != 0 or observer.skipped or observer.checked != set(observer.cases):
        raise ValueError(f'source/corpus checks failed: pytest={int(result)}, checked={len(observer.checked)}/{len(cases)}, skips={len(observer.skipped)}')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--corpus', type=Path, default=CORPUS)
    args = parser.parse_args()
    try:
        cases = validate(json.loads(args.corpus.read_text()))
        pytest, mcp, storage = bootstrap()
        files = sorted(HERE.parent.rglob('*.sqlite')) + sorted(HERE.parent.glob('*cases.json')) + [CORPUS]
        before = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
        try:
            with tempfile.TemporaryDirectory(prefix='aibridge-mcp-v17-') as temporary:
                run(pytest, mcp, storage, cases, Path(temporary).resolve())
        finally:
            source_guard()
            if before != {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in files}:
                raise ValueError('committed fixture bytes changed')
    except (ValueError, KeyError, TypeError, OSError, AssertionError) as exc:
        print('FAIL ' + str(exc), file=sys.stderr)
        return 1
    print(f'ok: {len(cases)}/{len(cases)} v17 MCP/claim scenarios; independent response and spawn counts + pinned source side-effect assertions; no skips, network/models/runtime state')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
