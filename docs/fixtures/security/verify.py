#!/usr/bin/env python3
"""Pinned Python v15 security parity; temporary Git/files only, no runtime DB."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

REFERENCE = Path(os.environ.get('AGENT_BRIDGE_REFERENCE', '/home/denis/Python/agent_bridge'))
PIN = '86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a'
CORPUS = Path(__file__).resolve().parents[1] / 'security-cases.json'


def bootstrap():
    sys.dont_write_bytecode = True
    python = REFERENCE / '.venv/bin/python'
    if python.is_file() and os.environ.get('_AB_V15_VERIFY') != '1':
        env = dict(os.environ, PYTHONDONTWRITEBYTECODE='1', _AB_V15_VERIFY='1')
        os.execve(str(python), [str(python), *sys.argv], env)
    head = subprocess.run(['git', '-C', str(REFERENCE), 'rev-parse', 'HEAD'], check=True, capture_output=True, text=True).stdout.strip()
    if head != PIN:
        raise RuntimeError('reference HEAD differs from pinned contract')
    sys.path.insert(0, str(REFERENCE / 'src'))
    import agent_bridge
    if Path(agent_bridge.__file__).resolve().parent != (REFERENCE / 'src/agent_bridge').resolve():
        raise RuntimeError('reference package imported from a different checkout')


def observe(case, root):
    from agent_bridge import git_worktree, mcp_server, storage
    op, arg = case['operation'], case['input']
    if op == 'secret_gate':
        return mcp_server._suspected_secret_error(arg['text'], arg['allow'])
    if op == 'parse_scopes':
        return storage._parse_reserved_scopes(arg['raw'], 'fixture-task')
    if op == 'scope_overlap':
        (root / 'src').mkdir()
        (root / 'alias').symlink_to('src', target_is_directory=True)
        (root / 'loop').symlink_to('loop')
        (root / 'broken').symlink_to('missing')
        return storage._canonical_scopes_overlap(arg['left'], arg['right'], root)
    if op == 'repo_support':
        subprocess.run(['git', 'init', '-q', str(root)], check=True, capture_output=True)
        for path, content in arg['files'].items():
            (root / path).write_text(content)
        if arg['sparse']:
            subprocess.run(['git', '-C', str(root), 'config', 'core.sparseCheckout', 'true'], check=True)
        return git_worktree.check_repo_support(root)
    raise ValueError('unknown operation')


def main():
    bootstrap()
    from agent_bridge.storage import ScopeDataError, SCHEMA_VERSION
    corpus = json.loads(CORPUS.read_text())
    if corpus['source']['commit'] != PIN or corpus['schema_version'] != SCHEMA_VERSION:
        raise ValueError('corpus source/schema mismatch')
    ids = [case['id'] for case in corpus['cases']]
    if len(ids) != len(set(ids)) or not ids:
        raise ValueError('empty or duplicate cases')
    failures = []
    for case in corpus['cases']:
        try:
            with tempfile.TemporaryDirectory(prefix='ab-security-') as temp:
                try:
                    result = observe(case, Path(temp))
                except ScopeDataError:
                    result = 'ScopeDataError'
            if result != case['expect']:
                failures.append(case['id'])
        except Exception as exc:
            failures.append(case['id'] + ': ' + type(exc).__name__)
    for failure in failures:
        print('FAIL ' + failure, file=sys.stderr)
    print(f"security: {len(ids) - len(failures)}/{len(ids)} cases passed (no skips)")
    return bool(failures)


if __name__ == '__main__':
    raise SystemExit(main())
