#!/usr/bin/env python3
"""Read-only verifier for the SQLite contract fixtures.

Validates both fixture generations against their expectation files:

* the four historical schema **v6** databases against ``expected.json``, and
* the fresh empty schema **v15** target ``empty-v15.sqlite`` against
  ``expected-v15.json``.

Every database is opened with ``mode=ro&immutable=1``; the verifier refuses a
fixture that has a ``-wal``/``-shm`` sidecar and re-hashes every fixture after
the checks to prove it never changed a single byte.  The verifier uses only the
Python standard library (``sqlite3``/``json``/``hashlib``).  It does not import
the Python reference runtime and never touches a runtime database; the
source-level parity checks live in ``parity/verify_parity.py``.

Usage::

    python3 docs/fixtures/sqlite/verify.py

Exit code 0 means every fixture matched; 1 means at least one check failed and
2 means an expectation manifest itself is unusable.
"""

from __future__ import annotations

import hashlib
import json
import sqlite3
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
EXPECTED_PATHS = (HERE / "expected.json", HERE / "expected-v15.json")

ACTIVE_STATUSES = (
    "implementing",
    "awaiting_review",
    "revising",
    "needs_user",
    "failed",
    "delivery_unknown",
)
TERMINAL_STATUSES = ("accepted", "closed")
ROUND_STATUSES = {
    "pending",
    "sent",
    "observing",
    "needs_user",
    "delivery_unknown",
    "failed",
    "complete",
}
VERIFIER_STATES = {None, "running", "done"}
ROW_TABLES = (
    "tasks",
    "rounds",
    "events",
    "active_writers",
    "worktrees",
    "worktree_quarantine",
)


class Reporter:
    def __init__(self) -> None:
        self.errors: list[str] = []

    def error(self, database: str, message: str) -> None:
        self.errors.append(f"{database}: {message}")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _normalize_task(row: sqlite3.Row) -> dict:
    record = dict(row)
    record["allowed_paths"] = json.loads(record["allowed_paths"])
    record["test_commands"] = json.loads(record["test_commands"])
    record["snapshot"] = (
        json.loads(record["snapshot"]) if record["snapshot"] is not None else None
    )
    return record


def _normalize_round(row: sqlite3.Row) -> dict:
    record = dict(row)
    record["attempted"] = bool(record["attempted"])
    for column in ("result_json", "verifier_json"):
        record[column] = (
            json.loads(record[column]) if record[column] is not None else None
        )
    return record


def _normalize_event(row: sqlite3.Row) -> dict:
    record = dict(row)
    record.pop("id", None)
    return record


def _normalize_active_writer(row: sqlite3.Row) -> dict:
    record = dict(row)
    record["scopes_json"] = json.loads(record["scopes_json"])
    record["parallel"] = bool(record["parallel"])
    return record


def _normalize_worktree(row: sqlite3.Row) -> dict:
    record = dict(row)
    if "baseline_json" in record and record["baseline_json"] is not None:
        record["baseline_json"] = json.loads(record["baseline_json"])
    return record


def _normalize_generic(row: sqlite3.Row) -> dict:
    return dict(row)


NORMALIZERS = {
    "tasks": _normalize_task,
    "rounds": _normalize_round,
    "events": _normalize_event,
    "active_writers": _normalize_active_writer,
    "worktrees": _normalize_worktree,
    "worktree_quarantine": _normalize_generic,
}

ROW_ORDER = {
    "tasks": lambda record: record["task_id"],
    "rounds": lambda record: (record["task_id"], record["round_number"]),
    "events": None,
    "active_writers": lambda record: record["task_id"],
    "worktrees": lambda record: record["task_id"],
    "worktree_quarantine": lambda record: record["entry_id"],
}


def _check_integrity(
    conn: sqlite3.Connection, name: str, spec: dict, reporter: Reporter
) -> None:
    for pragma in ("integrity_check", "quick_check"):
        rows = [tuple(row) for row in conn.execute(f"PRAGMA {pragma}")]
        if rows != [("ok",)]:
            reporter.error(name, f"{pragma} returned {rows}")
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    if version != spec["user_version"]:
        reporter.error(name, f"user_version={version}, expected {spec['user_version']}")
    conn.execute("PRAGMA foreign_keys=ON")
    rows = conn.execute("PRAGMA foreign_key_check").fetchall()
    if rows:
        reporter.error(name, f"foreign_key_check returned {len(rows)} row(s)")


def _check_tables(
    conn: sqlite3.Connection, name: str, schema: dict, reporter: Reporter
) -> None:
    expected_tables = schema["tables"]
    actual_tables = {
        row[0]
        for row in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"
        )
    }
    if actual_tables != set(expected_tables):
        reporter.error(
            name,
            f"tables {sorted(actual_tables)} != expected {sorted(expected_tables)}",
        )
        return
    for table, expected_columns in expected_tables.items():
        actual_rows = {
            row["name"]: row for row in conn.execute(f"PRAGMA table_info({table})")
        }
        actual = {
            column: {
                "type": row["type"],
                "notnull": bool(row["notnull"]),
                "pk": row["pk"],
            }
            for column, row in actual_rows.items()
        }
        expected = {
            column["name"]: {
                "type": column["type"],
                "notnull": bool(column["notnull"]),
                "pk": column["pk"],
            }
            for column in expected_columns
        }
        # Older expectation files omit defaults; v15 pins them exactly.
        if any("default" in column for column in expected_columns):
            for column in expected_columns:
                expected[column["name"]]["default"] = column.get("default")
            for column, row in actual_rows.items():
                actual[column]["default"] = row["dflt_value"]
        if actual != expected:
            reporter.error(name, f"table {table} columns differ: {actual} != {expected}")


def _check_indexes(
    conn: sqlite3.Connection, name: str, schema: dict, reporter: Reporter
) -> None:
    expected = schema["indexes"]
    actual_named = {
        row[0]
        for row in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL"
        )
    }
    if actual_named != set(expected):
        reporter.error(
            name, f"indexes {sorted(actual_named)} != expected {sorted(expected)}"
        )
    for index_name, index_spec in expected.items():
        listing = {
            row["name"]: row
            for row in conn.execute(f"PRAGMA index_list({index_spec['table']})")
        }
        if index_name not in listing:
            reporter.error(name, f"index {index_name} missing on {index_spec['table']}")
            continue
        row = listing[index_name]
        if bool(row["unique"]) != index_spec["unique"]:
            reporter.error(name, f"index {index_name} unique={bool(row['unique'])}")
        if bool(row["partial"]) != index_spec["partial"]:
            reporter.error(name, f"index {index_name} partial={bool(row['partial'])}")
        columns = [r["name"] for r in conn.execute(f"PRAGMA index_info({index_name})")]
        if columns != index_spec["columns"]:
            reporter.error(
                name, f"index {index_name} columns {columns} != {index_spec['columns']}"
            )


def _check_foreign_keys(
    conn: sqlite3.Connection, name: str, schema: dict, reporter: Reporter
) -> None:
    for foreign_key in schema["foreign_keys"]:
        rows = conn.execute(f"PRAGMA foreign_key_list({foreign_key['table']})").fetchall()
        found = {(row["table"], row["from"], row["to"]) for row in rows}
        key = (
            foreign_key["to_table"],
            foreign_key["from"],
            foreign_key["to_column"],
        )
        if key not in found:
            reporter.error(
                name, f"foreign key {key} not found in {foreign_key['table']}: {sorted(found)}"
            )


def _check_rows(
    conn: sqlite3.Connection, name: str, spec: dict, reporter: Reporter
) -> None:
    meta = {row["key"]: row["value"] for row in conn.execute("SELECT key, value FROM meta")}
    if meta != spec["meta"]:
        reporter.error(name, f"meta {meta} != {spec['meta']}")

    for table in ROW_TABLES:
        if table not in spec:
            continue
        normalizer = NORMALIZERS[table]
        if table == "events":
            actual = [
                normalizer(row) for row in conn.execute("SELECT * FROM events ORDER BY id")
            ]
        else:
            actual = [normalizer(row) for row in conn.execute(f"SELECT * FROM {table}")]
        order = ROW_ORDER[table]
        if order is not None:
            actual = sorted(actual, key=order)
            expected = sorted(spec[table], key=order)
        else:
            expected = list(spec[table])
        if actual != expected:
            reporter.error(
                name, f"{table} differ:\n  actual   {actual}\n  expected {expected}"
            )


def _check_invariants_v6(
    conn: sqlite3.Connection, name: str, reporter: Reporter
) -> None:
    placeholders = ",".join("?" for _ in ACTIVE_STATUSES)
    duplicates = conn.execute(
        f"SELECT project_id, COUNT(*) AS total FROM tasks WHERE status IN ({placeholders}) "
        "GROUP BY project_id HAVING total > 1",
        ACTIVE_STATUSES,
    ).fetchall()
    if duplicates:
        reporter.error(name, "multiple unfinished tasks for one project")

    for row in conn.execute("SELECT task_id, status FROM tasks"):
        if row["status"] not in (*ACTIVE_STATUSES, *TERMINAL_STATUSES):
            reporter.error(name, f"task {row['task_id']} has unknown status {row['status']!r}")
    for row in conn.execute(
        "SELECT task_id, round_number, status, verifier_state, verifier_json FROM rounds"
    ):
        if row["status"] not in ROUND_STATUSES:
            reporter.error(
                name,
                f"round {row['task_id']}/{row['round_number']} has unknown status {row['status']!r}",
            )
        if row["verifier_state"] not in VERIFIER_STATES:
            reporter.error(
                name,
                f"round {row['task_id']}/{row['round_number']} has unknown verifier_state "
                f"{row['verifier_state']!r}",
            )
        if row["verifier_state"] == "done" and not row["verifier_json"]:
            reporter.error(
                name,
                f"round {row['task_id']}/{row['round_number']} is verifier_state=done without verifier_json",
            )


def _check_invariants_v15(
    conn: sqlite3.Connection,
    name: str,
    reporter: Reporter,
    expected: dict,
) -> None:
    named = {
        row[0]
        for row in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL"
        )
    }
    for forbidden in expected.get("forbidden_indexes", ()):
        if forbidden in named:
            reporter.error(name, f"forbidden legacy index {forbidden} is present")
    for required in expected.get("required_indexes", ()):
        if required not in named:
            reporter.error(name, f"required v15 index {required} is missing")

    # The database must not constrain writer scope overlap: only the
    # application-level admission does.  There is no index over scopes_json and
    # the table DDL carries no CHECK constraint.
    for row in conn.execute(
        "SELECT name, sql FROM sqlite_master WHERE type='index' AND sql IS NOT NULL"
    ):
        if "scopes_json" in (row[1] or ""):
            reporter.error(name, f"index {row[0]} constrains scopes_json in SQL")
    ddl = conn.execute(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='active_writers'"
    ).fetchone()
    if ddl is None:
        reporter.error(name, "active_writers table is missing")
    elif "CHECK" in (ddl[0] or "").upper():
        reporter.error(name, "active_writers DDL carries a CHECK constraint")

    for row in conn.execute("SELECT task_id, status FROM tasks"):
        if row["status"] not in (*ACTIVE_STATUSES, *TERMINAL_STATUSES):
            reporter.error(name, f"task {row['task_id']} has unknown status {row['status']!r}")


def _check_sidecars(path: Path, name: str, reporter: Reporter) -> None:
    for suffix in ("-wal", "-shm"):
        if Path(f"{path}{suffix}").exists():
            reporter.error(name, f"sidecar {suffix} present; fixture is not checkpointed")


def verify_database(
    name: str,
    spec: dict,
    schema: dict,
    reporter: Reporter,
    expected: dict,
) -> None:
    path = HERE / name
    if not path.is_file():
        reporter.error(name, "fixture file is missing")
        return
    _check_sidecars(path, name, reporter)
    try:
        conn = sqlite3.connect(f"file:{path}?mode=ro&immutable=1", uri=True)
    except sqlite3.Error as exc:  # pragma: no cover - only on unreadable fixtures
        reporter.error(name, f"cannot open read-only: {exc}")
        return
    conn.row_factory = sqlite3.Row
    try:
        _check_integrity(conn, name, spec, reporter)
        _check_tables(conn, name, schema, reporter)
        _check_indexes(conn, name, schema, reporter)
        _check_foreign_keys(conn, name, schema, reporter)
        _check_rows(conn, name, spec, reporter)
        if expected.get("schema_version") == 15:
            _check_invariants_v15(conn, name, reporter, expected)
        else:
            _check_invariants_v6(conn, name, reporter)
    except (sqlite3.Error, ValueError) as exc:
        reporter.error(name, f"verification raised: {exc}")
    finally:
        conn.close()
    _check_sidecars(path, name, reporter)


def _load_expected(reporter: Reporter) -> list[dict]:
    loaded: list[dict] = []
    for path in EXPECTED_PATHS:
        if not path.is_file():
            reporter.error("<dir>", f"expectation file {path.name} is missing")
            continue
        try:
            expected = json.loads(path.read_text(encoding="utf-8"))
        except ValueError as exc:
            reporter.error("<dir>", f"{path.name} is not valid JSON: {exc}")
            continue
        if not isinstance(expected.get("databases"), dict) or not isinstance(
            expected.get("schema"), dict
        ):
            reporter.error("<dir>", f"{path.name} lacks 'databases' or 'schema'")
            continue
        expected["_path"] = path.name
        loaded.append(expected)
    return loaded


def main() -> int:
    reporter = Reporter()
    expected_sets = _load_expected(reporter)
    if not expected_sets:
        for error in reporter.errors:
            print(f"FAIL {error}", file=sys.stderr)
        print("no usable expectation manifest", file=sys.stderr)
        return 2

    all_databases: dict[str, tuple[dict, dict]] = {}
    for expected in expected_sets:
        for name, spec in expected["databases"].items():
            all_databases[name] = (spec, expected)

    present = {path.name for path in HERE.glob("*.sqlite")}
    if present != set(all_databases):
        reporter.error(
            "<dir>", f"fixture files {sorted(present)} != expected {sorted(all_databases)}"
        )

    fixture_paths = sorted(HERE.glob("*.sqlite"))
    before = {path.name: _sha256(path) for path in fixture_paths}

    for name, (spec, expected) in sorted(all_databases.items()):
        verify_database(name, spec, expected["schema"], reporter, expected)

    after = {path.name: _sha256(path) for path in sorted(HERE.glob("*.sqlite"))}
    if before != after:
        reporter.error("<dir>", "fixture bytes changed during verification")

    if reporter.errors:
        for error in reporter.errors:
            print(f"FAIL {error}", file=sys.stderr)
        print(f"{len(reporter.errors)} check(s) failed", file=sys.stderr)
        return 1
    print(
        f"ok: {len(all_databases)} fixture(s) verified read-only against "
        f"{', '.join(sorted(e['_path'] for e in expected_sets))}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
