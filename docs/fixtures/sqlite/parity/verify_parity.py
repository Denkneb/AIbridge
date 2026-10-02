#!/usr/bin/env python3
"""Read-only source-parity verifier for the SQLite contract fixtures.

Unlike ``../verify.py`` (which only proves the committed fixtures are
self-consistent with their expectation JSON), this verifier imports the
**actual pinned Python reference** and checks that the fixtures and the
documented schema really match the live source:

* ``Storage.initialize`` on a fresh temporary database must produce exactly the
  structural schema of ``empty-v15.sqlite`` (tables, columns, types, defaults,
  nullability, primary keys, foreign keys, named indexes, ``user_version`` and
  ``meta``);
* ``Storage.initialize`` on temporary copies of the four Rust-owned legacy v6
  fixtures must migrate 6 -> 15, preserving every history row (including event
  ids) and adding the documented new columns/tables/defaults;
* writer admission must really admit two disjoint parallel writers, refuse a
  second non-parallel reservation, and refuse an overlapping parallel writer
  through the application code -- while the SQL schema itself imposes no scope
  constraint (only the partial unique index over ``parallel=0``).

Everything runs against isolated ``tempfile`` directories; the committed
fixtures are only ever opened read-only, and no runtime database, config or
Python cache is touched.  Run it with the reference environment::

    PYTHONDONTWRITEBYTECODE=1 \\
    /home/denis/Python/agent_bridge/.venv/bin/python \\
        docs/fixtures/sqlite/parity/verify_parity.py \\
        --python-repo /home/denis/Python/agent_bridge

Exit code 0 means every check passed, 1 means a parity check failed, 2 means
the reference itself could not be used.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True

PINNED_COMMIT = "86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a"
HERE = Path(__file__).resolve().parent
FIXTURES = HERE.parent
LEGACY_FIXTURES = (
    "active-v6.sqlite",
    "awaiting-review-v6.sqlite",
    "empty-v6.sqlite",
    "terminal-v6.sqlite",
)
V15_TARGET = "empty-v15.sqlite"

WRITER_STATUSES = (
    "implementing",
    "awaiting_review",
    "revising",
    "needs_user",
    "failed",
    "delivery_unknown",
)
V15_NEW_TASK_COLUMNS = {
    "budget_json": None,
    "workflow_id": None,
    "depends_on": None,
    "execution_mode": "direct",
    "profile": None,
    "profile_json": None,
    "profile_hash": None,
    "profile_source": None,
}
V15_NEW_ROUND_COLUMNS = {
    "structured_findings": None,
    "checkpoint_json": None,
}


class Checks:
    def __init__(self) -> None:
        self.passed = 0
        self.failed: list[tuple[str, str]] = []
        self.skipped: list[tuple[str, str]] = []

    def check(self, condition: bool, label: str, detail: str = "") -> bool:
        if condition:
            self.passed += 1
            print(f"PASS {label}")
        else:
            self.failed.append((label, detail))
            print(f"FAIL {label} {detail}")
        return condition

    def skip(self, label: str, reason: str) -> None:
        self.skipped.append((label, reason))
        print(f"SKIP {label}: {reason}")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _connect_ro(path: Path) -> sqlite3.Connection:
    conn = sqlite3.connect(f"file:{path}?mode=ro&immutable=1", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def read_structure(path: Path) -> dict:
    conn = _connect_ro(path)
    try:
        table_names = [
            row[0]
            for row in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table' "
                "AND name NOT LIKE 'sqlite_%' ORDER BY name"
            )
        ]
        tables = {
            table: [
                {
                    "name": row["name"],
                    "type": row["type"],
                    "notnull": bool(row["notnull"]),
                    "pk": row["pk"],
                    "default": row["dflt_value"],
                }
                for row in conn.execute(f"PRAGMA table_info({table})")
            ]
            for table in table_names
        }
        indexes = {}
        for row in conn.execute(
            "SELECT name, tbl_name FROM sqlite_master WHERE type='index' "
            "AND sql IS NOT NULL ORDER BY name"
        ):
            listing = {
                item["name"]: item
                for item in conn.execute(f"PRAGMA index_list({row['tbl_name']})")
            }
            info = listing[row["name"]]
            indexes[row["name"]] = {
                "table": row["tbl_name"],
                "unique": bool(info["unique"]),
                "partial": bool(info["partial"]),
                "columns": [
                    item["name"]
                    for item in conn.execute(f"PRAGMA index_info({row['name']})")
                ],
            }
        foreign_keys = sorted(
            (
                {
                    "table": table,
                    "from": row["from"],
                    "to_table": row["table"],
                    "to_column": row["to"],
                }
                for table in table_names
                for row in conn.execute(f"PRAGMA foreign_key_list({table})")
            ),
            key=lambda fk: (fk["table"], fk["from"], fk["to_table"], fk["to_column"]),
        )
        meta = {
            row["key"]: row["value"]
            for row in conn.execute("SELECT key, value FROM meta")
        }
        return {
            "user_version": conn.execute("PRAGMA user_version").fetchone()[0],
            "meta": meta,
            "tables": tables,
            "indexes": indexes,
            "foreign_keys": foreign_keys,
        }
    finally:
        conn.close()


def read_rows(path: Path, table: str) -> list[dict]:
    conn = _connect_ro(path)
    try:
        return [dict(row) for row in conn.execute(f"SELECT * FROM {table}")]
    finally:
        conn.close()


def load_reference(repo: Path, expected_commit: str):
    head = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
    )
    if head.returncode != 0:
        raise SystemExit(f"cannot read HEAD of {repo}: {head.stderr.strip()}")
    if head.stdout.strip() != expected_commit:
        raise SystemExit(
            f"reference HEAD {head.stdout.strip()} != pinned {expected_commit}"
        )
    src = (repo / "src").resolve()
    sys.path.insert(0, str(src))
    import agent_bridge  # noqa: E402
    from agent_bridge import config as config_module  # noqa: E402
    from agent_bridge import storage as storage_module  # noqa: E402

    loaded = Path(agent_bridge.__file__).resolve().parent
    if loaded != src / "agent_bridge":
        raise SystemExit(
            f"imported agent_bridge from {loaded}, expected {src / 'agent_bridge'}"
        )
    return config_module, storage_module


def make_config(config_module, state_root: Path, *, parallel: bool, max_active: int):
    return config_module.ProjectConfig(
        project_id="proj",
        workspace=state_root / "workspace",
        host="127.0.0.1",
        port=1,
        password_file=state_root / "password",
        max_rounds=5,
        config_path=state_root / "projects.toml",
        state_root=state_root,
        max_active_tasks=max_active,
        allow_parallel_writers=parallel,
    )


def fresh_state_db(storage_module, config_module, tmp: Path, *, parallel: bool, max_active: int):
    state = tmp / "state"
    (state / "workspace").mkdir(parents=True, exist_ok=True)
    config = make_config(config_module, state, parallel=parallel, max_active=max_active)
    storage = storage_module.Storage(config)
    storage.initialize()
    return config, storage


def check_reference_and_fixtures(checks: Checks, config_module, storage_module) -> None:
    checks.check(
        (FIXTURES / V15_TARGET).is_file(), "fresh v15 target fixture exists"
    )
    for name in LEGACY_FIXTURES:
        checks.check((FIXTURES / name).is_file(), f"legacy v6 fixture {name} exists")
    empty_v6 = read_structure(FIXTURES / "empty-v6.sqlite")
    checks.check(
        empty_v6["user_version"] == 6,
        "legacy empty-v6 fixture is still schema v6 (not renamed/upgraded)",
        f"user_version={empty_v6['user_version']}",
    )
    empty_v15 = read_structure(FIXTURES / V15_TARGET)
    checks.check(
        empty_v15["user_version"] == 15,
        "fresh v15 target is schema 15",
        f"user_version={empty_v15['user_version']}",
    )

    with tempfile.TemporaryDirectory(prefix="parity-fresh-") as tmp_name:
        tmp = Path(tmp_name)
        _, storage = fresh_state_db(
            storage_module, config_module, tmp, parallel=False, max_active=1
        )
        source = read_structure(storage.path)
        fixture = read_structure(FIXTURES / V15_TARGET)
        checks.check(
            source["user_version"] == 15,
            "Storage.initialize fresh DB reports user_version 15",
            str(source["user_version"]),
        )
        checks.check(
            source["meta"] == {"schema_version": "15"},
            "Storage.initialize fresh DB meta.schema_version == 15",
            str(source["meta"]),
        )
        checks.check(
            source["tables"] == fixture["tables"],
            "fresh source table columns/types/defaults match the fixture",
            _diff(source["tables"], fixture["tables"]),
        )
        checks.check(
            source["indexes"] == fixture["indexes"],
            "fresh source named indexes match the fixture",
            _diff(source["indexes"], fixture["indexes"]),
        )
        checks.check(
            source["foreign_keys"] == fixture["foreign_keys"],
            "fresh source foreign keys match the fixture",
            _diff(source["foreign_keys"], fixture["foreign_keys"]),
        )
        checks.check(
            set(source["indexes"]) == {
                "ix_active_writers_project",
                "ix_events_task",
                "ix_tasks_project_status",
                "ux_active_writers_single",
                "ux_rounds_request",
            },
            "fresh source exposes exactly the v15 index set",
            str(sorted(source["indexes"])),
        )
        for forbidden in ("ux_tasks_active", "ux_active_writers_project"):
            checks.check(
                forbidden not in source["indexes"],
                f"legacy index {forbidden} is absent from a fresh v15 DB",
            )
        single = source["indexes"].get("ux_active_writers_single", {})
        checks.check(
            single.get("unique") is True
            and single.get("partial") is True
            and single.get("table") == "active_writers"
            and single.get("columns") == ["project_id"],
            "ux_active_writers_single is the partial unique writer index",
            str(single),
        )


def _diff(left: object, right: object) -> str:
    if left == right:
        return ""
    return f"left={left!r} right={right!r}"


def check_migration(checks: Checks, config_module, storage_module) -> None:
    for name in LEGACY_FIXTURES:
        with tempfile.TemporaryDirectory(prefix=f"parity-migrate-{name}-") as tmp_name:
            tmp = Path(tmp_name)
            state = tmp / "state"
            (state / "proj").mkdir(parents=True)
            (state / "workspace").mkdir(parents=True, exist_ok=True)
            db = state / "proj" / "state.sqlite"
            shutil.copy2(FIXTURES / name, db)

            before_tasks = read_rows(db, "tasks")
            before_rounds = read_rows(db, "rounds")
            before_events = read_rows(db, "events")

            config = make_config(config_module, state, parallel=False, max_active=1)
            storage = storage_module.Storage(config)
            storage.initialize()

            structure = read_structure(db)
            checks.check(
                structure["user_version"] == 15,
                f"{name}: migrated user_version is 15",
                str(structure["user_version"]),
            )
            checks.check(
                structure["meta"] == {"schema_version": "15"},
                f"{name}: migrated meta.schema_version is 15",
                str(structure["meta"]),
            )

            after_tasks = {row["task_id"]: row for row in read_rows(db, "tasks")}
            checks.check(
                set(after_tasks) == {row["task_id"] for row in before_tasks},
                f"{name}: task ids preserved through migration",
                str(sorted(after_tasks)),
            )
            for row in before_tasks:
                after = after_tasks[row["task_id"]]
                common_ok = all(after[column] == row[column] for column in row)
                checks.check(
                    common_ok,
                    f"{name}: task {row['task_id']} history columns preserved",
                    str({c: (row[c], after[c]) for c in row if after[c] != row[c]}),
                )
                defaults_ok = all(
                    after.get(column) == value
                    for column, value in V15_NEW_TASK_COLUMNS.items()
                )
                checks.check(
                    defaults_ok,
                    f"{name}: task {row['task_id']} new v15 columns defaulted",
                    str({c: after.get(c) for c in V15_NEW_TASK_COLUMNS}),
                )

            after_rounds = {
                (row["task_id"], row["round_number"]): row
                for row in read_rows(db, "rounds")
            }
            checks.check(
                set(after_rounds)
                == {(row["task_id"], row["round_number"]) for row in before_rounds},
                f"{name}: round keys preserved through migration",
            )
            for row in before_rounds:
                after = after_rounds[(row["task_id"], row["round_number"])]
                common_ok = all(after[column] == row[column] for column in row)
                checks.check(
                    common_ok,
                    f"{name}: round {row['task_id']}/{row['round_number']} history preserved",
                    str({c: (row[c], after[c]) for c in row if after[c] != row[c]}),
                )
                defaults_ok = all(
                    after.get(column) == value
                    for column, value in V15_NEW_ROUND_COLUMNS.items()
                )
                checks.check(
                    defaults_ok,
                    f"{name}: round {row['task_id']}/{row['round_number']} new v15 columns defaulted",
                    str({c: after.get(c) for c in V15_NEW_ROUND_COLUMNS}),
                )

            after_events = read_rows(db, "events")
            checks.check(
                after_events == before_events,
                f"{name}: event rows (including ids) preserved",
                _diff(after_events, before_events),
            )

            expected_writers = {
                row["task_id"]: row["allowed_paths"]
                for row in before_tasks
                if row["status"] in WRITER_STATUSES
            }
            writers = read_rows(db, "active_writers")
            checks.check(
                {row["task_id"] for row in writers} == set(expected_writers),
                f"{name}: active_writers reconciled to persisted writer tasks",
                str([(r["task_id"], r["parallel"]) for r in writers]),
            )
            checks.check(
                all(row["parallel"] == 0 for row in writers),
                f"{name}: migrated writer reservations default to parallel=0",
            )
            checks.check(
                all(
                    row["scopes_json"] == expected_writers[row["task_id"]]
                    for row in writers
                ),
                f"{name}: migrated writer scopes come from allowed_paths",
            )

            for table in ("worktrees", "worktree_quarantine"):
                checks.check(
                    read_rows(db, table) == [],
                    f"{name}: {table} created empty by the migration",
                )

            checks.check(
                set(structure["indexes"])
                == {
                    "ix_active_writers_project",
                    "ix_events_task",
                    "ix_tasks_project_status",
                    "ux_active_writers_single",
                    "ux_rounds_request",
                },
                f"{name}: migrated index set is the final v15 set",
                str(sorted(structure["indexes"])),
            )
            for forbidden in ("ux_tasks_active", "ux_active_writers_project"):
                checks.check(
                    forbidden not in structure["indexes"],
                    f"{name}: legacy index {forbidden} dropped",
                )


def check_column_coverage(checks: Checks, config_module, storage_module) -> None:
    with tempfile.TemporaryDirectory(prefix="parity-columns-") as tmp_name:
        tmp = Path(tmp_name)
        state = tmp / "state"
        (state / "proj").mkdir(parents=True)
        (state / "workspace").mkdir(parents=True, exist_ok=True)
        shutil.copy2(FIXTURES / "terminal-v6.sqlite", state / "proj" / "state.sqlite")
        config = make_config(config_module, state, parallel=False, max_active=1)
        storage_module.Storage(config).initialize()
        structure = read_structure(state / "proj" / "state.sqlite")
        task_columns = {column["name"] for column in structure["tables"]["tasks"]}
        round_columns = {column["name"] for column in structure["tables"]["rounds"]}
        checks.check(
            set(V15_NEW_TASK_COLUMNS) <= task_columns,
            "migrated tasks expose budget/workflow/deps/execution_mode/profile* columns",
            str(sorted(set(V15_NEW_TASK_COLUMNS) - task_columns)),
        )
        checks.check(
            {"findings", "structured_findings", "checkpoint_json"} <= round_columns,
            "migrated rounds expose findings/structured_findings/checkpoint_json",
            str(sorted({"findings", "structured_findings", "checkpoint_json"} - round_columns)),
        )
        checks.check(
            {"worktrees", "worktree_quarantine", "active_writers"} <= set(structure["tables"]),
            "migrated DB exposes worktrees/quarantine/active_writers tables",
        )
        writer_columns = {
            column["name"] for column in structure["tables"]["active_writers"]
        }
        checks.check(
            "parallel" in writer_columns,
            "active_writers exposes the v15 parallel flag",
        )


def check_findings_and_waiting(checks: Checks, config_module, storage_module) -> None:
    # Findings live on a revise round in v6 and must survive migration untouched.
    with tempfile.TemporaryDirectory(prefix="parity-findings-") as tmp_name:
        tmp = Path(tmp_name)
        state = tmp / "state"
        (state / "proj").mkdir(parents=True)
        (state / "workspace").mkdir(parents=True, exist_ok=True)
        db = state / "proj" / "state.sqlite"
        shutil.copy2(FIXTURES / "active-v6.sqlite", db)
        conn = sqlite3.connect(db)
        try:
            conn.execute(
                "UPDATE rounds SET kind='revise', findings=? WHERE task_id='task-1'",
                ("fixture review note",),
            )
            conn.commit()
        finally:
            conn.close()
        config = make_config(config_module, state, parallel=False, max_active=1)
        storage_module.Storage(config).initialize()
        rows = read_rows(db, "rounds")
        checks.check(
            any(row.get("findings") == "fixture review note" for row in rows),
            "revise findings preserved across the 6 -> 15 migration",
            str([row.get("findings") for row in rows]),
        )

    # A waiting_dependencies task must survive but never receive a writer
    # reservation (it reserves no slot until activation).
    with tempfile.TemporaryDirectory(prefix="parity-waiting-") as tmp_name:
        tmp = Path(tmp_name)
        state = tmp / "state"
        (state / "proj").mkdir(parents=True)
        (state / "workspace").mkdir(parents=True, exist_ok=True)
        db = state / "proj" / "state.sqlite"
        shutil.copy2(FIXTURES / "empty-v6.sqlite", db)
        conn = sqlite3.connect(db)
        try:
            conn.execute(
                "INSERT INTO tasks(task_id, project_id, workspace, status, session_id, "
                "task, allowed_paths, test_commands, created_at, updated_at, base_head, "
                "snapshot, revision_count, close_requested_at, close_reason) "
                "VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (
                    "task-wait",
                    "proj",
                    "/fixture/workspace",
                    "waiting_dependencies",
                    None,
                    "Implement the fixture change",
                    '["src/"]',
                    "[]",
                    "2026-01-01T00:00:00.000+00:00",
                    "2026-01-01T00:00:00.000+00:00",
                    None,
                    None,
                    0,
                    None,
                    None,
                ),
            )
            conn.execute(
                "INSERT INTO rounds(task_id, project_id, round_number, request_id, "
                "payload_hash, kind, status, attempted, created_at, updated_at) "
                "VALUES(?,?,?,?,?,?,?,?,?,?)",
                (
                    "task-wait",
                    "proj",
                    1,
                    "req-wait",
                    "hash-req-wait",
                    "implement",
                    "pending",
                    0,
                    "2026-01-01T00:00:00.000+00:00",
                    "2026-01-01T00:00:00.000+00:00",
                ),
            )
            conn.commit()
        finally:
            conn.close()
        config = make_config(config_module, state, parallel=False, max_active=1)
        storage_module.Storage(config).initialize()
        tasks = {row["task_id"]: row for row in read_rows(db, "tasks")}
        writers = read_rows(db, "active_writers")
        checks.check(
            tasks["task-wait"]["status"] == "waiting_dependencies"
            and tasks["task-wait"]["execution_mode"] == "direct",
            "waiting_dependencies task preserved with execution_mode default",
            str(tasks.get("task-wait")),
        )
        checks.check(
            writers == [],
            "waiting_dependencies task does not own an active_writers reservation",
            str(writers),
        )


def check_admission(checks: Checks, config_module, storage_module) -> None:
    with tempfile.TemporaryDirectory(prefix="parity-admit-parallel-") as tmp_name:
        tmp = Path(tmp_name)
        config, storage = fresh_state_db(
            storage_module, config_module, tmp, parallel=True, max_active=10
        )
        storage.create_task(
            task_id="task-a",
            round_request_id="req-a",
            payload_hash="hash-a",
            kind="implement",
            task="fixture A",
            allowed_paths=["src/a.py"],
            test_commands=[],
            base_head=None,
            snapshot=None,
            execution_mode="worktree",
        )
        storage.create_task(
            task_id="task-b",
            round_request_id="req-b",
            payload_hash="hash-b",
            kind="implement",
            task="fixture B",
            allowed_paths=["src/b.py"],
            test_commands=[],
            base_head=None,
            snapshot=None,
            execution_mode="worktree",
        )
        writers = read_rows(config.db_path, "active_writers")
        checks.check(
            {row["task_id"] for row in writers} == {"task-a", "task-b"}
            and all(row["parallel"] == 1 for row in writers),
            "disjoint parallel writers are admitted (parallel=1)",
            str([(r["task_id"], r["parallel"]) for r in writers]),
        )
        try:
            storage.create_task(
                task_id="task-c",
                round_request_id="req-c",
                payload_hash="hash-c",
                kind="implement",
                task="fixture C",
                allowed_paths=["src/a.py"],
                test_commands=[],
                base_head=None,
                snapshot=None,
                execution_mode="worktree",
            )
        except storage_module.ScopeOverlap:
            checks.check(True, "overlapping parallel writer is refused by source admission")
        except Exception as exc:  # noqa: BLE001
            checks.check(False, "overlapping parallel writer is refused", repr(exc))
        else:
            checks.check(False, "overlapping parallel writer is refused", "was admitted")

    with tempfile.TemporaryDirectory(prefix="parity-admit-single-") as tmp_name:
        tmp = Path(tmp_name)
        config, storage = fresh_state_db(
            storage_module, config_module, tmp, parallel=False, max_active=10
        )
        storage.create_task(
            task_id="task-a",
            round_request_id="req-a",
            payload_hash="hash-a",
            kind="implement",
            task="fixture A",
            allowed_paths=["src/a.py"],
            test_commands=[],
            base_head=None,
            snapshot=None,
        )
        try:
            storage.create_task(
                task_id="task-b",
                round_request_id="req-b",
                payload_hash="hash-b",
                kind="implement",
                task="fixture B",
                allowed_paths=["src/b.py"],
                test_commands=[],
                base_head=None,
                snapshot=None,
            )
        except storage_module.ProjectBusy:
            checks.check(True, "second non-parallel reservation is refused (ProjectBusy)")
        except Exception as exc:  # noqa: BLE001
            checks.check(False, "second non-parallel reservation is refused", repr(exc))
        else:
            checks.check(False, "second non-parallel reservation is refused", "was admitted")

    # SQL-level proof: the schema constrains only parallel=0 rows and never the
    # scopes.  Two parallel=1 overlapping rows must be insertable directly.
    with tempfile.TemporaryDirectory(prefix="parity-sql-") as tmp_name:
        tmp = Path(tmp_name)
        config, storage = fresh_state_db(
            storage_module, config_module, tmp, parallel=False, max_active=1
        )
        conn = sqlite3.connect(config.db_path)
        try:
            conn.execute(
                "INSERT INTO active_writers(task_id, project_id, scopes_json, "
                "created_at, parallel) VALUES('w1','proj','[\"src/\"]',"
                "'2026-01-01T00:00:00.000+00:00',1)"
            )
            conn.execute(
                "INSERT INTO active_writers(task_id, project_id, scopes_json, "
                "created_at, parallel) VALUES('w2','proj','[\"src/\"]',"
                "'2026-01-01T00:00:01.000+00:00',1)"
            )
            conn.commit()
            overlapping_ok = True
        except sqlite3.Error as exc:  # pragma: no cover - would be a schema bug
            overlapping_ok = False
            detail = str(exc)
        finally:
            conn.close()
        checks.check(
            overlapping_ok,
            "SQL schema allows two parallel=1 overlapping rows (no scope constraint)",
            "" if overlapping_ok else detail,
        )

        conn = sqlite3.connect(config.db_path)
        try:
            conn.execute(
                "INSERT INTO active_writers(task_id, project_id, scopes_json, "
                "created_at, parallel) VALUES('s1','proj','[\"a\"]',"
                "'2026-01-01T00:00:02.000+00:00',0)"
            )
            conn.execute(
                "INSERT INTO active_writers(task_id, project_id, scopes_json, "
                "created_at, parallel) VALUES('s2','proj','[\"b\"]',"
                "'2026-01-01T00:00:03.000+00:00',0)"
            )
            conn.commit()
            single_ok = False
            detail = "second parallel=0 row was admitted"
        except sqlite3.IntegrityError:
            single_ok = True
            detail = ""
        finally:
            conn.close()
        checks.check(
            single_ok,
            "ux_active_writers_single rejects a second parallel=0 reservation in SQL",
            detail,
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--python-repo",
        default=os.environ.get("AGENT_BRIDGE_PYTHON_REPO"),
        help="path to the pinned agent_bridge Python checkout (or set "
        "AGENT_BRIDGE_PYTHON_REPO)",
    )
    parser.add_argument("--expected-commit", default=PINNED_COMMIT)
    args = parser.parse_args()
    if not args.python_repo:
        print(
            "error: pass --python-repo or set AGENT_BRIDGE_PYTHON_REPO",
            file=sys.stderr,
        )
        return 2
    repo = Path(args.python_repo).expanduser().resolve()
    if not (repo / "src" / "agent_bridge").is_dir():
        print(f"error: {repo} is not an agent_bridge checkout", file=sys.stderr)
        return 2

    config_module, storage_module = load_reference(repo, args.expected_commit)

    fixture_paths = sorted(FIXTURES.glob("*.sqlite"))
    before = {path.name: _sha256(path) for path in fixture_paths}

    checks = Checks()
    check_reference_and_fixtures(checks, config_module, storage_module)
    check_migration(checks, config_module, storage_module)
    check_column_coverage(checks, config_module, storage_module)
    check_findings_and_waiting(checks, config_module, storage_module)
    check_admission(checks, config_module, storage_module)

    after = {path.name: _sha256(path) for path in sorted(FIXTURES.glob("*.sqlite"))}
    checks.check(before == after, "committed fixture bytes unchanged by the verifier")

    print(
        f"\n{checks.passed} passed, {len(checks.failed)} failed, "
        f"{len(checks.skipped)} skipped"
    )
    if checks.failed:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
