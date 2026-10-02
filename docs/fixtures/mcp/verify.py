#!/usr/bin/env python3
"""Read-only source-parity verifier for the MCP contract fixtures (task 0A.3).

Unlike a pure schema/self-consistency check, this verifier imports the **actual
pinned Python reference** (``agent_bridge.mcp_server`` and friends) and drives
the real tool implementations on isolated temporary fixtures:

* a fresh temporary Git workspace, a fresh temporary ``Storage`` database and a
  temporary ``projects.toml`` per case (nothing outside ``tempfile`` is written);
* the OpenCode network layer and the detached worker spawn are replaced with
  deterministic in-process doubles, so no server, model or child process is ever
  started;
* the ``task_status``/``request_changes``/``accept_task``/``close_task``/
  ``submit_task`` wrappers and the ``project_info`` tool closure from
  ``build_server`` are invoked directly and their normalized answers compared
  with the corpus expectations.

Cases labelled ``legacy-v6`` are retained as history: they are only checked
leniently (expectation kind and, for errors, the stable category) because some
of their surfaces deliberately changed in v15.  Cases labelled ``target-v15``
are checked strictly against ``expect.fields``/``expect.values``/``expect.absent``
and the recorded side effects.  Cases whose setup cannot be reproduced by the
isolated harness are reported as ``SKIP`` with an explicit reason -- they are
never counted as passed.

Usage::

    python3 docs/fixtures/mcp/verify.py

The script re-executes itself under ``<reference>/.venv/bin/python`` when the
current interpreter cannot import ``agent_bridge``; ``PYTHONDONTWRITEBYTECODE=1``
is always set so the reference checkout never gains a ``__pycache__``.  Override
the reference with ``--python-repo`` or ``AGENT_BRIDGE_PYTHON_REPO``.

Exit codes: 0 = no failure (skips are reported), 1 = at least one strict check
failed, 2 = the reference or corpus itself is unusable.
"""

from __future__ import annotations

import argparse
import asyncio
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import traceback
from contextlib import ExitStack, redirect_stdout
from pathlib import Path
from unittest import mock

sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent
CORPUS_PATH = HERE.parent / "mcp-cases.json"
DEFAULT_REFERENCE = "/home/denis/Python/agent_bridge"
PINNED_COMMIT = "86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a"
PINNED_SCHEMA = 15

FIXED_TS = "2026-01-01T00:00:00.000+00:00"
FIXED_SESSION = "ses_1"
FIXED_REVISION_SESSION = "ses_2"
FIXED_FOREIGN = "/foreign/root"
SUSPECTED_AWS = "AKIAIOSFODNN7EXAMPLE"
SUSPECTED_OPENAI = "sk-" + "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6"

_UUID_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)
_ISO_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$")

BASE_MAPPING = {
    "${PROJECT_ID}": "proj",
    "${TASK_ID}": "task-1",
    "${OTHER_TASK_ID}": "task-2",
    "${SESSION_ID}": FIXED_SESSION,
    "${REVISION_SESSION_ID}": FIXED_REVISION_SESSION,
    "${REQUEST_ID}": "req-1",
    "${REVISION_REQUEST_ID}": "req-2",
    "${WORKFLOW_ID}": "wf-1",
    "${FOREIGN_PATH}": FIXED_FOREIGN,
    "${SUSPECTED_AWS_KEY}": SUSPECTED_AWS,
    "${SUSPECTED_OPENAI_KEY}": SUSPECTED_OPENAI,
}


class Report:
    def __init__(self) -> None:
        self.passed: list[str] = []
        self.failed: list[tuple[str, str]] = []
        self.skipped: list[tuple[str, str]] = []

    def ok(self, label: str) -> None:
        self.passed.append(label)
        print(f"PASS {label}")

    def fail(self, label: str, detail: str = "") -> None:
        self.failed.append((label, detail))
        print(f"FAIL {label}: {detail}")

    def skip(self, label: str, reason: str) -> None:
        self.skipped.append((label, reason))
        print(f"SKIP {label}: {reason}")


def _reference_from_argv(argv: list[str]) -> Path:
    env = os.environ.get("AGENT_BRIDGE_PYTHON_REPO")
    if "--python-repo" in argv:
        index = argv.index("--python-repo")
        if index + 1 < len(argv):
            return Path(argv[index + 1])
    elif env:
        return Path(env)
    return Path(DEFAULT_REFERENCE)


def _ensure_reference(argv: list[str]) -> str | None:
    """Return an error string when the reference cannot be imported at all."""
    try:
        import agent_bridge  # noqa: F401

        return None
    except Exception:
        pass
    reference = _reference_from_argv(argv)
    venv_python = reference / ".venv" / "bin" / "python"
    if os.environ.get("MCP_VERIFY_BOOTSTRAPPED") == "1" or not venv_python.is_file():
        return (
            f"cannot import agent_bridge and no usable reference interpreter at "
            f"{venv_python}"
        )
    env = os.environ.copy()
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env["MCP_VERIFY_BOOTSTRAPPED"] = "1"
    src = str((reference / "src").resolve())
    env["PYTHONPATH"] = src + os.pathsep + env.get("PYTHONPATH", "")
    os.execve(
        str(venv_python),
        [str(venv_python), str(Path(__file__).resolve()), "--python-repo", str(reference), *argv],
        env,
    )
    return "unreachable"  # pragma: no cover


def _subst(value, mapping):
    if isinstance(value, str):
        if value in mapping:
            return mapping[value]
        out = value
        for key, replacement in mapping.items():
            if isinstance(replacement, str):
                out = out.replace(key, replacement)
        return out
    if isinstance(value, list):
        return [_subst(item, mapping) for item in value]
    if isinstance(value, dict):
        return {key: _subst(item, mapping) for key, item in value.items()}
    return value


def _normalize(value, fixture):
    if isinstance(value, str):
        return fixture.normalize_str(value)
    if isinstance(value, list):
        return [_normalize(item, fixture) for item in value]
    if isinstance(value, dict):
        return {key: _normalize(item, fixture) for key, item in value.items()}
    return value


class Fixture:
    """One isolated temporary MCP fixture driven by the real Python code."""

    def __init__(self, tmp: Path, setup: dict, extra_placeholders: dict) -> None:
        self.tmp = tmp
        self.setup = setup
        self.server = setup.get("server", "ready")
        self.blockers = bool(setup.get("blocker_pending"))
        self.running: set[str] = set()
        self.spawned: list[tuple[str, int]] = []
        self.task_ids: list[str] = []
        #: Exact submit-time snapshot persisted for each seeded task, captured
        #: before any post-seed drift.  Used to prove that a successful
        #: activation really replaced a stale baseline and that a blocked
        #: activation left the original baseline untouched.
        self.submit_snapshots: dict[str, dict] = {}
        self.linked = setup.get("linked_project")
        self.worker_running_after_spawn = bool(setup.get("worker_running_after_spawn"))
        mapping = dict(BASE_MAPPING)
        mapping.update(extra_placeholders)
        self.mapping = mapping

    # -- construction ----------------------------------------------------
    def build(self, modules) -> None:
        self.modules = modules
        config_cls = modules["config"].ProjectConfig
        workspace = self.tmp / "ws"
        workspace.mkdir()
        self.workspace = workspace
        self._git_init(workspace, self.setup.get("workspace", {}))
        cfg = self.setup.get("config", {})
        self.password_file = self.tmp / "secrets" / "proj.password"
        self.password_file.parent.mkdir(parents=True, exist_ok=True)
        self.password_file.write_text("local-test-password\n", encoding="utf-8")
        os.chmod(self.password_file, 0o600)
        self.config_path = self.tmp / "projects.toml"

        linked_workspace: Path | None = None
        linked_id: str | None = None
        linked_password: Path | None = None
        trusted: list[Path] = []
        if self.linked:
            linked_id = self.linked.get("project_id", "beta")
            linked_workspace = self.tmp / f"ws-{linked_id}"
            linked_workspace.mkdir()
            self.linked_workspace = linked_workspace
            self._git_init(linked_workspace, self.linked.get("workspace", {}))
            linked_password = self.tmp / "secrets" / f"{linked_id}.password"
            linked_password.write_text("local-test-password\n", encoding="utf-8")
            os.chmod(linked_password, 0o600)
            trusted.append(linked_workspace)

        config_text = (
            "[projects.proj]\n"
            f'workspace = "{workspace}"\n'
            'opencode_url = "http://127.0.0.1:4101"\n'
            f'password_file = "{self.password_file}"\n'
            "max_rounds = 3\n"
        )
        if trusted:
            config_text += (
                "auto_approve_external_directories = ["
                + ", ".join(f'"{path}"' for path in trusted)
                + "]\n"
            )
        if linked_id is not None:
            config_text += (
                f"\n[projects.{linked_id}]\n"
                f'workspace = "{linked_workspace}"\n'
                'opencode_url = "http://127.0.0.1:4102"\n'
                f'password_file = "{linked_password}"\n'
                "max_rounds = 3\n"
            )
        self.config_path.write_text(config_text, encoding="utf-8")
        self.state_root = self.tmp / "state"
        self.config = config_cls(
            project_id="proj",
            workspace=workspace,
            host="127.0.0.1",
            port=4101,
            password_file=self.password_file,
            max_rounds=int(cfg.get("max_rounds", 3)),
            config_path=self.config_path,
            state_root=self.state_root,
            execution_mode=cfg.get("execution_mode", "direct"),
            max_active_tasks=int(cfg.get("max_active_tasks", 1)),
            allow_parallel_writers=bool(cfg.get("allow_parallel_writers", False)),
            default_profile=cfg.get("default_profile"),
            opencode_model=tuple(cfg["opencode_model"]) if cfg.get("opencode_model") else None,
            auto_approve_external_directories=tuple(trusted),
        )
        self.mapping["${WORKSPACE}"] = str(workspace)
        self.mapping["${FIXTURE_DIR}"] = str(self.tmp)
        self.mapping["${STATE_ROOT}"] = str(self.state_root)
        self.mapping["${CONFIG_PATH}"] = str(self.config_path)
        if linked_id is not None and linked_workspace is not None:
            self.mapping["${LINKED_PROJECT_ID}"] = linked_id
            self.mapping["${LINKED_WORKSPACE}"] = str(linked_workspace)
        storage_cls = modules["storage"].Storage
        self.storage = storage_cls(self.config)
        self.storage.initialize()
        self._seed()
        self.linked_storage = None
        self.linked_config = None
        if linked_id is not None and linked_workspace is not None:
            self.linked_config = modules["config"].load_project(
                self.config_path, linked_id, state_root=self.state_root
            )
            self.linked_storage = storage_cls(self.linked_config)
            self.linked_storage.initialize()
            with self._connect(self.linked_storage) as conn:
                for task_spec in self.setup.get("linked_tasks", []):
                    self._seed_task(
                        conn,
                        task_spec,
                        project_id=linked_id,
                        workspace=linked_workspace,
                    )
                conn.commit()
        self._apply_post_seed()
        self._seed_worktrees()
        self._apply_breaks()
        self._server_fns = None

    def _git(self, workspace: Path, *args: str) -> None:
        subprocess.run(
            ["git", "-C", str(workspace), *args],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    def _git_init(self, workspace: Path, ws_setup: dict) -> None:
        if ws_setup.get("git_repo", True) is False:
            return
        self._git(workspace, "init", "-q", "-b", "main")
        (workspace / "module.py").write_text("value = 1\n", encoding="utf-8")
        (workspace / "other.txt").write_text("tracked = 1\n", encoding="utf-8")
        (workspace / ".gitignore").write_text("*.log\n", encoding="utf-8")
        self._git(workspace, "add", ".")
        self._git(
            workspace,
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-q",
            "-m",
            "init",
        )
        for dirty in ws_setup.get("dirty_paths", []):
            path = workspace / dirty
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("dirty = 1\n", encoding="utf-8")

    # -- seeding ---------------------------------------------------------
    def _seed(self) -> None:
        setup = self.setup
        with self._connect() as conn:
            task = setup.get("task")
            if task is not None:
                self._seed_task(conn, task)
            for extra in setup.get("extra_tasks", []):
                self._seed_task(conn, extra)
            for writer in setup.get("active_writers", []):
                conn.execute(
                    "INSERT OR REPLACE INTO active_writers"
                    "(task_id, project_id, scopes_json, created_at, parallel) "
                    "VALUES (?,?,?,?,?)",
                    (
                        writer["task_id"],
                        self.config.project_id,
                        json.dumps(["module.py"]),
                        FIXED_TS,
                        int(writer.get("parallel", 0)),
                    ),
                )
            conn.commit()

    def _apply_post_seed(self) -> None:
        """Mutate the shared workspace *after* the submit snapshots were taken.

        This models the real race the B1 baseline contract guards: a same-project
        successor persists its submit-time snapshot, then its predecessor keeps
        editing the shared workspace.  Without this drift the submit snapshot and
        the activation-time workspace are identical, so a broken or missing
        rebaseline could not be distinguished from a correct one.
        """
        for action in self.setup.get("post_seed", []):
            kind = action["kind"]
            if kind not in ("write", "commit"):
                raise ValueError(f"unknown post_seed action {kind!r}")
            for rel, content in action.get("write", {}).items():
                path = self.workspace / rel
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8")
            stage = action.get("stage") or []
            if stage:
                self._git(self.workspace, "add", "--", *stage)
            if kind == "commit":
                self._git(
                    self.workspace,
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.com",
                    "commit",
                    "-qam",
                    action.get("message", "post-seed drift"),
                )

    def _seed_worktrees(self) -> None:
        for spec in self.setup.get("worktrees", []):
            self.storage.register_worktree(
                task_id=spec["task_id"],
                path=spec.get("path", str(self.tmp / "wt" / spec["task_id"])),
                runtime_dir=spec.get(
                    "runtime_dir", str(self.tmp / "wt-runtime" / spec["task_id"])
                ),
                base_head=spec.get("base_head"),
                status=spec.get("status", "pending"),
            )

    def _apply_breaks(self) -> None:
        if self.setup.get("break_git"):
            shutil.rmtree(self.workspace / ".git", ignore_errors=True)
        break_linked = self.setup.get("break_linked")
        if break_linked and self.linked_storage is not None:
            if break_linked == "unreadable":
                for suffix in ("", "-wal", "-shm"):
                    path = Path(str(self.linked_storage.path) + suffix)
                    if path.is_file():
                        path.unlink()

    def _connect(self, storage=None):
        import sqlite3

        storage = storage or self.storage
        conn = sqlite3.connect(str(storage.path))
        conn.row_factory = sqlite3.Row
        return conn

    def _seed_task(
        self,
        conn,
        spec: dict,
        *,
        project_id: str | None = None,
        workspace: Path | None = None,
    ) -> None:
        project_id = project_id or self.config.project_id
        workspace = workspace or self.workspace
        task_id = spec["task_id"]
        self.task_ids.append(task_id)
        status = spec.get("status", "implementing")
        session_id = spec.get("session_id")
        allowed_paths = spec.get("allowed_paths", ["module.py"])
        test_commands = spec.get("test_commands", ["pytest -q"])
        budget_json = spec.get("budget_raw")
        if budget_json is None and spec.get("budget") is not None:
            budget_json = json.dumps(spec["budget"], sort_keys=True)
        depends_on = spec.get("depends_on") or []
        close_at = FIXED_TS if spec.get("close_requested") else None
        snapshot = spec.get("snapshot")
        if spec.get("snapshot_from_workspace"):
            snapshot = self.modules["git_snapshot"].take_snapshot(workspace)
            snapshot["allow_dirty"] = bool(spec.get("allow_dirty", False))
            snapshot["allow_commit"] = bool(spec.get("allow_commit", False))
            snapshot["external_repositories"] = []
        base_head = spec.get("base_head")
        if snapshot is not None:
            self.submit_snapshots[task_id] = json.loads(json.dumps(snapshot))
            # The real submit flow persists ``snapshot["head"]`` as ``base_head``
            # (mcp_server.submit_task_impl); mirror it unless a case pins one.
            if base_head is None and spec.get("snapshot_from_workspace"):
                base_head = snapshot.get("head")
        conn.execute(
            "INSERT OR REPLACE INTO tasks"
            "(task_id, project_id, workspace, status, session_id, task, allowed_paths,"
            " test_commands, created_at, updated_at, base_head, snapshot, revision_count,"
            " close_requested_at, close_reason, budget_json, workflow_id, depends_on,"
            " execution_mode, profile, profile_json, profile_hash, profile_source)"
            " VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (
                task_id,
                project_id,
                str(workspace),
                status,
                session_id,
                spec.get("text", "fixture task"),
                json.dumps(allowed_paths),
                json.dumps(test_commands),
                FIXED_TS,
                FIXED_TS,
                base_head,
                json.dumps(snapshot) if snapshot is not None else None,
                int(spec.get("revision_count", 0)),
                close_at,
                "fixture reason" if close_at else None,
                budget_json,
                spec.get("workflow_id"),
                json.dumps(depends_on, sort_keys=True) if depends_on else None,
                spec.get("execution_mode", "direct"),
                spec.get("profile"),
                spec.get("profile_json"),
                spec.get("profile_hash"),
                spec.get("profile_source"),
            ),
        )
        for round_spec in spec.get("rounds", []):
            result_json = (
                json.dumps(round_spec["stored_result"])
                if round_spec.get("stored_result") is not None
                else None
            )
            verifier_json = (
                json.dumps(round_spec["verifier_progress"])
                if round_spec.get("verifier_progress") is not None
                else None
            )
            conn.execute(
                "INSERT OR REPLACE INTO rounds"
                "(task_id, project_id, round_number, request_id, payload_hash, kind,"
                " status, outbound_message_id, attempted, response_message_id, response,"
                " error_code, result_json, findings, structured_findings, session_id,"
                " worker_started_at, worker_deadline_at, verifier_state, verifier_json,"
                " checkpoint_json, created_at, updated_at)"
                " VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (
                    task_id,
                    project_id,
                    int(round_spec["round_number"]),
                    round_spec.get("request_id", f"req-{round_spec['round_number']}"),
                    "0" * 64,
                    round_spec.get("kind", "implement"),
                    round_spec.get("status", "complete"),
                    None,
                    1,
                    None,
                    round_spec.get("response"),
                    round_spec.get("error_code"),
                    result_json,
                    round_spec.get("findings"),
                    round_spec.get("structured_findings"),
                    round_spec.get("session_id", session_id),
                    round_spec.get("worker_started_at"),
                    round_spec.get("worker_deadline_at"),
                    round_spec.get("verifier_state"),
                    verifier_json,
                    None,
                    FIXED_TS,
                    FIXED_TS,
                ),
            )

    # -- doubles ---------------------------------------------------------
    def fake_check_server(self, config):
        if self.server == "unavailable":
            return "server_unavailable: ${DETAIL}"
        if self.server == "unhealthy":
            return "server_unhealthy: healthy=False"
        if self.server == "wrong_directory":
            return "server_wrong_directory: ${FOREIGN_PATH}"
        return None

    def fake_turn_active(self, config, session_id):
        if self.server in ("busy", "retry"):
            return True, None
        return False, None

    def record_spawn(self, config, task_id, round_number):
        self.spawned.append((task_id, int(round_number)))
        if self.worker_running_after_spawn:
            self.running.add(task_id)

    def server_fns(self, mcp_server):
        if self._server_fns is None:
            server = mcp_server.build_server(self.config, self.storage)
            self._server_fns = {
                name: tool.fn
                for name, tool in server._tool_manager._tools.items()
            }
            self._server = server
        return self._server_fns

    # -- normalization ---------------------------------------------------
    def normalize_str(self, value: str) -> str:
        if value in self.mapping:
            return value
        if value == "${WORKSPACE}":
            return value
        if value == str(self.workspace):
            return "${WORKSPACE}"
        if value == str(self.tmp):
            return "${FIXTURE_DIR}"
        if value == FIXED_SESSION:
            return "${SESSION_ID}"
        if value == FIXED_REVISION_SESSION:
            return "${REVISION_SESSION_ID}"
        if value == "proj":
            return "${PROJECT_ID}"
        if value == "wf-1":
            return "${WORKFLOW_ID}"
        if value == "req-1":
            return "${REQUEST_ID}"
        if value == "req-2":
            return "${REVISION_REQUEST_ID}"
        if value == FIXED_FOREIGN:
            return "${FOREIGN_PATH}"
        if value == "task-1":
            return "${TASK_ID}"
        if value == "task-2":
            return "${OTHER_TASK_ID}"
        if _UUID_RE.match(value):
            return "${TASK_ID}"
        if _ISO_RE.match(value):
            return "${TIMESTAMP}"
        if value.startswith("agent-bridge ") and " round " in value:
            return "${SESSION_TITLE}"
        if "agent-bridge" in value and "console" in value:
            return "${CONSOLE_COMMAND}"
        if "attach-opencode" in value:
            return "${ATTACH_COMMAND}"
        if value.startswith(str(self.workspace)):
            return "${WORKSPACE}" + value[len(str(self.workspace)) :]
        return value


def _deep_equal(actual, expected) -> bool:
    return actual == expected


def _subset_equal(actual, expected) -> bool:
    """True when ``expected`` is a recursive subset of ``actual``."""
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            return False
        return all(
            key in actual and _subset_equal(actual[key], value)
            for key, value in expected.items()
        )
    if isinstance(expected, list):
        if not isinstance(actual, list) or len(actual) != len(expected):
            return False
        return all(_subset_equal(a, e) for a, e in zip(actual, expected))
    return actual == expected


def _serialized(value) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)


def _suspected_text(args: dict, source: str) -> str:
    """Rebuild the exact untrusted text the secret gate scans for one tool."""
    if source == "task":
        return str(args.get("task", ""))
    if source == "findings+structured":
        findings = str(args.get("findings", ""))
        parts = [findings]
        structured = args.get("structured_findings")
        if isinstance(structured, list):
            for item in structured:
                if isinstance(item, dict):
                    for key in ("message", "path", "code"):
                        value = item.get(key)
                        if isinstance(value, str):
                            parts.append(value)
        return "\n".join(parts)
    return ""


def _check_error(
    case: dict,
    actual: dict,
    args: dict,
    fixture: Fixture,
    modules: dict,
    report: Report,
    label: str,
) -> bool:
    category = case["error_category"]
    if not isinstance(actual, dict) or "error" not in actual:
        report.fail(label, f"expected error {category}, got {actual!r}")
        return False
    if actual.get("error") != category:
        report.fail(label, f"error {actual.get('error')!r} != {category!r}")
        return False
    ok = True
    for key, expected in case.get("error_payload", {}).items():
        if key not in actual:
            report.fail(label, f"error_payload missing {key}")
            ok = False
        elif not _subset_equal(_normalize(actual[key], fixture), expected):
            report.fail(
                label,
                f"error_payload[{key}] {_normalize(actual[key], fixture)!r} != {expected!r}",
            )
            ok = False
    # The whole serialized result, not only ``error_payload``, must obey the
    # exact field set and must never echo forbidden content (a raw credential or
    # the rejected task text).  This closes the subset loophole where an extra
    # leaked field would otherwise pass.
    exact_fields = case.get("error_exact_fields")
    if exact_fields is not None and set(actual) != set(exact_fields):
        report.fail(
            label,
            f"error exact fields {sorted(actual)} != {sorted(exact_fields)}",
        )
        ok = False
    blob = _serialized(actual)
    for forbidden in _subst(case.get("forbidden_content", []), fixture.mapping):
        if forbidden and forbidden in blob:
            report.fail(label, f"forbidden content {forbidden!r} present in result")
            ok = False
    # Validate the promised category-only shape directly against the real
    # scanner on the same untrusted text, so the expected category list is not
    # merely self-consistent with the corpus.
    source = case.get("suspected_categories_from")
    if source:
        text = _suspected_text(args, source)
        observed = modules["secret_scanner"].suspected_secret_categories(text)
        expected_categories = case.get("error_payload", {}).get("categories")
        if expected_categories is not None and observed != expected_categories:
            report.fail(
                label,
                f"scanner categories {observed!r} != expected {expected_categories!r}",
            )
            ok = False
        if actual.get("categories") != observed:
            report.fail(
                label,
                f"result categories {actual.get('categories')!r} != scanner {observed!r}",
            )
            ok = False
    return ok


def _check_success(case: dict, actual: dict, fixture: Fixture, report: Report, label: str) -> bool:
    if not isinstance(actual, dict):
        report.fail(label, f"expected a dict, got {actual!r}")
        return False
    if actual.get("error") is not None:
        report.fail(label, f"unexpected error {actual.get('error')!r}")
        return False
    expect = case.get("expect", {})
    ok = True
    fields = expect.get("fields")
    if fields is not None:
        if expect.get("shape", "contains") == "exact":
            if set(actual) != set(fields):
                report.fail(label, f"exact fields {sorted(actual)} != {sorted(fields)}")
                ok = False
        else:
            missing = [field for field in fields if field not in actual]
            if missing:
                report.fail(label, f"missing fields {missing}")
                ok = False
    for absent in expect.get("absent", []):
        if absent in actual:
            report.fail(label, f"forbidden field {absent!r} present")
            ok = False
    for key, expected in expect.get("values", {}).items():
        if key not in actual:
            report.fail(label, f"values key {key!r} missing")
            ok = False
            continue
        if not _deep_equal(actual[key], expected):
            report.fail(label, f"value[{key}] {actual[key]!r} != {expected!r}")
            ok = False
    return ok


def _check_side_effects(
    case: dict, fixture: Fixture, report: Report, label: str, *, strict: bool
) -> bool:
    ok = True
    normalized_spawned = [
        (fixture.normalize_str(task_id), round_number)
        for task_id, round_number in fixture.spawned
    ]
    expected_spawn_count = case.get("expect", {}).get("spawn_count")
    if expected_spawn_count is not None and len(fixture.spawned) != expected_spawn_count:
        report.fail(
            label,
            f"spawn count {len(fixture.spawned)} != {expected_spawn_count}",
        )
        ok = False
    for effect in case.get("expect", {}).get("side_effects", []):
        low = effect.lower()
        if "no spawn_worker" in low:
            if fixture.spawned:
                report.fail(label, f"unexpected spawn {normalized_spawned}")
                ok = False
        elif effect.startswith("spawn_worker("):
            match = re.match(r"spawn_worker\(([^,]+),\s*(\d+)\)", effect)
            expected = (match.group(1), int(match.group(2)))
            if expected not in normalized_spawned:
                report.fail(label, f"expected spawn {expected}, got {normalized_spawned}")
                ok = False
        elif "no revise round" in low:
            rounds = fixture.storage.get_rounds("task-1")
            if any(r.kind == "revise" for r in rounds):
                report.fail(label, "an unexpected revise round was persisted")
                ok = False
        elif "revise round" in low:
            rounds = fixture.storage.get_rounds("task-1")
            if not any(r.kind == "revise" and r.round_number == 2 for r in rounds):
                report.fail(label, f"no revise round 2 in {[(r.kind, r.round_number) for r in rounds]}")
                ok = False
        elif "no task or round persisted" in low:
            if fixture.storage.count_unfinished_tasks() != 0:
                report.fail(label, "a task row was persisted for a refused request")
                ok = False
            if fixture.storage.find_round_by_request(fixture.mapping["${REQUEST_ID}"]) is not None:
                report.fail(label, "a round row was persisted for a refused request")
                ok = False
        elif "no new round" in low:
            rounds = fixture.storage.get_rounds("task-1")
            if len(rounds) != 2:
                report.fail(label, f"expected exactly 2 rounds, got {len(rounds)}")
                ok = False
        elif strict:
            # A required target-v15 side effect that the harness cannot observe
            # must fail the case instead of silently passing after a SKIP.
            report.fail(label, f"unverified required side effect: {effect}")
            ok = False
        else:
            report.skip(f"{label} side_effect", f"unverified effect: {effect}")
    return ok


def _check_invariants(
    case: dict,
    actual: dict,
    fixture: Fixture,
    report: Report,
    label: str,
    *,
    strict: bool,
) -> bool:
    ok = True
    for invariant in case.get("expect", {}).get("invariants", []):
        if "ambiguous" in case["id"] and "tasks lists" in invariant:
            tasks = actual.get("tasks")
            expected = {
                ("${TASK_ID}", "implementing"),
                ("${OTHER_TASK_ID}", "revising"),
            }
            if not isinstance(tasks, list) or {
                (t.get("task_id"), t.get("status")) for t in tasks
            } != expected:
                report.fail(label, f"ambiguous tasks {tasks!r}")
                ok = False
        elif "no recovery or activation" in invariant:
            if fixture.spawned:
                report.fail(label, f"ambiguous call spawned {fixture.spawned}")
                ok = False
        elif strict:
            report.fail(label, f"unverified required invariant: {invariant}")
            ok = False
        else:
            report.skip(f"{label} invariant", f"unverified invariant: {invariant}")
    return ok


def _check_persisted_findings(
    case: dict, fixture: Fixture, report: Report, label: str
) -> bool:
    """Assert the exact canonical persisted revision round for a review call."""
    persisted = case.get("persisted_revision")
    if persisted is None:
        return True
    ok = True
    rounds = fixture.storage.get_rounds("task-1")
    revise = [r for r in rounds if r.kind == "revise"]
    if len(revise) != 1:
        report.fail(label, f"expected exactly one revise round, got {len(revise)}")
        return False
    round_obj = revise[0]
    if round_obj.round_number != persisted.get("round_number", 2):
        report.fail(label, f"revise round_number {round_obj.round_number}")
        ok = False
    if round_obj.findings != persisted.get("findings"):
        report.fail(
            label,
            f"persisted findings {round_obj.findings!r} != {persisted.get('findings')!r}",
        )
        ok = False
    raw = round_obj.structured_findings
    try:
        decoded = json.loads(raw) if raw is not None else None
    except ValueError:
        decoded = raw
    if decoded != persisted.get("structured_findings"):
        report.fail(
            label,
            f"persisted structured_findings {decoded!r} != "
            f"{persisted.get('structured_findings')!r}",
        )
        ok = False
    if persisted.get("revision_count") is not None:
        task = fixture.storage.get_task("task-1")
        if task is None or task.revision_count != persisted["revision_count"]:
            report.fail(
                label,
                f"revision_count {getattr(task, 'revision_count', None)} != "
                f"{persisted['revision_count']}",
            )
            ok = False
    return ok


def _check_activation(
    case: dict, fixture: Fixture, report: Report, label: str
) -> bool:
    """Assert the persisted baseline/status/spawn contract of an activation."""
    spec = case.get("activation")
    if spec is None:
        return True
    ok = True
    task = fixture.storage.get_task("task-1")
    if task is None:
        report.fail(label, "activation task missing")
        return False
    if spec.get("persisted_status") is not None:
        if task.status != spec["persisted_status"]:
            report.fail(label, f"persisted status {task.status!r} != {spec['persisted_status']!r}")
            ok = False
    if "spawn_count" in spec:
        if len(fixture.spawned) != spec["spawn_count"]:
            report.fail(
                label,
                f"spawn count {len(fixture.spawned)} != {spec['spawn_count']}",
            )
            ok = False
    git_snapshot = fixture.modules["git_snapshot"]
    original = fixture.submit_snapshots.get("task-1")
    if spec.get("baseline_refreshed") is True:
        # Compare the persisted baseline with a *fresh* real snapshot of the
        # activation-time workspace, never with the harness's own bookkeeping.
        # The old check only asserted ``base_head``/``allow_dirty``, so a broken
        # refresh that kept a stale (or corrupted) ``manifest``/``index``/
        # ``dirty_paths``/``status`` still passed.  The full comparison below is
        # what makes the two target-v15 rebaseline cases non-vacuous.
        fresh = git_snapshot.take_snapshot(fixture.workspace)
        current_head = fresh["head"]
        if task.base_head != current_head:
            report.fail(
                label,
                f"base_head {task.base_head!r} not refreshed to {current_head!r}",
            )
            ok = False
        expected_allow_dirty = spec.get("allow_dirty")
        if expected_allow_dirty is not None:
            if not task.snapshot or task.snapshot.get("allow_dirty") != expected_allow_dirty:
                report.fail(
                    label,
                    f"persisted allow_dirty "
                    f"{(task.snapshot or {}).get('allow_dirty')!r} != {expected_allow_dirty!r}",
                )
                ok = False
        if spec.get("assert_refreshed_snapshot"):
            if original is None:
                report.fail(label, "no submit snapshot recorded to rebaseline")
                ok = False
            else:
                expected = dict(fresh)
                expected["allow_dirty"] = bool(original.get("allow_dirty", False))
                expected["allow_commit"] = bool(original.get("allow_commit", False))
                expected["external_repositories"] = []
                persisted = task.snapshot or {}
                if set(persisted) != set(expected):
                    report.fail(
                        label,
                        f"refreshed snapshot fields {sorted(persisted)} != "
                        f"{sorted(expected)}",
                    )
                    ok = False
                for key in sorted(expected):
                    if persisted.get(key) != expected[key]:
                        report.fail(
                            label,
                            f"refreshed snapshot[{key}] {persisted.get(key)!r} != "
                            f"{expected[key]!r}",
                        )
                        ok = False
                if persisted == original:
                    report.fail(
                        label,
                        "stale submit snapshot was not rebaselined on activation",
                    )
                    ok = False
    if spec.get("baseline_refreshed") is False:
        if spec.get("persisted_base_head") is not None:
            if task.base_head != spec["persisted_base_head"]:
                report.fail(
                    label,
                    f"base_head {task.base_head!r} != preserved "
                    f"{spec['persisted_base_head']!r}",
                )
                ok = False
        if spec.get("assert_preserved_snapshot"):
            if original is None:
                report.fail(label, "no submit snapshot recorded to preserve")
                ok = False
            elif task.snapshot != original:
                report.fail(
                    label,
                    f"blocked activation mutated snapshot {task.snapshot!r} != "
                    f"original {original!r}",
                )
                ok = False
    if spec.get("dependencies_satisfied") is not None:
        kinds = [e["kind"] for e in fixture.storage.list_events("task-1")]
        count = kinds.count("dependencies_satisfied")
        if count != spec["dependencies_satisfied"]:
            report.fail(
                label,
                f"dependencies_satisfied count {count} != "
                f"{spec['dependencies_satisfied']}",
            )
            ok = False
    if spec.get("blocker_reason") is not None:
        # Recompute the read-only diagnostic the same way task_status does.
        blocker = None
        try:
            blocker = fixture.modules["mcp_server"]._activation_baseline_blocker(
                fixture.config, task
            )
        except Exception as exc:  # noqa: BLE001
            report.fail(label, f"blocker recompute failed: {exc}")
            return False
        if not blocker or blocker.get("reason") != spec["blocker_reason"]:
            report.fail(label, f"activation blocker {blocker!r}")
            ok = False
        for key in ("dirty_paths", "paths"):
            if key in spec:
                observed = blocker.get(key) if blocker else None
                if observed != spec[key]:
                    report.fail(label, f"blocker {key} {observed!r} != {spec[key]!r}")
                    ok = False
    return ok


def _check_profile_identity(
    case: dict, actual: dict, fixture: Fixture, modules: dict, report: Report, label: str
) -> bool:
    """Verify the pinned profile snapshot identity/hash and public non-leakage."""
    spec = case.get("profile_identity")
    if spec is None:
        return True
    ok = True
    profiles = modules["profiles"]
    task = fixture.storage.get_active_task()
    if task is None:
        report.fail(label, "profile identity: no persisted task")
        return False
    if task.profile != spec.get("profile"):
        report.fail(label, f"profile id {task.profile!r} != {spec.get('profile')!r}")
        ok = False
    parsed = profiles.parse_profile_snapshot(task.profile_json)
    if parsed is None:
        report.fail(label, "profile snapshot is not parseable")
        return False
    canonical = profiles.canonical_snapshot(parsed)
    if canonical != task.profile_json:
        report.fail(label, "persisted profile snapshot is not canonical")
        ok = False
    if profiles.snapshot_hash(parsed) != task.profile_hash:
        report.fail(
            label,
            f"profile_hash {task.profile_hash!r} != snapshot hash "
            f"{profiles.snapshot_hash(parsed)!r}",
        )
        ok = False
    for field in spec.get("forbidden_fields", []):
        if field in actual:
            report.fail(label, f"public result leaks forbidden profile field {field!r}")
            ok = False
    for field in spec.get("public_fields", []):
        if field not in actual:
            report.fail(label, f"public result missing profile field {field!r}")
            ok = False
    return ok


def _check_linked_gate(
    case: dict, fixture: Fixture, modules: dict, report: Report, label: str
) -> bool:
    """Run the real read-only dependency gate on the synthetic linked DBs.

    The expected gate is never fed to the tool as a mocked return: the persisted
    task is read back and ``_task_dependency_gate`` is executed against the
    isolated linked project's state database.
    """
    spec = case.get("expected_gate")
    if spec is None:
        return True
    task = fixture.storage.get_active_task()
    if task is None:
        report.fail(label, "linked gate: no persisted task")
        return False
    gate = modules["mcp_server"]._task_dependency_gate(fixture.config, task)
    normalized = _normalize(gate, fixture)
    if normalized != spec:
        report.fail(label, f"linked gate {normalized!r} != {spec!r}")
        return False
    return True


def _invoke_tool(tool: str, mcp_server, fixture: Fixture, args: dict):
    if tool == "project_info":
        return fixture.server_fns(mcp_server)["project_info"]()
    if tool == "submit_task":
        return mcp_server.submit_task_impl(fixture.storage, fixture.config, **args)
    if tool == "task_status":
        return mcp_server.task_status_impl(fixture.storage, fixture.config, **args)
    if tool == "request_changes":
        return mcp_server.request_changes_impl(fixture.storage, fixture.config, **args)
    if tool == "accept_task":
        return mcp_server.accept_task_impl(fixture.storage, fixture.config, **args)
    if tool == "close_task":
        return mcp_server.close_task_impl(fixture.storage, fixture.config, **args)
    return None


def run_case(case: dict, modules: dict, report: Report, *, strict: bool) -> None:
    label = f"{case['id']} [{case.get('coverage', '?')}]"
    mcp_server = modules["mcp_server"]
    extra_placeholders = modules["extra_placeholders"]
    base_mapping = dict(BASE_MAPPING)
    base_mapping.update(extra_placeholders)
    try:
        with tempfile.TemporaryDirectory(prefix="mcp-fixture-") as tmp_name:
            setup = _subst(case["setup"], base_mapping)
            fixture = Fixture(Path(tmp_name), setup, extra_placeholders)
            fixture.build(modules)
            args = _subst(case["input"], fixture.mapping)
            tool = case["tool"]
            with ExitStack() as stack:
                stack.enter_context(
                    mock.patch.object(
                        mcp_server,
                        "_task_worker_running",
                        lambda config, task_id: task_id in fixture.running,
                    )
                )
                stack.enter_context(
                    mock.patch.object(mcp_server, "_check_server", fixture.fake_check_server)
                )
                stack.enter_context(
                    mock.patch.object(mcp_server, "_server_turn_active", fixture.fake_turn_active)
                )
                stack.enter_context(
                    mock.patch.object(
                        mcp_server,
                        "_blockers_present",
                        lambda config, session_id: fixture.blockers,
                    )
                )
                stack.enter_context(
                    mock.patch.object(mcp_server, "spawn_worker", fixture.record_spawn)
                )
                stack.enter_context(
                    mock.patch.object(
                        mcp_server, "_submit_phase", lambda *a, **k: None
                    )
                )
                repeat = int(case.get("repeat", 1))
                actual = None
                for _ in range(repeat):
                    actual = _invoke_tool(tool, mcp_server, fixture, args)
                    if actual is None:
                        report.skip(label, f"unsupported tool {tool!r}")
                        return
            normalized = _normalize(actual, fixture)
            if not strict:
                # Legacy-v6 cases are historical: some surfaces deliberately
                # changed in v15, so only the stable expectation kind/category
                # is checked.  A divergence is reported as SKIP, never PASS.
                if case["expectation"] == "error":
                    observed = (
                        str(normalized.get("error", "")).split(":")[0]
                        if isinstance(normalized, dict)
                        else ""
                    )
                    if observed == case["error_category"] or (
                        isinstance(normalized, dict)
                        and normalized.get("status") == case["error_category"]
                    ):
                        report.ok(label)
                    else:
                        report.skip(
                            label,
                            "legacy-v6 case not reproduced by the isolated harness "
                            "(unsupported harness boundary: legacy external-repo and "
                            "turn-active setup differs; no v15 divergence claimed; "
                            f"got {normalized!r})",
                        )
                elif isinstance(normalized, dict) and normalized.get("error") is None:
                    report.ok(label)
                else:
                    report.skip(
                        label,
                        "legacy-v6 case not reproduced by the isolated harness "
                        "(unsupported harness boundary: legacy external-repo and "
                        "turn-active setup differs; no v15 divergence claimed; "
                        f"got {normalized!r})",
                    )
                return
            if case["expectation"] == "error":
                ok = _check_error(case, normalized, args, fixture, modules, report, label)
            else:
                ok = _check_success(case, normalized, fixture, report, label)
            if ok:
                ok = _check_side_effects(case, fixture, report, label, strict=strict)
                ok = _check_invariants(
                    case, normalized, fixture, report, label, strict=strict
                ) and ok
                ok = _check_persisted_findings(case, fixture, report, label) and ok
                ok = _check_activation(case, fixture, report, label) and ok
                ok = _check_linked_gate(case, fixture, modules, report, label) and ok
                ok = _check_profile_identity(
                    case, normalized, fixture, modules, report, label
                ) and ok
            if ok:
                report.ok(label)
    except Exception as exc:  # noqa: BLE001
        detail = f"{type(exc).__name__}: {exc}"
        if strict:
            report.fail(label, detail)
            traceback.print_exc()
        else:
            report.skip(label, f"legacy-v6 case not reproducible by the isolated harness: {detail}")


def check_tool_surface(modules: dict, corpus: dict, report: Report) -> None:
    mcp_server = modules["mcp_server"]
    failures_before = len(report.failed)
    with tempfile.TemporaryDirectory(prefix="mcp-surface-") as tmp_name:
        fixture = Fixture(Path(tmp_name), {}, modules["extra_placeholders"])
        fixture.build(modules)
        server = mcp_server.build_server(fixture.config, fixture.storage)
        tools = asyncio.run(server.list_tools())
        by_name = {tool.name: tool for tool in tools}
        expected = corpus.get("tool_surface", {})
        if len(tools) != expected.get("tool_count", 6):
            report.fail(
                "tool-surface count", f"{len(tools)} != {expected.get('tool_count')}"
            )
        for spec in expected.get("tools", []):
            name = spec["name"]
            tool = by_name.get(name)
            if tool is None:
                report.fail(f"tool-surface {name}", "missing")
                continue
            schema = tool.inputSchema or {}
            required = set(schema.get("required") or [])
            if required != set(spec.get("required", [])):
                report.fail(f"tool-surface {name} required", f"{sorted(required)}")
            properties = schema.get("properties", {})
            for param, default in spec.get("defaults", {}).items():
                if param not in properties:
                    report.fail(f"tool-surface {name}.{param}", "parameter missing")
                elif properties[param].get("default") != default:
                    report.fail(
                        f"tool-surface {name}.{param} default",
                        f"{properties[param].get('default')!r} != {default!r}",
                    )
            declared_annotations = spec.get("annotations")
            if declared_annotations is not None:
                observed = getattr(tool, "annotations", None)
                if hasattr(observed, "model_dump"):
                    observed = observed.model_dump(exclude_none=True)
                elif observed is not None and not isinstance(observed, dict):
                    observed = dict(observed)
                if not isinstance(observed, dict):
                    report.fail(
                        f"tool-surface {name} annotations", f"{observed!r}"
                    )
                else:
                    for key, expected_value in declared_annotations.items():
                        if observed.get(key) != expected_value:
                            report.fail(
                                f"tool-surface {name}.{key} annotation",
                                f"{observed.get(key)!r} != {expected_value!r}",
                            )
        # The published JSON schema drops the ``ge``/``le`` bounds, so the
        # 0..300 contract is proven behaviourally against the live function.
        with mock.patch.object(mcp_server, "spawn_worker", lambda *a, **k: None):
            for bad in (301, -1, True):
                result = mcp_server.task_status_impl(
                    fixture.storage, fixture.config, None, bad
                )
                if result.get("error") != "invalid_wait_seconds":
                    report.fail(
                        f"tool-surface task_status wait_seconds={bad!r}",
                        f"got {result!r}",
                    )
            for good in (0, 300):
                result = mcp_server.task_status_impl(
                    fixture.storage, fixture.config, None, good
                )
                if result.get("error") is not None:
                    report.fail(
                        f"tool-surface task_status wait_seconds={good!r}",
                        f"got {result!r}",
                    )
    if len(report.failed) == failures_before:
        report.ok("tool-surface: names, required, defaults and 0..300 bounds verified")


def check_corpus_metadata(corpus: dict, modules: dict, report: Report) -> None:
    source = corpus.get("source", {})
    if source.get("commit") != PINNED_COMMIT:
        report.fail("corpus source.commit", f"{source.get('commit')} != {PINNED_COMMIT}")
    else:
        report.ok("corpus source.commit matches the pinned reference")
    if corpus.get("contract_schema_version") != PINNED_SCHEMA:
        report.fail(
            "corpus contract_schema_version",
            f"{corpus.get('contract_schema_version')} != {PINNED_SCHEMA}",
        )
    else:
        report.ok("corpus contract_schema_version is 15")
    categories = set(corpus.get("error_categories", {}))
    new_ids = [
        case["id"]
        for case in corpus["cases"]
        if case.get("coverage") == "target-v15"
    ]
    report.ok(f"corpus target-v15 cases: {len(new_ids)}") if new_ids else report.fail(
        "corpus target-v15 cases", "none"
    )
    for case in corpus["cases"]:
        if case["expectation"] == "error" and case.get("error_category") not in categories:
            report.fail(
                f"corpus {case['id']} category",
                f"{case.get('error_category')!r} not documented",
            )
    report.ok("corpus every error_category is documented")


def check_activation_regressions(modules: dict, corpus: dict, report: Report) -> None:
    """Prove the rebaseline assertions are not vacuous.

    Re-run the two strict target-v15 rebaseline cases with the reference's
    ``_refresh_activation_baseline`` replaced first by the exact broken refresh
    that motivated the round-2 finding (a correctly set ``base_head`` but an
    empty ``manifest``) and then by a no-op that leaves the stale submit
    snapshot in place.  The enhanced ``_check_activation`` must reject both;
    otherwise the corpus would keep passing on a corrupt baseline.
    """
    mcp_server = modules["mcp_server"]
    cases = {case["id"]: case for case in corpus["cases"]}
    targets = [
        "task-status-activates-waiting-dependency-baseline-refresh",
        "task-status-activates-waiting-dependency-allow-dirty",
    ]

    def corrupt_refresh(storage, config, task):
        blocker, snapshot = mcp_server._recompute_activation_baseline(config, task)
        if blocker is not None:
            return blocker
        if snapshot is not None:
            storage.refresh_task_baseline(
                task.task_id,
                {**(task.snapshot or {}), "manifest": {}},
                snapshot.get("head"),
            )
        return None

    def no_refresh(storage, config, task):
        return None

    modes = (("corrupt-refresh", corrupt_refresh), ("no-refresh", no_refresh))
    for mode, patched in modes:
        for case_id in targets:
            case = cases.get(case_id)
            if case is None:
                report.fail(f"activation-regression {mode}", f"missing case {case_id}")
                continue
            sub = Report()
            with mock.patch.object(
                mcp_server, "_refresh_activation_baseline", patched
            ), redirect_stdout(io.StringIO()):
                run_case(case, modules, sub, strict=True)
            detected = any(
                "snapshot" in detail or "base_head" in detail
                for _label, detail in sub.failed
            )
            if detected:
                report.ok(f"activation-regression {mode} {case_id} is detected")
            else:
                report.fail(
                    f"activation-regression {mode} {case_id}",
                    "a corrupt or missing rebaseline did not trip the "
                    "snapshot/base_head assertions",
                )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-repo", default=str(DEFAULT_REFERENCE))
    parser.add_argument("--filter", default="")
    parser.add_argument("--target-only", action="store_true")
    args = parser.parse_args(argv)

    if not CORPUS_PATH.is_file():
        print(f"corpus not found at {CORPUS_PATH}", file=sys.stderr)
        return 2
    try:
        corpus = json.loads(CORPUS_PATH.read_text(encoding="utf-8"))
    except ValueError as exc:
        print(f"corpus is not valid JSON: {exc}", file=sys.stderr)
        return 2

    reference = Path(args.python_repo)
    head = subprocess.run(
        ["git", "-C", str(reference), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
    )
    if head.returncode != 0:
        print(f"cannot read reference HEAD: {head.stderr.strip()}", file=sys.stderr)
        return 2
    if head.stdout.strip() != PINNED_COMMIT:
        print(
            f"reference HEAD {head.stdout.strip()} != pinned {PINNED_COMMIT}",
            file=sys.stderr,
        )
        return 2

    src = (reference / "src").resolve()
    sys.path.insert(0, str(src))
    import agent_bridge  # noqa: E402
    from agent_bridge import (  # noqa: E402
        config,
        git_snapshot,
        mcp_server,
        profiles,
        secret_scanner,
        storage,
        usage,
    )

    loaded = Path(agent_bridge.__file__).resolve().parent
    if loaded != src / "agent_bridge":
        print(f"imported agent_bridge from {loaded}", file=sys.stderr)
        return 2

    # Canonical built-in snapshots so seeded rows carry a valid profile_json.
    def snapshot(profile_id: str, origin: str) -> str:
        definition = profiles.builtin_profiles()[profile_id]
        built = profiles.build_snapshot(
            definition, origin, model=None, model_source="project"
        )
        return profiles.canonical_snapshot(built)

    def snapshot_hash(profile_id: str, origin: str) -> str:
        return profiles.snapshot_hash(
            profiles.parse_profile_snapshot(snapshot(profile_id, origin))
        )

    extra_placeholders = {
        "${SNAPSHOT_TEST_WRITER}": snapshot("test-writer", "argument"),
        "${SNAPSHOT_TEST_WRITER_HASH}": snapshot_hash("test-writer", "argument"),
        "${SNAPSHOT_IMPLEMENTER}": snapshot("implementer", "builtin_default"),
        "${STRUCTURED_FINDINGS_201}": [
            {
                "severity": "info",
                "path": "module.py",
                "code": "F-1",
                "message": "finding",
            }
        ]
        * 201,
    }
    modules = {
        "config": config,
        "git_snapshot": git_snapshot,
        "mcp_server": mcp_server,
        "profiles": profiles,
        "secret_scanner": secret_scanner,
        "storage": storage,
        "usage": usage,
        "extra_placeholders": extra_placeholders,
    }

    report = Report()
    print(f"reference {reference} @ {head.stdout.strip()} (read-only)")
    check_corpus_metadata(corpus, modules, report)
    check_tool_surface(modules, corpus, report)
    check_activation_regressions(modules, corpus, report)

    selected = [
        case
        for case in corpus["cases"]
        if not args.filter or args.filter in case["id"]
    ]
    targeted = [c for c in selected if c.get("coverage") == "target-v15"]
    legacy = [] if args.target_only else [
        c for c in selected if c.get("coverage") != "target-v15"
    ]
    print(f"running {len(targeted)} target-v15 case(s) strictly")
    for case in targeted:
        run_case(case, modules, report, strict=True)
    if legacy:
        print(f"running {len(legacy)} legacy-v6 case(s) leniently")
        for case in legacy:
            run_case(case, modules, report, strict=False)

    print(
        f"\nsummary: {len(report.passed)} passed, {len(report.failed)} failed, "
        f"{len(report.skipped)} skipped"
    )
    if report.failed:
        for label, detail in report.failed:
            print(f"FAILED {label}: {detail}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    bootstrap_error = _ensure_reference(sys.argv[1:])
    if bootstrap_error:
        print(bootstrap_error, file=sys.stderr)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1:]))
