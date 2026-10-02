#!/usr/bin/env python3
"""Deterministic generator for the SQLite contract fixtures.

Two fixture generations live next to this script:

* the four historical **schema v6** databases with ``expected.json`` (the
  legacy contract that must stay byte-for-byte stable), and
* the fresh, empty **schema v15** target ``empty-v15.sqlite`` with
  ``expected-v15.json`` (the current Python ``Storage`` schema at the pinned
  reference commit).

The generator is stdlib-only, reads no external state and is safe to re-run: it
recreates every database from scratch and rewrites the expectation files with
sorted keys.  The v15 DDL below is the exact fresh-schema ``initialize`` text of
the pinned Python source; ``parity/verify_parity.py`` compares it against the
real ``Storage.initialize`` and the real v6 -> v15 migration.

Usage::

    python3 docs/fixtures/sqlite/generate.py

The generated databases are checkpointed and closed; no ``-wal``/``-shm``
sidecars are left behind.  Regeneration is only needed when the fixture
contract itself changes; the committed ``.sqlite`` files are authoritative and
``verify.py`` validates them read-only.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

HERE = Path(__file__).resolve().parent

SCHEMA_VERSION = 6

# Exact schema emitted by ``Storage.initialize`` in the Python source at the
# recorded commit.  Kept verbatim so ``sqlite_master`` matches the runtime.
SCHEMA_SQL = """

                CREATE TABLE IF NOT EXISTS meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS tasks (
                    task_id TEXT PRIMARY KEY,
                    project_id TEXT NOT NULL,
                    workspace TEXT NOT NULL,
                    status TEXT NOT NULL,
                    session_id TEXT,
                    task TEXT NOT NULL,
                    allowed_paths TEXT NOT NULL,
                    test_commands TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    base_head TEXT,
                    snapshot TEXT,
                    revision_count INTEGER NOT NULL DEFAULT 0,
                    close_requested_at TEXT,
                    close_reason TEXT
                );
                CREATE UNIQUE INDEX IF NOT EXISTS ux_tasks_active
                    ON tasks(project_id) WHERE status IN
                    ('implementing','awaiting_review','revising','needs_user','failed','delivery_unknown');
                CREATE TABLE IF NOT EXISTS rounds (
                    task_id TEXT NOT NULL,
                    project_id TEXT NOT NULL,
                    round_number INTEGER NOT NULL,
                    request_id TEXT NOT NULL,
                    payload_hash TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    status TEXT NOT NULL,
                    outbound_message_id TEXT,
                    attempted INTEGER NOT NULL DEFAULT 0,
                    response_message_id TEXT,
                    response TEXT,
                    error_code TEXT,
                    result_json TEXT,
                    findings TEXT,
                    session_id TEXT,
                    worker_started_at TEXT,
                    worker_deadline_at TEXT,
                    verifier_state TEXT,
                    verifier_json TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY (task_id, round_number),
                    FOREIGN KEY (task_id) REFERENCES tasks(task_id)
                );
                CREATE UNIQUE INDEX IF NOT EXISTS ux_rounds_request
                    ON rounds(project_id, request_id);
                CREATE TABLE IF NOT EXISTS events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    task_id TEXT NOT NULL,
                    round_number INTEGER,
                    kind TEXT NOT NULL,
                    message TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS ix_events_task ON events(task_id, id);

"""

# --------------------------------------------------------------------------
# Schema v15 (increment B2): the exact fresh-schema DDL emitted by
# ``Storage.initialize`` in the pinned Python source.  The single-writer bound
# is enforced by the *partial* unique index ``ux_active_writers_single`` over
# ``active_writers(project_id) WHERE parallel=0``; there is deliberately no
# ``ux_tasks_active`` and no global ``ux_active_writers_project`` index, and the
# database itself never constrains scope overlap (that is application-level
# admission).  ``parity/verify_parity.py`` proves this against the real source.
# --------------------------------------------------------------------------

SCHEMA_VERSION_V15 = 15

V15_SCHEMA_SQL = """
                CREATE TABLE IF NOT EXISTS meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS tasks (
                    task_id TEXT PRIMARY KEY,
                    project_id TEXT NOT NULL,
                    workspace TEXT NOT NULL,
                    status TEXT NOT NULL,
                    session_id TEXT,
                    task TEXT NOT NULL,
                    allowed_paths TEXT NOT NULL,
                    test_commands TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    base_head TEXT,
                    snapshot TEXT,
                    revision_count INTEGER NOT NULL DEFAULT 0,
                    close_requested_at TEXT,
                    close_reason TEXT,
                    budget_json TEXT,
                    workflow_id TEXT,
                    depends_on TEXT,
                    execution_mode TEXT NOT NULL DEFAULT 'direct',
                    profile TEXT,
                    profile_json TEXT,
                    profile_hash TEXT,
                    profile_source TEXT
                );
                CREATE INDEX IF NOT EXISTS ix_tasks_project_status
                    ON tasks(project_id, status);
                CREATE TABLE IF NOT EXISTS active_writers (
                    task_id TEXT PRIMARY KEY,
                    project_id TEXT NOT NULL,
                    scopes_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    parallel INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS ix_active_writers_project
                    ON active_writers(project_id);
                CREATE UNIQUE INDEX IF NOT EXISTS ux_active_writers_single
                    ON active_writers(project_id) WHERE parallel=0;
                CREATE TABLE IF NOT EXISTS rounds (
                    task_id TEXT NOT NULL,
                    project_id TEXT NOT NULL,
                    round_number INTEGER NOT NULL,
                    request_id TEXT NOT NULL,
                    payload_hash TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    status TEXT NOT NULL,
                    outbound_message_id TEXT,
                    attempted INTEGER NOT NULL DEFAULT 0,
                    response_message_id TEXT,
                    response TEXT,
                    error_code TEXT,
                    result_json TEXT,
                    findings TEXT,
                    structured_findings TEXT,
                    session_id TEXT,
                    worker_started_at TEXT,
                    worker_deadline_at TEXT,
                    verifier_state TEXT,
                    verifier_json TEXT,
                    checkpoint_json TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY (task_id, round_number),
                    FOREIGN KEY (task_id) REFERENCES tasks(task_id)
                );
                CREATE UNIQUE INDEX IF NOT EXISTS ux_rounds_request
                    ON rounds(project_id, request_id);
                CREATE TABLE IF NOT EXISTS events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    task_id TEXT NOT NULL,
                    round_number INTEGER,
                    kind TEXT NOT NULL,
                    message TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS ix_events_task ON events(task_id, id);
                CREATE TABLE IF NOT EXISTS worktrees (
                    task_id TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    runtime_dir TEXT,
                    base_head TEXT,
                    baseline_json TEXT,
                    server_endpoint TEXT,
                    server_port INTEGER,
                    server_process_record TEXT,
                    status TEXT NOT NULL,
                    created_at TEXT,
                    updated_at TEXT,
                    removed_at TEXT,
                    cleanup_reason TEXT,
                    delivery_state TEXT,
                    delivery_journal_path TEXT,
                    delivered_at TEXT,
                    FOREIGN KEY (task_id) REFERENCES tasks(task_id)
                );
                CREATE TABLE IF NOT EXISTS worktree_quarantine (
                    entry_id TEXT PRIMARY KEY,
                    original_path TEXT NOT NULL,
                    quarantined_path TEXT,
                    reason TEXT,
                    found_at TEXT,
                    status TEXT NOT NULL
                );
"""

#: Named indexes that must exist in a schema v15 database, and the two legacy
#: indexes that must NOT exist any more.
V15_REQUIRED_INDEXES = (
    "ix_active_writers_project",
    "ix_events_task",
    "ix_tasks_project_status",
    "ux_active_writers_single",
    "ux_rounds_request",
)
V15_FORBIDDEN_INDEXES = ("ux_active_writers_project", "ux_tasks_active")

V15_INVARIANTS = [
    "PRAGMA user_version is 15 and meta.schema_version is '15'",
    "PRAGMA integrity_check and quick_check return ok; foreign_key_check returns no rows",
    "ux_active_writers_single is a partial unique index on active_writers(project_id) WHERE parallel=0",
    "ux_tasks_active and ux_active_writers_project do NOT exist (removed in v15)",
    "ix_tasks_project_status is a non-unique index on tasks(project_id, status)",
    "ix_active_writers_project is a non-unique index on active_writers(project_id)",
    "the database never constrains writer scope overlap: disjointness is application admission, not SQL",
    "every round references an existing task (rounds.task_id -> tasks.task_id)",
    "every worktree references an existing task (worktrees.task_id -> tasks.task_id)",
    "fixtures are closed and checkpointed: no -wal/-shm sidecar files next to any fixture",
]

# --------------------------------------------------------------------------
# Synthetic conventions (no real data, secrets or machine-specific paths).
# --------------------------------------------------------------------------

PROJECT_ID = "proj"
WORKSPACE = "/fixture/workspace"
TASK_TEXT = "Implement the fixture change"

HEAD = "1" * 40
INDEX_FINGERPRINT = "3" * 64
WORKTREE_FINGERPRINT = "4" * 64
BLOB_DIGEST = "5" * 64

T1 = "2026-01-01T00:00:00.000+00:00"
T2 = "2026-01-01T00:00:01.000+00:00"
T3 = "2026-01-01T00:00:02.000+00:00"
T4 = "2026-01-01T00:00:03.000+00:00"
T5 = "2026-01-01T00:00:04.000+00:00"
T6 = "2026-01-01T00:00:05.000+00:00"
DEADLINE = "2026-01-01T00:15:01.000+00:00"

SNAPSHOT = {
    "allow_commit": False,
    "allow_dirty": False,
    "dirty_paths": [],
    "external_repositories": [],
    "head": HEAD,
    "index_fingerprint": INDEX_FINGERPRINT,
    "manifest": {"module.py": BLOB_DIGEST},
    "status": "",
}

AWAITING_RESULT = {
    "baseline_dirty_paths": [],
    "changed_paths": ["module.py"],
    "committed_paths": [],
    "git_policy_violations": [],
    "head_after": HEAD,
    "head_before": HEAD,
    "model": {"model_id": "claude-sonnet-4-20250514", "provider_id": "anthropic"},
    "repositories": [
        {
            "baseline_dirty_paths": [],
            "changed_paths": ["module.py"],
            "committed_paths": [],
            "git_policy_violations": [],
            "head_after": HEAD,
            "head_before": HEAD,
            "root": WORKSPACE,
            "scope_violations": [],
        }
    ],
    "scope_violations": [],
    "task_changed_paths": ["module.py"],
    "usage": {
        "cache_read": 0,
        "cache_write": 0,
        "cost": 0,
        "input": 120,
        "output": 30,
        "reasoning": 0,
    },
}

AWAITING_VERIFIER = {
    "after": {
        "head": HEAD,
        "index_fingerprint": INDEX_FINGERPRINT,
        "worktree_fingerprint": WORKTREE_FINGERPRINT,
    },
    "before": {
        "head": HEAD,
        "index_fingerprint": INDEX_FINGERPRINT,
        "worktree_fingerprint": WORKTREE_FINGERPRINT,
    },
    "commands": [{"command": "pytest -q", "duration": 1.234, "exit_code": 0}],
    "log": "verification/task-1/round_1",
    "status": "passed",
}

TERMINAL_RESULT = {"changed_paths": [], "head_after": HEAD, "head_before": HEAD}


def task_row(
    *,
    task_id: str,
    status: str,
    session_id: str | None,
    created_at: str,
    updated_at: str,
    test_commands: list[str] | None = None,
) -> dict:
    return {
        "allowed_paths": ["module.py"],
        "base_head": HEAD,
        "close_reason": None,
        "close_requested_at": None,
        "created_at": created_at,
        "project_id": PROJECT_ID,
        "revision_count": 0,
        "session_id": session_id,
        "snapshot": SNAPSHOT,
        "status": status,
        "task": TASK_TEXT,
        "task_id": task_id,
        "test_commands": ["pytest -q"] if test_commands is None else test_commands,
        "updated_at": updated_at,
        "workspace": WORKSPACE,
    }


def round_row(
    *,
    task_id: str,
    round_number: int,
    request_id: str,
    kind: str,
    status: str,
    created_at: str,
    updated_at: str,
    outbound_message_id: str | None = None,
    attempted: bool = False,
    response_message_id: str | None = None,
    response: str | None = None,
    error_code: str | None = None,
    result_json: dict | None = None,
    findings: str | None = None,
    session_id: str | None = None,
    worker_started_at: str | None = None,
    worker_deadline_at: str | None = None,
    verifier_state: str | None = None,
    verifier_json: dict | None = None,
) -> dict:
    return {
        "attempted": attempted,
        "created_at": created_at,
        "error_code": error_code,
        "findings": findings,
        "kind": kind,
        "outbound_message_id": outbound_message_id,
        "payload_hash": f"hash-{request_id}",
        "project_id": PROJECT_ID,
        "request_id": request_id,
        "response": response,
        "response_message_id": response_message_id,
        "result_json": result_json,
        "round_number": round_number,
        "session_id": session_id,
        "status": status,
        "task_id": task_id,
        "updated_at": updated_at,
        "verifier_json": verifier_json,
        "verifier_state": verifier_state,
        "worker_deadline_at": worker_deadline_at,
        "worker_started_at": worker_started_at,
    }


def event_row(
    *, task_id: str, round_number: int | None, kind: str, message: str, created_at: str
) -> dict:
    return {
        "created_at": created_at,
        "kind": kind,
        "message": message,
        "round_number": round_number,
        "task_id": task_id,
    }


DATABASES = {
    "empty-v6.sqlite": {
        "meta": {"schema_version": str(SCHEMA_VERSION)},
        "rounds": [],
        "tasks": [],
        "events": [],
        "user_version": SCHEMA_VERSION,
    },
    "active-v6.sqlite": {
        "meta": {"schema_version": str(SCHEMA_VERSION)},
        "tasks": [
            task_row(
                task_id="task-1",
                status="implementing",
                session_id="ses-1",
                created_at=T1,
                updated_at=T2,
            )
        ],
        "rounds": [
            round_row(
                task_id="task-1",
                round_number=1,
                request_id="req-1",
                kind="implement",
                status="observing",
                outbound_message_id="msg-1",
                attempted=True,
                session_id="ses-1",
                worker_started_at=T2,
                worker_deadline_at=DEADLINE,
                created_at=T1,
                updated_at=T2,
            )
        ],
        "events": [
            event_row(
                task_id="task-1",
                round_number=1,
                kind="created",
                message="task created (implement)",
                created_at=T1,
            )
        ],
        "user_version": SCHEMA_VERSION,
    },
    "awaiting-review-v6.sqlite": {
        "meta": {"schema_version": str(SCHEMA_VERSION)},
        "tasks": [
            task_row(
                task_id="task-1",
                status="awaiting_review",
                session_id="ses-1",
                created_at=T1,
                updated_at=T4,
            )
        ],
        "rounds": [
            round_row(
                task_id="task-1",
                round_number=1,
                request_id="req-1",
                kind="implement",
                status="complete",
                outbound_message_id="msg-1",
                attempted=True,
                response_message_id="msg-2",
                response="Implemented the change.",
                result_json=AWAITING_RESULT,
                session_id="ses-1",
                worker_started_at=T2,
                worker_deadline_at=DEADLINE,
                verifier_state="done",
                verifier_json=AWAITING_VERIFIER,
                created_at=T1,
                updated_at=T4,
            )
        ],
        "events": [
            event_row(
                task_id="task-1",
                round_number=1,
                kind="created",
                message="task created (implement)",
                created_at=T1,
            ),
            event_row(
                task_id="task-1",
                round_number=1,
                kind="complete",
                message="round finished: complete",
                created_at=T4,
            ),
        ],
        "user_version": SCHEMA_VERSION,
    },
    "terminal-v6.sqlite": {
        "meta": {"schema_version": str(SCHEMA_VERSION)},
        "tasks": [
            task_row(
                task_id="task-1",
                status="accepted",
                session_id="ses-1",
                created_at=T1,
                updated_at=T4,
                test_commands=[],
            ),
            task_row(
                task_id="task-2",
                status="closed",
                session_id="ses-2",
                created_at=T3,
                updated_at=T6,
                test_commands=[],
            ),
        ],
        "rounds": [
            round_row(
                task_id="task-1",
                round_number=1,
                request_id="req-1",
                kind="implement",
                status="complete",
                outbound_message_id="msg-1",
                attempted=True,
                response_message_id="msg-2",
                response="Implemented the change.",
                result_json=TERMINAL_RESULT,
                session_id="ses-1",
                created_at=T1,
                updated_at=T3,
            ),
            round_row(
                task_id="task-2",
                round_number=1,
                request_id="req-2",
                kind="implement",
                status="complete",
                outbound_message_id="msg-3",
                attempted=True,
                response_message_id="msg-4",
                response="Implemented the second change.",
                result_json=TERMINAL_RESULT,
                session_id="ses-2",
                created_at=T3,
                updated_at=T5,
            ),
        ],
        "events": [
            event_row(
                task_id="task-1",
                round_number=1,
                kind="created",
                message="task created (implement)",
                created_at=T1,
            ),
            event_row(
                task_id="task-1",
                round_number=1,
                kind="complete",
                message="round finished: complete",
                created_at=T3,
            ),
            event_row(
                task_id="task-1",
                round_number=None,
                kind="accepted",
                message="task accepted by Codex",
                created_at=T4,
            ),
            event_row(
                task_id="task-2",
                round_number=1,
                kind="created",
                message="task created (implement)",
                created_at=T3,
            ),
            event_row(
                task_id="task-2",
                round_number=1,
                kind="complete",
                message="round finished: complete",
                created_at=T5,
            ),
            event_row(
                task_id="task-2",
                round_number=None,
                kind="closed",
                message="task closed: fixture cleanup",
                created_at=T6,
            ),
        ],
        "user_version": SCHEMA_VERSION,
    },
}

SCHEMA_EXPECTATIONS = {
    "tables": {
        "events": [
            {"name": "id", "notnull": False, "pk": 1, "type": "INTEGER"},
            {"name": "task_id", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "round_number", "notnull": False, "pk": 0, "type": "INTEGER"},
            {"name": "kind", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "message", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "created_at", "notnull": True, "pk": 0, "type": "TEXT"},
        ],
        "meta": [
            {"name": "key", "notnull": False, "pk": 1, "type": "TEXT"},
            {"name": "value", "notnull": True, "pk": 0, "type": "TEXT"},
        ],
        "rounds": [
            {"name": "task_id", "notnull": True, "pk": 1, "type": "TEXT"},
            {"name": "project_id", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "round_number", "notnull": True, "pk": 2, "type": "INTEGER"},
            {"name": "request_id", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "payload_hash", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "kind", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "status", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "outbound_message_id", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "attempted", "notnull": True, "pk": 0, "type": "INTEGER"},
            {"name": "response_message_id", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "response", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "error_code", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "result_json", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "findings", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "session_id", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "worker_started_at", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "worker_deadline_at", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "verifier_state", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "verifier_json", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "created_at", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "updated_at", "notnull": True, "pk": 0, "type": "TEXT"},
        ],
        "tasks": [
            {"name": "task_id", "notnull": False, "pk": 1, "type": "TEXT"},
            {"name": "project_id", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "workspace", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "status", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "session_id", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "task", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "allowed_paths", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "test_commands", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "created_at", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "updated_at", "notnull": True, "pk": 0, "type": "TEXT"},
            {"name": "base_head", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "snapshot", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "revision_count", "notnull": True, "pk": 0, "type": "INTEGER"},
            {"name": "close_requested_at", "notnull": False, "pk": 0, "type": "TEXT"},
            {"name": "close_reason", "notnull": False, "pk": 0, "type": "TEXT"},
        ],
    },
    "indexes": {
        "ix_events_task": {
            "columns": ["task_id", "id"],
            "partial": False,
            "table": "events",
            "unique": False,
        },
        "ux_rounds_request": {
            "columns": ["project_id", "request_id"],
            "partial": False,
            "table": "rounds",
            "unique": True,
        },
        "ux_tasks_active": {
            "columns": ["project_id"],
            "partial": True,
            "table": "tasks",
            "unique": True,
        },
    },
    "foreign_keys": [
        {
            "from": "task_id",
            "table": "rounds",
            "to_column": "task_id",
            "to_table": "tasks",
        }
    ],
}

INVARIANTS = [
    "PRAGMA user_version is 6 and meta.schema_version is '6'",
    "PRAGMA integrity_check and quick_check return ok; foreign_key_check returns no rows",
    "at most one unfinished task per project (ux_tasks_active partial unique index)",
    "every round references an existing task (rounds.task_id -> tasks.task_id)",
    "rounds(project_id, request_id) is unique for request idempotency (ux_rounds_request)",
    "task.allowed_paths and task.test_commands are JSON arrays; task.snapshot, round.result_json and round.verifier_json are JSON objects or null",
    "verifier_state is null, 'running' or 'done'; a 'done' round carries verifier_json",
    "fixtures are closed and checkpointed: no -wal/-shm sidecar files next to any fixture",
]

SOURCE = {
    "commit": "64b2a8370d5e86fdf2f660fd0b71fc912371dee2",
    "modules": [
        "src/agent_bridge/mcp_server.py",
        "src/agent_bridge/storage.py",
        "src/agent_bridge/verifier.py",
        "src/agent_bridge/worker.py",
    ],
    "python_repo": "agent_bridge",
    "tests": ["tests/test_mcp.py", "tests/test_storage.py"],
}

SYNTHETIC_CONVENTIONS = {
    "fingerprints": "64-character lowercase hex sha256 digests (index/worktree/blob)",
    "heads": "40-character lowercase hex synthetic Git object ids",
    "message_ids": "msg-<n>",
    "payload_hash": "hash-<request_id>",
    "project_id": PROJECT_ID,
    "request_ids": "req-<n>",
    "session_ids": "ses-<n>",
    "task_ids": "task-<n>",
    "timestamps": "fixed UTC ISO-8601 with millisecond precision (2026-01-01T00:00:0n.000+00:00)",
    "workspace": WORKSPACE,
}


def build_expected() -> dict:
    return {
        "databases": DATABASES,
        "fixture_version": 1,
        "invariants": INVARIANTS,
        "schema": SCHEMA_EXPECTATIONS,
        "schema_version": SCHEMA_VERSION,
        "source": SOURCE,
        "synthetic_conventions": SYNTHETIC_CONVENTIONS,
    }


# --------------------------------------------------------------------------
# Schema v15 fixtures (fresh empty target).
# --------------------------------------------------------------------------

V15_DATABASES = {
    "empty-v15.sqlite": {
        "meta": {"schema_version": str(SCHEMA_VERSION_V15)},
        "tasks": [],
        "rounds": [],
        "events": [],
        "active_writers": [],
        "worktrees": [],
        "worktree_quarantine": [],
        "user_version": SCHEMA_VERSION_V15,
    },
}

V15_SOURCE = {
    "commit": "86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a",
    "modules": [
        "src/agent_bridge/config.py",
        "src/agent_bridge/storage.py",
        "src/agent_bridge/usage.py",
    ],
    "python_repo": "agent_bridge",
    "tests": ["tests/test_storage.py"],
}


def _read_schema(path: Path) -> dict:
    """Derive the structural schema of a database read-only.

    The returned shape matches ``expected.json`` so ``verify.py`` can use the
    same checks for both generations.
    """
    conn = sqlite3.connect(f"file:{path}?mode=ro&immutable=1", uri=True)
    conn.row_factory = sqlite3.Row
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
                    "default": row["dflt_value"],
                    "name": row["name"],
                    "notnull": bool(row["notnull"]),
                    "pk": row["pk"],
                    "type": row["type"],
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
                "columns": [
                    item["name"]
                    for item in conn.execute(f"PRAGMA index_info({row['name']})")
                ],
                "partial": bool(info["partial"]),
                "table": row["tbl_name"],
                "unique": bool(info["unique"]),
            }
        foreign_keys = []
        for table in table_names:
            for row in conn.execute(f"PRAGMA foreign_key_list({table})"):
                foreign_keys.append(
                    {
                        "from": row["from"],
                        "table": table,
                        "to_column": row["to"],
                        "to_table": row["table"],
                    }
                )
        foreign_keys.sort(
            key=lambda fk: (fk["table"], fk["from"], fk["to_table"], fk["to_column"])
        )
        return {"tables": tables, "indexes": indexes, "foreign_keys": foreign_keys}
    finally:
        conn.close()


def build_expected_v15() -> dict:
    schema = _read_schema(HERE / "empty-v15.sqlite")
    return {
        "databases": V15_DATABASES,
        "fixture_version": 1,
        "forbidden_indexes": list(V15_FORBIDDEN_INDEXES),
        "invariants": V15_INVARIANTS,
        "required_indexes": list(V15_REQUIRED_INDEXES),
        "schema": schema,
        "schema_version": SCHEMA_VERSION_V15,
        "source": V15_SOURCE,
        "synthetic_conventions": SYNTHETIC_CONVENTIONS,
    }


def _encode(value: object) -> object:
    if isinstance(value, (dict, list)):
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
    return value


def _insert(conn: sqlite3.Connection, table: str, rows: list[dict]) -> None:
    if not rows:
        return
    columns = sorted(rows[0])
    placeholders = ",".join("?" for _ in columns)
    sql = f"INSERT INTO {table} ({','.join(columns)}) VALUES ({placeholders})"
    for row in rows:
        conn.execute(sql, [_encode(row[column]) for column in columns])


def create_database(name: str, data: dict) -> None:
    path = HERE / name
    for sidecar in (path, Path(f"{path}-wal"), Path(f"{path}-shm")):
        if sidecar.exists():
            sidecar.unlink()
    conn = sqlite3.connect(path, isolation_level=None)
    try:
        conn.execute("PRAGMA journal_mode=WAL")
        conn.executescript(SCHEMA_SQL)
        conn.execute(f"PRAGMA user_version={SCHEMA_VERSION}")
        for key, value in sorted(data["meta"].items()):
            conn.execute("INSERT INTO meta(key, value) VALUES (?, ?)", (key, value))
        _insert(conn, "tasks", data["tasks"])
        _insert(conn, "rounds", data["rounds"])
        _insert(conn, "events", data["events"])
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    finally:
        conn.close()
    for sidecar in (Path(f"{path}-wal"), Path(f"{path}-shm")):
        if sidecar.exists():
            sidecar.unlink()


def create_v15_database(name: str, data: dict) -> None:
    """Create one fresh schema v15 database with the exact source DDL."""
    path = HERE / name
    for sidecar in (path, Path(f"{path}-wal"), Path(f"{path}-shm")):
        if sidecar.exists():
            sidecar.unlink()
    conn = sqlite3.connect(path, isolation_level=None)
    try:
        conn.executescript(V15_SCHEMA_SQL)
        conn.execute(f"PRAGMA user_version={SCHEMA_VERSION_V15}")
        for key, value in sorted(data["meta"].items()):
            conn.execute("INSERT INTO meta(key, value) VALUES (?, ?)", (key, value))
        for table in (
            "tasks",
            "rounds",
            "events",
            "active_writers",
            "worktrees",
            "worktree_quarantine",
        ):
            _insert(conn, table, data.get(table, []))
    finally:
        conn.close()
    for sidecar in (Path(f"{path}-wal"), Path(f"{path}-shm")):
        if sidecar.exists():
            sidecar.unlink()


def write_expected(expected: dict) -> None:
    text = json.dumps(expected, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    (HERE / "expected.json").write_text(text, encoding="utf-8")


def write_expected_v15(expected: dict) -> None:
    text = json.dumps(expected, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    (HERE / "expected-v15.json").write_text(text, encoding="utf-8")


def main() -> int:
    for name, data in DATABASES.items():
        create_database(name, data)
    write_expected(build_expected())
    for name, data in V15_DATABASES.items():
        create_v15_database(name, data)
    write_expected_v15(build_expected_v15())
    print(
        f"generated {len(DATABASES)} legacy v6 databases + expected.json and "
        f"{len(V15_DATABASES)} v15 database(s) + expected-v15.json in {HERE}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
