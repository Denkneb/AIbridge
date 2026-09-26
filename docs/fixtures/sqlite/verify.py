#!/usr/bin/env python3
"""Read-only verifier for the schema v6 SQLite contract fixtures.

Validates the four fixture databases against ``expected.json`` without ever
writing to them: every database is opened with ``mode=ro&immutable=1`` and the
verifier refuses to accept a fixture that has a ``-wal``/``-shm`` sidecar.  The
verifier uses only the Python standard library (``sqlite3``/``json``).

Usage::

    python3 docs/fixtures/sqlite/verify.py

Exit code 0 means every fixture matched; 1 means at least one check failed and
2 means the fixture manifest itself is unusable.
"""

from __future__ import annotations

import json
import sqlite3
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
EXPECTED_PATH = HERE / "expected.json"

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


class Reporter:
    def __init__(self) -> None:
        self.errors: list[str] = []

    def error(self, database: str, message: str) -> None:
        self.errors.append(f"{database}: {message}")


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


def _check_integrity(conn: sqlite3.Connection, name: str, spec: dict, reporter: Reporter) -> None:
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


def _check_tables(conn: sqlite3.Connection, name: str, schema: dict, reporter: Reporter) -> None:
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
        actual = {
            row["name"]: {
                "type": row["type"],
                "notnull": bool(row["notnull"]),
                "pk": row["pk"],
            }
            for row in conn.execute(f"PRAGMA table_info({table})")
        }
        expected = {
            column["name"]: {
                "type": column["type"],
                "notnull": bool(column["notnull"]),
                "pk": column["pk"],
            }
            for column in expected_columns
        }
        if actual != expected:
            reporter.error(name, f"table {table} columns differ: {actual} != {expected}")


def _check_indexes(conn: sqlite3.Connection, name: str, schema: dict, reporter: Reporter) -> None:
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


def _check_foreign_keys(conn: sqlite3.Connection, name: str, schema: dict, reporter: Reporter) -> None:
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


def _check_rows(conn: sqlite3.Connection, name: str, spec: dict, reporter: Reporter) -> None:
    meta = {row["key"]: row["value"] for row in conn.execute("SELECT key, value FROM meta")}
    if meta != spec["meta"]:
        reporter.error(name, f"meta {meta} != {spec['meta']}")

    tasks = sorted(
        (_normalize_task(row) for row in conn.execute("SELECT * FROM tasks")),
        key=lambda record: record["task_id"],
    )
    expected_tasks = sorted(spec["tasks"], key=lambda record: record["task_id"])
    if tasks != expected_tasks:
        reporter.error(name, f"tasks differ:\n  actual   {tasks}\n  expected {expected_tasks}")

    rounds = sorted(
        (_normalize_round(row) for row in conn.execute("SELECT * FROM rounds")),
        key=lambda record: (record["task_id"], record["round_number"]),
    )
    expected_rounds = sorted(
        spec["rounds"], key=lambda record: (record["task_id"], record["round_number"])
    )
    if rounds != expected_rounds:
        reporter.error(name, f"rounds differ:\n  actual   {rounds}\n  expected {expected_rounds}")

    events = [_normalize_event(row) for row in conn.execute("SELECT * FROM events ORDER BY id")]
    if events != spec["events"]:
        reporter.error(name, f"events differ:\n  actual   {events}\n  expected {spec['events']}")


def _check_invariants(conn: sqlite3.Connection, name: str, reporter: Reporter) -> None:
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


def _check_sidecars(path: Path, name: str, reporter: Reporter) -> None:
    for suffix in ("-wal", "-shm"):
        if Path(f"{path}{suffix}").exists():
            reporter.error(name, f"sidecar {suffix} present; fixture is not checkpointed")


def verify_database(name: str, spec: dict, schema: dict, reporter: Reporter) -> None:
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
        _check_invariants(conn, name, reporter)
    except (sqlite3.Error, ValueError) as exc:
        reporter.error(name, f"verification raised: {exc}")
    finally:
        conn.close()
    _check_sidecars(path, name, reporter)


def main() -> int:
    if not EXPECTED_PATH.is_file():
        print(f"expected.json not found at {EXPECTED_PATH}", file=sys.stderr)
        return 2
    try:
        expected = json.loads(EXPECTED_PATH.read_text(encoding="utf-8"))
    except ValueError as exc:
        print(f"expected.json is not valid JSON: {exc}", file=sys.stderr)
        return 2

    databases = expected.get("databases")
    schema = expected.get("schema")
    if not isinstance(databases, dict) or not isinstance(schema, dict):
        print("expected.json lacks 'databases' or 'schema'", file=sys.stderr)
        return 2

    reporter = Reporter()
    present = {path.name for path in HERE.glob("*.sqlite")}
    if present != set(databases):
        reporter.error(
            "<dir>", f"fixture files {sorted(present)} != expected {sorted(databases)}"
        )
    for name, spec in sorted(databases.items()):
        verify_database(name, spec, schema, reporter)

    if reporter.errors:
        for error in reporter.errors:
            print(f"FAIL {error}", file=sys.stderr)
        print(f"{len(reporter.errors)} check(s) failed", file=sys.stderr)
        return 1
    print(f"ok: {len(databases)} fixture(s) verified against expected.json")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
