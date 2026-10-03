#!/usr/bin/env python3
"""Check the pinned manifest against source AST and in-memory SQLite only.

Never imports/executes agent_bridge, reads runtime state, or modifies its checkout.
Historical fixtures keep their own source pins and are checked separately.
"""
import argparse
import ast
import json
from pathlib import Path
import re
import sqlite3
import subprocess
import sys


class ContractError(Exception):
    pass


def check(condition, message):
    if not condition:
        raise ContractError(message)


def function(tree, name):
    matches = [n for n in ast.walk(tree) if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef)) and n.name == name]
    check(len(matches) == 1, f"source function is absent or ambiguous: {name}")
    return matches[0]


def constant(tree, name):
    for n in tree.body:
        if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == name for t in n.targets):
            return ast.literal_eval(n.value)
    raise ContractError(f"source constant is absent: {name}")


def sql_constants(node):
    return [n.value for n in ast.walk(node) if isinstance(n, ast.Constant) and isinstance(n.value, str)]


def normalized_sql(sql):
    return re.sub(r"\s+", "", sql).rstrip(";").casefold()


def cli_surface(tree):
    """Read literal argparse declarations, including literal command-name loops."""
    commands, handles = {}, {}
    def literal(node, env):
        if isinstance(node, ast.Name) and node.id in env:
            return env[node.id]
        return ast.literal_eval(node)
    def visit(statements, env):
        for stmt in statements:
            if isinstance(stmt, ast.For):
                check(isinstance(stmt.target, ast.Name), "unsupported CLI loop target")
                for value in literal(stmt.iter, env):
                    visit(stmt.body, {**env, stmt.target.id: value})
                continue
            if isinstance(stmt, ast.Assign) and isinstance(stmt.value, ast.Call):
                call = stmt.value
                if isinstance(call.func, ast.Attribute) and call.func.attr == 'add_parser':
                    name = literal(call.args[0], env)
                    commands[name] = set()
                    for target in stmt.targets:
                        if isinstance(target, ast.Name):
                            handles[target.id] = name
                elif isinstance(call.func, ast.Attribute) and call.func.attr == 'add_mutually_exclusive_group':
                    parent = handles.get(getattr(call.func.value, 'id', None))
                    for target in stmt.targets:
                        if isinstance(target, ast.Name) and parent:
                            handles[target.id] = parent
            if not isinstance(stmt, ast.Expr) or not isinstance(stmt.value, ast.Call):
                continue
            call = stmt.value
            if isinstance(call.func, ast.Name) and call.func.id == '_add_common':
                commands[handles[call.args[0].id]].update(['--config', '--project', '--state-root'])
            elif isinstance(call.func, ast.Attribute) and call.func.attr == 'add_argument':
                name = handles.get(getattr(call.func.value, 'id', None))
                if name:
                    metavar = next((literal(k.value, env) for k in call.keywords if k.arg == 'metavar'), None)
                    for arg in call.args:
                        value = literal(arg, env)
                        commands[name].add(metavar if not value.startswith('-') and metavar else value)
    visit(function(tree, 'build_parser').body, {})
    return commands


def verify(manifest, source):
    revision = subprocess.check_output(['git', '-C', str(source), 'rev-parse', 'HEAD'], text=True).strip()
    check(revision == manifest['source']['revision'], 'reference HEAD differs from manifest pin')
    check(not subprocess.check_output(['git', '-C', str(source), 'status', '--porcelain'], text=True).strip(), 'reference checkout contains uncommitted changes')
    root = source / 'src' / 'agent_bridge'
    trees = {name: ast.parse((root / f'{name}.py').read_text(), filename=f'{name}.py') for name in ['storage', 'cli', 'config', 'mcp_server', 'automation', 'codex_client', 'runtime', 'worktree_runtime']}
    version = constant(trees['storage'], 'SCHEMA_VERSION')
    check(version == manifest['source']['schema_version'] == manifest['storage']['schema_version'], 'schema version mismatch')
    check(list(constant(trees['storage'], 'MIGRATABLE_VERSIONS')) == manifest['storage']['migratable_versions'], 'migration allowlist mismatch')
    check(manifest['contract_baselines']['reference_target'] == {'revision': revision, 'schema_version': version}, 'reference target mismatch')
    check(manifest['contract_baselines']['rust_storage']['schema_version'] == 15, 'manifest refresh must preserve declared current Rust target')
    check(manifest['contract_baselines']['fixtures_0A']['schema_version'] == 15, 'historical fixture baseline changed')

    actual = cli_surface(trees['cli'])
    declared = {c['name']: {o['name'] for o in c['options']} for c in manifest['cli']['commands']}
    check(set(actual) == set(declared), 'CLI command coverage mismatch')
    for name in actual:
        check(actual[name] == declared[name], f'CLI option coverage mismatch: {name}')
    for name in ['automation-status', 'automation-pause', 'automation-resume', 'automation-stop', 'automation-worker']:
        command = next(c for c in manifest['cli']['commands'] if c['name'] == name)
        option = next(o for o in command['options'] if o['name'] == '--run')
        check(option['required'] == (name == 'automation-worker'), 'automation --run requirement mismatch')

    wrappers = [n for n in ast.walk(trees['mcp_server']) if isinstance(n, ast.FunctionDef) and any(isinstance(d, ast.Call) and isinstance(d.func, ast.Attribute) and d.func.attr == 'tool' for d in n.decorator_list)]
    tools = {t['name']: t for t in manifest['mcp']['tools']}
    check(len(wrappers) == manifest['mcp']['tool_count'] == len(tools) == 6, 'public MCP tool count mismatch')
    check({n.name for n in wrappers} == set(tools), 'MCP tool names mismatch')
    for wrapper in wrappers:
        parameters = {p['name']: p for p in tools[wrapper.name]['parameters']}
        check({a.arg for a in wrapper.args.args} == set(parameters), f'MCP argument coverage mismatch: {wrapper.name}')
        defaults = [None] * (len(wrapper.args.args) - len(wrapper.args.defaults)) + list(wrapper.args.defaults)
        for argument, default in zip(wrapper.args.args, defaults):
            parameter = parameters[argument.arg]
            check(parameter['required'] == (default is None), 'MCP required/default mismatch')
            if default is not None:
                if isinstance(default, ast.Name):
                    imports = [n for n in trees['mcp_server'].body if isinstance(n, ast.ImportFrom) and any(a.name == default.id for a in n.names)]
                    check(len(imports) == 1 and imports[0].module in trees, 'unsupported MCP default constant import')
                    value = constant(trees[imports[0].module], default.id)
                else:
                    value = ast.literal_eval(default)
                check(type(parameter.get('default')) is type(value) and parameter.get('default') == value, 'MCP default mismatch')
    internal = manifest['mcp']['internal_only']
    for name in ['submit_task_impl', 'request_changes_impl']:
        check([a.arg for a in function(trees['mcp_server'], name).args.kwonlyargs] == internal[name], f'internal MCP keyword-only inputs mismatch: {name}')

    config = next(n for n in trees['config'].body if isinstance(n, ast.ClassDef) and n.name == 'ProjectConfig')
    fields = {n.target.id: n for n in config.body if isinstance(n, ast.AnnAssign)}
    for name in ['delivery_mode', 'auto_approve_state_directory']:
        check(name in manifest['config']['project_keys']['optional'], f'config key missing: {name}')
        value = ast.literal_eval(fields[name].value)
        declared_default = manifest['config']['defaults'][name]
        check(type(value) is type(declared_default) and value == declared_default, f'config default mismatch: {name}')
    check(list(constant(trees['storage'], 'DELIVERY_MODES')) == manifest['delivery']['task_policy']['values'], 'stored delivery modes mismatch')
    check(list(constant(trees['config'], 'DELIVERY_MODES')) == manifest['delivery']['task_policy']['values'], 'config delivery modes mismatch')

    # Execute extracted literal DDL only, in RAM. Never run Python migrations or open files.
    initialize = function(trees['storage'], 'initialize')
    ddl = [n.args[0].value for n in ast.walk(initialize) if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute) and n.func.attr == 'executescript' and isinstance(n.args[0], ast.Constant)]
    check(len(ddl) == 1, 'fresh schema literal DDL missing or ambiguous')
    migration_literals = sql_constants(function(trees['storage'], '_migrate'))
    additions = [sql for sql in migration_literals if sql.startswith('CREATE TABLE IF NOT EXISTS automation_runs') or sql.startswith('CREATE UNIQUE INDEX IF NOT EXISTS ux_automation_unfinished')]
    check(len(additions) == 2, 'automation schema DDL coverage mismatch')
    with sqlite3.connect(':memory:') as database:
        database.executescript(ddl[0])
        database.execute(constant(trees['storage'], '_UX_ACTIVE_WRITERS_SINGLE_SQL'))
        for sql in additions:
            database.execute(sql)
        names = {r[0] for r in database.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")}
        check(names == set(manifest['storage']['tables']), 'table coverage mismatch')
        for name, table in manifest['storage']['tables'].items():
            rows = database.execute(f'PRAGMA table_info("{name}")').fetchall()
            columns = {r[1]: (r[2], not bool(r[3] or r[5]), r[4]) for r in rows}
            check(set(columns) == {c['name'] for c in table['columns']}, f'column coverage mismatch: {name}')
            for c in table['columns']:
                actual_type, nullable, default = columns[c['name']]
                check((actual_type, nullable) == (c['type'], c['nullable']), f'column type/nullability mismatch: {name}.{c["name"]}')
                if 'default' in c:
                    check(default is not None and default.strip("'") == str(c['default']), f'column default mismatch: {name}.{c["name"]}')
                else:
                    check(default is None, f'undocumented default: {name}.{c["name"]}')
            check([r[1] for r in sorted(rows, key=lambda r: r[5]) if r[5]] == table['primary_key'], f'primary key mismatch: {name}')
            foreign = sorted(f'{r[3]} REFERENCES {r[2]}({r[4]})' for r in database.execute(f'PRAGMA foreign_key_list("{name}")'))
            check(foreign == sorted(table.get('foreign_keys', [])), f'foreign key mismatch: {name}')
        indexes = dict(database.execute("SELECT name,sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL"))
        check(set(indexes) == {i['name'] for i in manifest['storage']['indexes']}, 'index coverage mismatch')
        for index in manifest['storage']['indexes']:
            sql = indexes[index['name']]
            unique = 'UNIQUE ' if re.match(r'CREATE\s+UNIQUE', sql, re.I) else ''
            definition = unique + re.split(r'\bON\s+', sql, maxsplit=1, flags=re.I)[1]
            check(normalized_sql(definition) == normalized_sql(index['definition']), f'index definition/predicate mismatch: {index["name"]}')

    auto = manifest['automation']
    check(auto['codex']['output_schemas'] == constant(trees['codex_client'], 'SCHEMAS'), 'Codex structured output schema mismatch')
    check(set(auto['run']['terminal']) == constant(trees['automation'], 'TERMINAL'), 'automation terminal states mismatch')
    check(auto['plan']['step_id_pattern'] in sql_constants(trees['automation']), 'automation step id pattern mismatch')
    plan = function(trees['automation'], 'validate_plan')
    allowed = next(ast.literal_eval(n.value) for n in ast.walk(plan) if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'allowed' for t in n.targets))
    check(set(auto['plan']['allowed_keys']) == allowed, 'approved plan keys mismatch')
    step_keys = [n for n in ast.walk(plan) if isinstance(n, ast.Set) and all(isinstance(e, ast.Constant) and isinstance(e.value, str) for e in n.elts) and {'id', 'task', 'allowed_paths'} <= {e.value for e in n.elts}]
    check(len(step_keys) == 1 and {e.value for e in step_keys[0].elts} == set(auto['plan']['step_allowed_keys']), 'approved step keys mismatch')
    limit_loop = [n for n in ast.walk(plan) if isinstance(n, ast.For) and isinstance(n.target, ast.Tuple) and [e.id for e in n.target.elts] == ['key', 'default', 'maximum']]
    check(len(limit_loop) == 1, 'plan limits loop missing or ambiguous')
    limits = {}
    for item in limit_loop[0].iter.elts:
        key, default, maximum = item.elts
        default_value = 'project.max_rounds' if isinstance(default, ast.Attribute) and ast.unparse(default) == 'config.max_rounds' else ast.literal_eval(default)
        limits[ast.literal_eval(key)] = {'default': default_value, 'min': 1, 'max': ast.literal_eval(maximum)}
    check(auto['plan']['limits'] == limits, 'approved plan limits mismatch')
    adapter = function(trees['codex_client'], 'call')
    argv = [n.value for n in ast.walk(adapter) if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'argv' for t in n.targets)]
    check(len(argv) == 1 and isinstance(argv[0], ast.List), 'Codex argv missing or ambiguous')
    literal_flags = [n.value for n in argv[0].elts if isinstance(n, ast.Constant)]
    check(literal_flags == [s for s in auto['codex']['argv_contract'] if not s.startswith('<')], 'Codex read-only argv flags mismatch')
    wait = manifest['runtime']['manager_lock_wait']
    check(wait['poll_initial_seconds'] == constant(trees['runtime'], 'LOCK_POLL_INITIAL'), 'runtime poll initial mismatch')
    check(wait['poll_max_seconds'] == constant(trees['runtime'], 'LOCK_POLL_MAX'), 'runtime poll cap mismatch')
    check(wait['worktree_start_seconds'] == 3 * constant(trees['worktree_runtime'], 'READY_TIMEOUT'), 'worktree startup wait mismatch')
    check(auto['internal_mcp']['public_mcp_exposure'] is False, 'automation inputs exposed as public MCP')
    check(subprocess.check_output(['git', '-C', str(source), 'rev-parse', 'HEAD'], text=True).strip() == revision, 'reference HEAD changed during verification')
    check(not subprocess.check_output(['git', '-C', str(source), 'status', '--porcelain'], text=True).strip(), 'reference checkout changed during verification')
    print(f'ok: pinned source, {len(actual)} CLI commands/options, 6 MCP signatures/defaults, config defaults, {len(names)} SQLite tables and {len(indexes)} indexes, automation schemas and runtime bounds; source AST + RAM SQLite only')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=Path('/home/denis/Python/agent_bridge'))
    parser.add_argument('--manifest', type=Path, default=Path(__file__).with_name('contract-manifest.json'))
    args = parser.parse_args()
    try:
        verify(json.loads(args.manifest.read_text()), args.source)
    except (ContractError, OSError, ValueError, KeyError, StopIteration, sqlite3.Error, subprocess.CalledProcessError) as error:
        print(f'error: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
