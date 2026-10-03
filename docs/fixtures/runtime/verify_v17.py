#!/usr/bin/env python3
"""Runtime/automation delta parity: actual pinned code, isolated fixtures only.

Source integration assertions plus independent final-state/schema/lease/count
expectations. Executable Codex/sleep stand-ins are synthetic; no real model runs.
"""
import sys
sys.dont_write_bytecode = True
import argparse
from contextlib import contextmanager, ExitStack
import copy
from functools import wraps
import hashlib
import importlib.util
import inspect
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import sqlite3
import subprocess
import tempfile
import threading
from types import SimpleNamespace
from unittest.mock import patch
from urllib.parse import unquote, urlparse

HERE = Path(__file__).resolve().parent
CORPUS = HERE.parent / 'runtime-automation-v17.json'
spec = importlib.util.spec_from_file_location('mcp_delta_contract', HERE.parent / 'mcp/verify_v17.py')
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)
FILES = {'test_automation.py', 'test_runtime.py', 'test_worktree_runtime.py', 'test_codex_client.py'}
OPERATIONS = {'source_scenario', 'plan', 'answer', 'launch_lease', 'blocked_run', 'runtime_bounds'}


def validate(corpus):
    if corpus.get('corpus_version') != 1 or corpus.get('schema_version') != 17 or corpus.get('source', {}).get('commit') != shared.PIN:
        raise ValueError('corpus version/source mismatch')
    cases = corpus['cases']
    if len(cases) != 77:
        raise ValueError('corpus must retain all 77 cases')
    ids = [c['id'] for c in cases]
    if ids != sorted(ids) or len(set(ids)) != len(ids) or {c['operation'] for c in cases} != OPERATIONS:
        raise ValueError('sorted unique ids / operation coverage mismatch')
    nodes = []
    for case in cases:
        if set(case) - {'id', 'operation', 'source_test', 'changes', 'kind', 'input', 'expect'}:
            raise ValueError('unknown case field')
        if case['operation'] == 'source_scenario':
            file, node = case['source_test'].split('::')
            if file not in FILES or not re.fullmatch(r'test_[a-z0-9_]+(?:\[[a-z]+\])?', node):
                raise ValueError('unknown source scenario')
            nodes.append(case['source_test'])
        if case['operation'] == 'answer' and case['kind'] not in {'prepare', 'review'}:
            raise ValueError('invalid answer kind')
    if len(nodes) != len(set(nodes)) or {n.split('::')[0] for n in nodes} != FILES:
        raise ValueError('source scenarios duplicate / file coverage missing')
    return cases


def assert_fields(actual, expected, label):
    for name, wanted in expected.items():
        if name not in actual or not shared.equal(actual[name], wanted):
            raise AssertionError(f'{label}: field {name} mismatch ({actual.get(name)!r} != {wanted!r})')


def expand(value):
    if isinstance(value, dict):
        if set(value) == {'repeat_text', 'count'}:
            if not isinstance(value['repeat_text'], str) or type(value['count']) is not int or not 0 <= value['count'] <= 100000:
                raise ValueError('invalid bounded repeated string')
            return value['repeat_text'] * value['count']
        return {k: expand(v) for k, v in value.items()}
    if isinstance(value, list):
        return [expand(v) for v in value]
    return value


def config(root, git=False):
    ws = root / 'workspace'
    ws.mkdir(parents=True)
    (ws / 'module.py').write_text('def add(a,b):\n    return a-b\n')
    if git:
        for args in [('init', '-q'), ('add', 'module.py'), ('-c', 'user.name=fixture', '-c', 'user.email=fixture@example.com', 'commit', '-q', '-m', 'fixture')]:
            subprocess.run(['git', '-C', str(ws), *args], check=True, capture_output=True)
    from tests.conftest import make_config
    return make_config(root, ws)


def pure_cases(corpus, cases, root, automation, codex, runtime, wt):
    passed = 0
    for case in cases:
        if case['operation'] == 'source_scenario':
            continue
        operation, expected = case['operation'], case['expect']
        directory = root / case['id']
        directory.mkdir()
        if operation == 'answer':
            value = expand(case['input'])
            try:
                answer = codex.validate_answer(case['kind'], value)
            except codex.CodexError as exc:
                assert expected.get('error_contains') in str(exc), case['id']
            else:
                assert 'exact' in expected and shared.equal(answer, expected['exact']), case['id']
        elif operation == 'plan':
            cfg = config(directory)
            before = {str(p): p.read_bytes() for p in directory.rglob('*') if p.is_file()}
            plan = copy.deepcopy(corpus['base_plan'])
            for path, value in case['changes'].items():
                parts = path.split('.')
                target = plan
                for part in parts[:-1]:
                    target = target[int(part)] if isinstance(target, list) else target[part]
                target[parts[-1]] = expand(value)
            try:
                result = automation.create_run(cfg, plan) if 'error_contains' in expected else automation.validate_plan(cfg, plan)
            except automation.AutomationError as exc:
                assert expected.get('error_contains') in str(exc), case['id'] + ': unexpected plan error'
            else:
                assert 'error_contains' not in expected, case['id'] + ': invalid plan accepted'
                result['step_order'] = [s['id'] for s in result['steps']]
                assert_fields(result, expected['values'], case['id'])
            assert not cfg.db_path.exists(), 'plan validation created SQLite state'
            assert before == {str(p): p.read_bytes() for p in directory.rglob('*') if p.is_file()}, 'plan validation changed files'
        elif operation == 'runtime_bounds':
            assert_fields({'ready_timeout': wt.READY_TIMEOUT, 'startup_lock_timeout': wt.WORKTREE_START_LOCK_TIMEOUT, 'manager_default_wait': inspect.signature(runtime._manager_lock).parameters['wait'].default}, expected, case['id'])
        elif operation == 'launch_lease':
            cfg = config(directory, git=True)
            run = automation.create_run(cfg, corpus['base_plan'])
            calls = []
            def fake_spawn(*args, **kwargs):
                calls.append((args, kwargs))
                return SimpleNamespace(pid=999991)
            with patch.object(runtime, '_spawn_process', fake_spawn), patch.object(runtime, '_identity', lambda pid: 'synthetic-start'):
                result = automation.launch(cfg, run['run_id'])
                try:
                    automation.launch(cfg, run['run_id'], resume=True)
                except automation.AutomationError as exc:
                    repeat = str(exc)
                else:
                    raise AssertionError('duplicate detached launch admitted')
            record_path = cfg.state_dir / 'automation' / run['run_id'] / 'process.json'
            record = json.loads(record_path.read_text())
            binding = record['project_id'] == cfg.project_id and record['workspace'] == str(cfg.workspace) and record['run_id'] == run['run_id'] and record['kind'] == 'automation' and record['pid'] == result['pid']
            assert expected['repeat_error_contains'] in repeat, case['id']
            assert_fields({'status': result['status'], 'spawn_attempts': len(calls), 'record_binding_matches': binding}, {k: v for k, v in expected.items() if k != 'repeat_error_contains'}, case['id'])
        elif operation == 'blocked_run':
            cfg = config(directory, git=True)
            run = automation.create_run(cfg, corpus['base_plan'])
            class UnavailableCodex:
                attempts = 0
                def call(self, *args):
                    self.attempts += 1
                    raise codex.CodexError('synthetic unavailable model')
            client = UnavailableCodex()
            coordinator = automation.Coordinator(cfg, run['run_id'], client)
            # Only retry delay is replaced; run persistence/limits/locking are real.
            with patch.object(automation.time, 'sleep', lambda delay: None):
                coordinator.run()
            saved = automation.RunStore(cfg).load(run['run_id'])
            from agent_bridge.storage import Storage
            with Storage(cfg).connect() as conn:
                count = conn.execute('SELECT COUNT(*) FROM tasks').fetchone()[0]
            assert_fields({'status': saved['status'], 'blocker_code': saved['blocker']['code'], 'task_count': count, 'prepare_attempts': client.attempts}, expected, case['id'])
        passed += 1
    return passed


def source_cases(pytest, automation, codex, runtime, wt, cases, root, children):
    chosen = {c['source_test']: c for c in cases if c['operation'] == 'source_scenario'}
    class Observer:
        checked = set()
        skipped = []
        guard = threading.Lock()
        def key(self, item):
            return Path(item.path).name + '::' + item.name
        def pytest_collection_modifyitems(self, items):
            if {self.key(i) for i in items} != set(chosen) or len(items) != len(chosen):
                raise ValueError('source collection differs from corpus')
        def pytest_runtest_logreport(self, report):
            if report.skipped:
                self.skipped.append(report.nodeid)
        @pytest.hookimpl(wrapper=True)
        def pytest_runtest_call(self, item):
            key = self.key(item)
            results, errors, locks = [], [], []
            responses, adapter_errors = [], []
            child_start = len(children)
            original_start, original_lock, original_call = wt.start_worktree_server, runtime._manager_lock, codex.CodexClient.call
            @wraps(original_start)
            def start(*args, **kwargs):
                try:
                    value = original_start(*args, **kwargs)
                except BaseException as exc:
                    with self.guard: errors.append(type(exc).__name__)
                    raise
                with self.guard: results.append(value)
                return value
            @contextmanager
            def lock(*args, **kwargs):
                try:
                    with original_lock(*args, **kwargs):
                        yield
                except BaseException:
                    with self.guard: locks.append(False)
                    raise
                else:
                    with self.guard: locks.append(True)
            @wraps(original_call)
            def call(*args, **kwargs):
                try:
                    value = original_call(*args, **kwargs)
                except BaseException as exc:
                    adapter_errors.append(type(exc).__name__)
                    raise
                responses.append(value)
                return value
            with patch.object(wt, 'start_worktree_server', start), patch.object(runtime, '_manager_lock', lock), patch.object(codex.CodexClient, 'call', call):
                yield
                actual = {'lock_successes': locks.count(True), 'lock_errors': locks.count(False), 'start_successes': len(results), 'start_errors': len(errors), 'distinct_ports': len({p for p, _ in results}), 'standin_children': sum(kind == 'sleep' for kind, _ in children[child_start:]), 'fake_codex_children': sum(kind == 'fake_codex' for kind, _ in children[child_start:]), 'adapter_response': responses[-1] if responses else None, 'adapter_error': adapter_errors[-1] if adapter_errors else None, 'engine_calls': len(item.funcargs.get('engine', []))}
                cfg = item.funcargs.get('config')
                if Path(item.path).name == 'test_automation.py':
                    saved = automation.RunStore(cfg).load()
                    from agent_bridge.storage import Storage
                    with Storage(cfg).connect() as conn:
                        actual.update(task_statuses=sorted(r[0] for r in conn.execute('SELECT status FROM tasks')), rounds=conn.execute('SELECT COUNT(*) FROM rounds').fetchone()[0], delivery_states=sorted(r[0] for r in conn.execute('SELECT delivery_state FROM worktrees')))
                    actual.update(status=saved['status'], phase=saved['phase'])
                    # Compare state inventories as multisets, not generated task UUID order.
                    if 'delivery_states' in chosen[key]['expect']:
                        actual['delivery_states'] = sorted(actual['delivery_states'])
                expected = copy.deepcopy(chosen[key]['expect'])
                if 'delivery_states' in expected:
                    expected['delivery_states'].sort()
                assert_fields(actual, expected, chosen[key]['id'])
                self.checked.add(key)
    observer = Observer()
    nodes = [str(shared.REFERENCE / 'tests' / name) for name in chosen]
    result = pytest.main(['-q', '--tb=short', '-c', '/dev/null', '-o', 'addopts=', '-p', 'no:cacheprovider', '--rootdir', str(shared.REFERENCE), '--basetemp', str(root / 'pytest'), *nodes], plugins=[observer])
    if result != 0 or observer.skipped or observer.checked != set(chosen):
        raise ValueError(f'source/corpus checks failed: pytest={int(result)}, checked={len(observer.checked)}/{len(chosen)}, skips={len(observer.skipped)}')
    return len(observer.checked)


@contextmanager
def confinement(root, children):
    connect, popen = sqlite3.connect, subprocess.Popen
    def within(path):
        return Path(path).resolve().is_relative_to(root)
    def confined_connect(database, *args, **kwargs):
        name = os.fspath(database)
        path = unquote(urlparse(name).path) if name.startswith('file:') else name
        if not within(path):
            raise AssertionError('SQLite outside temporary fixture directory')
        return connect(database, *args, **kwargs)
    def confined_popen(command, *args, **kwargs):
        if isinstance(command, (str, bytes)) or kwargs.get('shell'):
            raise AssertionError('shell/unexpected process launch forbidden')
        argv = [os.fspath(arg) for arg in command]
        cwd, kind = kwargs.get('cwd'), None
        if Path(argv[0]).name == 'git':
            location = argv[argv.index('-C') + 1] if '-C' in argv else cwd
            if location is None and 'init' in argv:
                location = argv[-1]
            assert location is not None and within(location), 'Git outside synthetic fixture directory'
        elif argv in (['python3', '-B', 'module.py'], ['python3', '-B', 'consumer.py'], ['false']):
            assert cwd is not None and within(cwd), 'synthetic check outside temporary workspace'
            kind = 'check'
        elif argv == [sys.executable, '-c', 'import time; time.sleep(60)']:
            assert cwd is not None and within(cwd), 'sleep stand-in outside fixture workspace'
            kind = 'sleep'
        elif argv[0] == 'codex':
            target = Path(shutil.which('codex') or '/nonexistent').resolve()
            assert within(target) and target.name == 'codex' and target.read_text().startswith('#!'+sys.executable+'\n'), 'real Codex execution forbidden'
            assert argv[1] == 'exec' and '--ignore-user-config' in argv and argv[argv.index('--sandbox') + 1] == 'read-only' and argv[argv.index('--disable') + 1] == 'multi_agent', 'adapter read-only flags changed'
            for flag in ('-C', '--output-schema', '-o'):
                assert within(argv[argv.index(flag) + 1]), 'adapter workspace/output outside fixtures'
            kind = 'fake_codex'
        else:
            raise AssertionError('unexpected process: only fixture Git/checks/sleep/fake Codex allowed')
        proc = popen(command, *args, **kwargs)
        if kind:
            children.append((kind, proc))
        return proc
    with ExitStack() as stack:
        stack.enter_context(patch.object(sqlite3, 'connect', confined_connect))
        stack.enter_context(patch.object(subprocess, 'Popen', confined_popen))
        for name in ('connect', 'connect_ex'):
            stack.enter_context(patch.object(socket.socket, name, side_effect=AssertionError('network forbidden')))
        stack.enter_context(patch.object(socket, 'create_connection', side_effect=AssertionError('network forbidden')))
        stack.enter_context(patch.dict(os.environ, {'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': '/dev/null'}))
        try:
            yield
        finally:
            for kind, child in children:
                if child.poll() is None:
                    # Every permitted stand-in/check is our own tracked child.
                    if os.getpgid(child.pid) == child.pid:
                        os.killpg(child.pid, signal.SIGKILL)
                    else:
                        child.kill()
                    child.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--corpus', type=Path, default=CORPUS)
    args = parser.parse_args()
    try:
        corpus = json.loads(args.corpus.read_text())
        cases = validate(corpus)
        pytest, _, _ = shared.bootstrap()
        from agent_bridge import automation, codex_client, runtime, worktree_runtime
        files = sorted(HERE.parent.rglob('*.sqlite')) + sorted(HERE.parent.glob('*.json'))
        before = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
        try:
            with tempfile.TemporaryDirectory(prefix='aibridge-runtime-v17-') as temporary:
                root = Path(temporary).resolve()
                children = []
                with confinement(root, children):
                    passed = pure_cases(corpus, cases, root, automation, codex_client, runtime, worktree_runtime)
                    passed += source_cases(pytest, automation, codex_client, runtime, worktree_runtime, cases, root, children)
        finally:
            shared.source_guard()
            if before != {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in files}:
                raise ValueError('committed fixture bytes changed')
    except (ValueError, KeyError, TypeError, OSError, AssertionError) as exc:
        print('FAIL ' + str(exc), file=sys.stderr)
        return 1
    print(f'ok: {passed}/{len(cases)} v17 runtime/automation cases; no skips; synthetic stand-ins only, no real models/network/user runtime state')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
