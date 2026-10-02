#!/usr/bin/env python3
"""Read-only parity verifier for the config contract fixtures (task 0A.2).

This verifier is deliberately more than a JSON/schema self-check: it
materialises every case into an isolated temporary fixture tree, calls the
*actual* reference Python functions (``agent_bridge.config``,
``agent_bridge.profiles`` and ``agent_bridge.project_env``) and compares the
observed outcome (rejection category or normalised values) with the corpus.

Design notes
------------

* Read-only with respect to the reference checkout.  The reference repository
  is imported through its own ``.venv`` (falling back to the current
  interpreter) with ``PYTHONDONTWRITEBYTECODE=1`` and
  ``sys.dont_write_bytecode = True`` so no ``__pycache__`` is written there.
* No external interactions: workspaces are plain temporary directories (config
  loading never needs Git), and the only "external" state that cannot be
  created unprivileged (foreign file ownership) is mocked through
  ``os.geteuid`` instead of ``chown``.
* Nothing is written into the workspace or the reference repository; every
  fixture lives under ``tempfile.mkdtemp`` and is removed afterwards.
* A case that cannot be executed is reported as a failure; it is never counted
  as passed.

Usage::

    python3 docs/fixtures/config/verify.py

Exit code 0 means every case matched; 1 means at least one parity check failed;
2 means the corpus or the reference checkout is unusable.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import sys
import tempfile
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
CASES_PATH = REPO / "docs" / "fixtures" / "config-cases.json"
DEFAULT_REFERENCE = "/home/denis/Python/agent_bridge"
REFERENCE = Path(os.environ.get("AGENT_BRIDGE_REFERENCE", DEFAULT_REFERENCE))

PLACEHOLDER_RE = re.compile(r"\$\{[A-Z_]+\}")
HEX40_RE = re.compile(r"^[0-9a-f]{40}$")
FORBIDDEN_PATH_FRAGMENTS = ("/home/", "/Users/", "C:\\")

KNOWN_OPERATIONS = {
    "load_all_projects",
    "load_project",
    "load_linked_projects",
    "load_project_env",
    "resolve_profile",
    "profile_snapshot",
}

#: ``error_category`` -> regular expression that must match the reference
#: rejection message.  Derived by reading config.py / project_env.py, never by
#: trusting the corpus.  ``None`` means the category is produced by a dedicated
#: operation (``unknown_profile`` / ``profile_snapshot_corrupt``).
CATEGORY_MATCHERS: dict[str, str | None] = {
    "allow_parallel_writers_direct": r'allow_parallel_writers=true requires execution_mode="worktree"',
    "allow_parallel_writers_type": r"allow_parallel_writers must be a boolean",
    "auto_approve_permissions_entry": r"auto_approve_permissions entries must be non-empty",
    "auto_approve_permissions_external_directory": r"external_directory is not accepted in auto_approve_permissions",
    "auto_approve_permissions_type": r"auto_approve_permissions must be a list",
    "config_toml_invalid": r"invalid TOML",
    "default_profile_type": r"default_profile must be a non-empty string",
    "default_profile_unknown": r"is not a known profile",
    "default_profile_whitespace": r"default_profile must not have surrounding whitespace",
    "duplicate_endpoint": r"(endpoint .* already used by|duplicate server endpoint)",
    "duplicate_mcp_token_file": r"MCP token file must be unique",
    "duplicate_workspace": r"workspace .* already used by",
    "env_duplicate_name": r"duplicate variable name",
    "env_file_missing": r"project env file not found",
    "env_file_mode": r"permissions must be exactly 600",
    "env_file_not_regular": r"not a regular file",
    "env_file_owner": r"must be owned by the current user",
    "env_file_symlink": r"must not be a symlink",
    "env_invalid_name": r"invalid variable name",
    "env_invalid_utf8": r"not valid UTF-8",
    "env_line_missing_equals": r"expected NAME=value",
    "env_nul_byte": r"NUL byte is not allowed",
    "env_protected_name": r"reserved agent-bridge service variable name",
    "execution_mode_type": r"execution_mode must be a string",
    "execution_mode_value": r"execution_mode must be one of",
    "execution_mode_whitespace": r"execution_mode must not have surrounding whitespace",
    "external_directory_missing": r"external directory does not exist",
    "external_directory_not_absolute": r"external directory must be an absolute path",
    "external_directory_not_directory": r"external directory is not a directory",
    "external_directory_root": r"filesystem root '/' is not a trusted external directory",
    "external_directory_type": r"auto_approve_external_directories must be a list",
    "invalid_project_id": r"invalid project id",
    "max_active_tasks_invalid": r"max_active_tasks must be a positive integer",
    "max_rounds_not_positive": r"max_rounds must be a positive integer",
    "mcp_pairing": r"mcp_url and mcp_token_file must be set together",
    "mcp_token_equals_password": r"MCP token must use a separate credential file",
    "mcp_token_file_type": r"mcp_token_file must be a non-empty string",
    "mcp_url_endpoint": r"(only 127\.0\.0\.1 endpoints are allowed|must include an explicit port|invalid port|port out of range)",
    "mcp_url_suffix": r"mcp_url must end with /mcp",
    "missing_required_key": r"missing required key",
    "opencode_env_file_type": r"opencode_env_file must be a non-empty string",
    "opencode_model_format": r"opencode_model must be '<providerID>/<modelID>'",
    "opencode_model_type": r"opencode_model must be a string",
    "opencode_model_whitespace": r"opencode_model must not have surrounding whitespace",
    "opencode_url_credentials": r"credentials in opencode_url are not allowed",
    "opencode_url_host": r"only 127\.0\.0\.1 endpoints are allowed",
    "opencode_url_missing_port": r"must include an explicit port",
    "opencode_url_path": r"must not contain a path",
    "opencode_url_port_range": r"(port out of range|invalid port)",
    "opencode_url_query_or_fragment": r"query/fragment in opencode_url are not allowed",
    "opencode_url_scheme": r"must use http on 127\.0\.0\.1",
    "opencode_url_type": r"opencode_url must be a string",
    "profile_id_invalid": r"profile id must match",
    "profile_instructions_control": r"instructions must not contain control characters",
    "profile_instructions_secret": r"suspected secret",
    "profile_instructions_too_long": r"instructions are too long",
    "profile_instructions_type": r"instructions must be a string",
    "profile_missing_purpose": r"missing required key 'purpose'",
    "profile_model_format": r"model must be '<providerID>/<modelID>'",
    "profile_model_type": r"model must be a string",
    "profile_model_whitespace": r"model must not have surrounding whitespace",
    "profile_not_table": r"profile .* must be a table",
    "profile_purpose_control": r"purpose must not contain control characters",
    "profile_purpose_invalid": r"purpose must be a non-empty string",
    "profile_purpose_too_long": r"purpose is too long",
    "profile_snapshot_corrupt": None,
    "profile_unknown_key": r"unknown key",
    "profiles_table_type": r"profiles must be a table",
    "projects_table_missing": r"missing non-empty \[projects\] table",
    "unknown_profile": None,
    "unknown_project": r"unknown project",
    "workspace_missing": r"workspace does not exist",
    "workspace_not_directory": r"workspace is not a directory",
}


def _bootstrap_reference() -> None:
    """Re-exec under the reference ``.venv`` interpreter when available.

    The system ``python3`` may be older than 3.11 (no ``tomllib``) or lack the
    reference package.  Running the reference's own interpreter guarantees the
    exact pinned environment; the guard env var prevents an exec loop.
    """
    if os.environ.get("_AB_CONFIG_VERIFY_ACTIVE") == "1":
        return
    venv = REFERENCE / ".venv"
    venv_python = venv / "bin" / "python"
    if not venv_python.is_file():
        return
    if Path(sys.prefix).resolve() == venv.resolve():
        return
    env = dict(os.environ)
    env["_AB_CONFIG_VERIFY_ACTIVE"] = "1"
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env["PYTHONPATH"] = str(REFERENCE / "src") + os.pathsep + env.get("PYTHONPATH", "")
    os.execve(str(venv_python), [str(venv_python), os.path.abspath(__file__), *sys.argv[1:]], env)


_bootstrap_reference()

sys.dont_write_bytecode = True
if str(REFERENCE / "src") not in sys.path:
    sys.path.insert(0, str(REFERENCE / "src"))

try:
    from agent_bridge import config as ab_config
    from agent_bridge import profiles as ab_profiles
    from agent_bridge import project_env as ab_project_env
except Exception as exc:  # pragma: no cover - environment problem
    print(
        f"cannot import the reference package from {REFERENCE}: "
        f"{type(exc).__name__}: {exc}",
        file=sys.stderr,
    )
    raise SystemExit(2)


class Reporter:
    def __init__(self) -> None:
        self.errors: list[str] = []

    def error(self, case_id: str, message: str) -> None:
        self.errors.append(f"{case_id}: {message}")


def _subst(text: str, mapping: dict[str, str]) -> str:
    # Longest placeholder first so ${WORKSPACE} never eats ${WORKSPACE_LINK}.
    for key in sorted(mapping, key=len, reverse=True):
        text = text.replace(key, mapping[key])
    return text


def _subst_json(value: object, mapping: dict[str, str]) -> object:
    if isinstance(value, str):
        return _subst(value, mapping)
    if isinstance(value, list):
        return [_subst_json(item, mapping) for item in value]
    if isinstance(value, dict):
        return {key: _subst_json(item, mapping) for key, item in value.items()}
    return value


def _jsonify(value: object) -> object:
    if isinstance(value, Path):
        return str(value)
    if isinstance(value, tuple):
        return [_jsonify(item) for item in value]
    if isinstance(value, list):
        return [_jsonify(item) for item in value]
    return value


def _content_bytes(case: dict) -> bytes:
    if "content_hex" in case:
        return bytes.fromhex(case["content_hex"])
    if "content" in case:
        return case["content"].encode("utf-8")
    return b""


def _build_fixture(case_dir: Path) -> dict[str, str]:
    base = case_dir.resolve()
    ws = base / "ws"
    parent = base / "parent"
    other = parent / "other-ws"
    trusted = base / "trusted"
    config_dir = base / "config"
    config_ws = config_dir / "ws"
    for directory in (ws, other, parent, trusted, config_dir, config_ws):
        directory.mkdir(parents=True, exist_ok=True)
    file_path = base / "regular-file"
    file_path.write_text("x\n", encoding="utf-8")
    (base / "trusted-link").symlink_to(trusted, target_is_directory=True)
    (base / "ws-link").symlink_to(ws, target_is_directory=True)
    env_file = base / "abs" / "proj.env"
    env_file.parent.mkdir(parents=True, exist_ok=True)
    env_file.write_text("A=1\n", encoding="utf-8")
    os.chmod(env_file, 0o600)
    return {
        "${CONFIG_DIR}": str(config_dir),
        "${ENV_FILE}": str(env_file),
        "${FILE_PATH}": str(file_path),
        "${MISSING_DIR}": str(base / "missing"),
        "${OTHER_WORKSPACE}": str(other),
        "${PARENT_DIR}": str(parent),
        "${TRUSTED_DIR}": str(trusted),
        "${TRUSTED_LINK}": str(base / "trusted-link"),
        "${WORKSPACE}": str(ws),
        "${WORKSPACE_LINK}": str(base / "ws-link"),
    }


def _state_root(mapping: dict[str, str]) -> Path:
    return Path(mapping["${CONFIG_DIR}"]).parent / "state"


def _invoke(case: dict, cfg_file: Path, mapping: dict[str, str]) -> object:
    state = _state_root(mapping)
    operation = case["operation"]
    if operation == "load_all_projects":
        return ab_config.load_all_projects(cfg_file, state_root=state)
    if operation == "load_project":
        return ab_config.load_project(cfg_file, case["project"], state_root=state)
    if operation == "load_linked_projects":
        cfg = ab_config.load_project(cfg_file, case["project"], state_root=state)
        return [linked.project_id for linked in ab_config.load_linked_projects(cfg)]
    raise ValueError(f"unknown operation {operation!r}")


def _match_category(case: dict, message: str, reporter: Reporter) -> None:
    matcher = CATEGORY_MATCHERS.get(case["error_category"])
    if matcher is None:
        reporter.error(case["id"], f"no message matcher for {case['error_category']}")
        return
    if not re.search(matcher, message):
        reporter.error(
            case["id"],
            f"{case['error_category']} message mismatch: {message!r}",
        )


def _compare_config(
    case: dict, cfg: object, mapping: dict[str, str], reporter: Reporter
) -> None:
    for key, expected in case["expect"].items():
        if key == "profile_definitions":
            actual_definitions = cfg.profile_definitions()
            for profile_id, fields in expected.items():
                definition = actual_definitions.get(profile_id)
                if definition is None:
                    reporter.error(case["id"], f"profile {profile_id!r} missing")
                    continue
                view = {
                    "instructions": definition.instructions,
                    "model": definition.model_text,
                    "purpose": definition.purpose,
                    "source": definition.source,
                }
                for field, wanted in fields.items():
                    if view.get(field) != wanted:
                        reporter.error(
                            case["id"],
                            f"profile {profile_id}.{field}={view.get(field)!r} != {wanted!r}",
                        )
            continue
        if not hasattr(cfg, key):
            reporter.error(case["id"], f"config has no attribute {key!r}")
            continue
        actual = _jsonify(getattr(cfg, key))
        wanted = _subst_json(expected, mapping)
        if actual != wanted:
            reporter.error(case["id"], f"{key}={actual!r} != {wanted!r}")


def _verify_valid(case: dict, cfg_file: Path, mapping: dict[str, str], reporter: Reporter) -> None:
    value = _invoke(case, cfg_file, mapping)
    if case["operation"] == "load_linked_projects":
        wanted = _subst_json(case["expect"]["linked_project_ids"], mapping)
        if value != wanted:
            reporter.error(case["id"], f"linked_project_ids={value!r} != {wanted!r}")
        return
    cfg = value["proj"] if case["operation"] == "load_all_projects" else value
    _compare_config(case, cfg, mapping, reporter)


def _expect_reject(case: dict, cfg_file: Path, mapping: dict[str, str], reporter: Reporter) -> None:
    try:
        _invoke(case, cfg_file, mapping)
    except (ab_config.ConfigError, ab_project_env.ProjectEnvError) as exc:
        _match_category(case, str(exc), reporter)
    except Exception as exc:  # noqa: BLE001 - report the exact unexpected failure
        reporter.error(case["id"], f"unexpected {type(exc).__name__}: {exc}")
    else:
        reporter.error(
            case["id"], f"expected {case['error_category']} but the loader succeeded"
        )


def _verify_resolve(case: dict, cfg_file: Path, mapping: dict[str, str], reporter: Reporter) -> None:
    cfg = ab_config.load_project(cfg_file, case.get("project", "proj"), state_root=_state_root(mapping))
    resolved = ab_profiles.resolve_profile(cfg, case.get("profile"))
    if case["expectation"] == "invalid":
        if resolved is not None:
            reporter.error(case["id"], "expected unknown_profile but resolution succeeded")
        return
    if resolved is None:
        reporter.error(case["id"], "expected a resolved profile but got None")
        return
    definition, origin = resolved
    actual = {"id": definition.id, "origin": origin, "source": definition.source}
    wanted = _subst_json(case["expect"], mapping)
    if actual != wanted:
        reporter.error(case["id"], f"resolution={actual!r} != {wanted!r}")


def _verify_snapshot(case: dict, cfg_file: Path, mapping: dict[str, str], reporter: Reporter) -> None:
    cfg = ab_config.load_project(cfg_file, case.get("project", "proj"), state_root=_state_root(mapping))
    resolved = ab_profiles.resolve_profile(cfg, case.get("profile"))
    if resolved is None:
        reporter.error(case["id"], "profile did not resolve; cannot build a snapshot")
        return
    definition, origin = resolved
    model, model_source = ab_profiles.snapshot_model_source(definition, cfg.opencode_model)
    snapshot = ab_profiles.build_snapshot(definition, origin, model=model, model_source=model_source)
    if set(snapshot) != set(ab_profiles.PROFILE_SNAPSHOT_KEYS):
        reporter.error(case["id"], f"snapshot keys {sorted(snapshot)} are not canonical")
        return
    encoded = ab_profiles.canonical_snapshot(snapshot)
    if ab_profiles.parse_profile_snapshot(encoded) != snapshot:
        reporter.error(case["id"], "canonical snapshot does not round-trip")
    digest = ab_profiles.snapshot_hash(snapshot)
    if len(digest) != 64:
        reporter.error(case["id"], f"snapshot hash length {len(digest)} != 64")

    if case["expectation"] == "invalid":
        tampered = dict(snapshot)
        tampered[case["tamper"]] = case["tamper_value"]
        _, error = ab_profiles.validate_profile_snapshot(
            snapshot["id"],
            ab_profiles.canonical_snapshot(tampered),
            digest,
            snapshot["origin"],
        )
        if error != case["error_category"]:
            reporter.error(case["id"], f"tamper error {error!r} != {case['error_category']!r}")
        return

    actual = {
        "definition_version": snapshot["definition_version"],
        "id": snapshot["id"],
        "model": snapshot["model"],
        "model_source": snapshot["model_source"],
        "origin": snapshot["origin"],
        "source": snapshot["source"],
    }
    wanted = _subst_json(case["expect"], mapping)
    if actual != wanted:
        reporter.error(case["id"], f"snapshot={actual!r} != {wanted!r}")


def _verify_env(case: dict, mapping: dict[str, str], reporter: Reporter) -> None:
    env_path = Path(mapping["${ENV_FILE}"])
    setup = case.get("setup") or {}
    kind = setup.get("kind", "regular")
    mode = int(str(setup.get("mode", "0600")), 8)
    if env_path.is_symlink() or env_path.exists():
        if env_path.is_dir() and not env_path.is_symlink():
            shutil.rmtree(env_path)
        else:
            env_path.unlink()
    target = env_path.parent / "env-target"
    if target.exists():
        target.unlink()

    if kind == "regular":
        env_path.write_bytes(_content_bytes(case))
        os.chmod(env_path, mode)
    elif kind == "symlink":
        target.write_bytes(_content_bytes(case))
        os.chmod(target, mode)
        env_path.symlink_to(target)
    elif kind == "directory":
        env_path.mkdir(parents=True)
    elif kind != "missing":
        reporter.error(case["id"], f"unknown setup kind {kind!r}")
        return

    owner = setup.get("owner", "current_user")
    try:
        if owner == "other_user":
            real_uid = os.geteuid()
            fake_uid = 1 if real_uid != 1 else 2
            with mock.patch("agent_bridge.project_env.os.geteuid", return_value=fake_uid):
                result = ab_project_env.load_project_env(env_path)
        else:
            result = ab_project_env.load_project_env(env_path)
    except (ab_config.ConfigError, ab_project_env.ProjectEnvError) as exc:
        if case["expectation"] != "invalid":
            reporter.error(case["id"], f"unexpected rejection: {exc}")
            return
        _match_category(case, str(exc), reporter)
        return
    except Exception as exc:  # noqa: BLE001 - report the exact unexpected failure
        reporter.error(case["id"], f"unexpected {type(exc).__name__}: {exc}")
        return
    if case["expectation"] == "invalid":
        reporter.error(
            case["id"], f"expected {case['error_category']} but the loader succeeded"
        )
        return
    wanted = _subst_json(case["expect"]["parsed"], mapping)
    if result != wanted:
        reporter.error(case["id"], f"parsed={result!r} != {wanted!r}")


def _verify_case(case: dict, reporter: Reporter) -> None:
    case_dir = Path(tempfile.mkdtemp(prefix="ab-config-fixture-"))
    try:
        mapping = _build_fixture(case_dir)
        if case["operation"] == "load_project_env":
            _verify_env(case, mapping, reporter)
            return
        cfg_file = Path(mapping["${CONFIG_DIR}"]) / "projects.toml"
        cfg_file.write_text(_subst(case["toml"], mapping), encoding="utf-8")
        if case["operation"] == "resolve_profile":
            _verify_resolve(case, cfg_file, mapping, reporter)
        elif case["operation"] == "profile_snapshot":
            _verify_snapshot(case, cfg_file, mapping, reporter)
        elif case["expectation"] == "invalid":
            _expect_reject(case, cfg_file, mapping, reporter)
        else:
            _verify_valid(case, cfg_file, mapping, reporter)
    except Exception as exc:  # noqa: BLE001 - a case must never silently pass
        reporter.error(case["id"], f"harness {type(exc).__name__}: {exc}")
    finally:
        shutil.rmtree(case_dir, ignore_errors=True)


def _validate_corpus(corpus: dict, reporter: Reporter) -> list[dict]:
    if not isinstance(corpus, dict):
        reporter.error("<corpus>", "top level must be an object")
        return []
    cases = corpus.get("cases")
    if not isinstance(cases, list) or not cases:
        reporter.error("<corpus>", "cases must be a non-empty list")
        return []
    categories = corpus.get("error_categories")
    if not isinstance(categories, dict):
        reporter.error("<corpus>", "error_categories must be an object")
        return []
    placeholders = corpus.get("placeholder_conventions")
    if not isinstance(placeholders, dict):
        reporter.error("<corpus>", "placeholder_conventions must be an object")
        return []
    for category, matcher in CATEGORY_MATCHERS.items():
        if matcher is not None and category not in categories:
            reporter.error("<corpus>", f"matcher for unknown category {category}")
    for category in categories:
        if category not in CATEGORY_MATCHERS:
            reporter.error("<corpus>", f"category {category} has no matcher")

    ids = [case.get("id") for case in cases]
    if ids != sorted(ids):
        reporter.error("<corpus>", "cases are not sorted by id")
    if len(ids) != len(set(ids)):
        reporter.error("<corpus>", "case ids are not unique")

    source = corpus.get("source")
    if not isinstance(source, dict) or not HEX40_RE.match(str(source.get("commit", ""))):
        reporter.error("<corpus>", "source.commit must be a 40-character hex revision")

    for case in cases:
        case_id = str(case.get("id"))
        for key in ("id", "input_kind", "operation", "rule", "expectation", "coverage"):
            if key not in case:
                reporter.error(case_id, f"missing key {key!r}")
        if case.get("expectation") not in ("valid", "invalid"):
            reporter.error(case_id, "expectation must be 'valid' or 'invalid'")
        if case.get("coverage") not in ("legacy-v6", "target-v15"):
            reporter.error(case_id, "coverage must be legacy-v6 or target-v15")
        if case.get("operation") not in KNOWN_OPERATIONS:
            reporter.error(case_id, f"unknown operation {case.get('operation')!r}")
        if case.get("expectation") == "invalid":
            category = case.get("error_category")
            if category not in categories:
                reporter.error(case_id, f"unknown error_category {category!r}")
        elif "expect" not in case:
            reporter.error(case_id, "valid case lacks 'expect'")
        payload = str(case.get("toml", "")) + str(case.get("content", ""))
        for token in PLACEHOLDER_RE.findall(payload):
            if token not in placeholders:
                reporter.error(case_id, f"undeclared placeholder {token}")
        for fragment in FORBIDDEN_PATH_FRAGMENTS:
            if fragment in payload:
                reporter.error(case_id, f"machine-specific path fragment {fragment!r}")
    return cases


def _check_reference_commit(corpus: dict, reporter: Reporter) -> None:
    source_commit = str(corpus.get("source", {}).get("commit", ""))
    if not HEX40_RE.match(source_commit):
        return
    try:
        import subprocess

        result = subprocess.run(
            ["git", "-C", str(REFERENCE), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return
    if result.returncode == 0:
        head = result.stdout.strip()
        if head and head != source_commit:
            reporter.error(
                "<corpus>",
                f"source.commit {source_commit} != reference HEAD {head}",
            )


def main() -> int:
    if not CASES_PATH.is_file():
        print(f"corpus not found at {CASES_PATH}", file=sys.stderr)
        return 2
    try:
        corpus = json.loads(CASES_PATH.read_text(encoding="utf-8"))
    except ValueError as exc:
        print(f"corpus is not valid JSON: {exc}", file=sys.stderr)
        return 2

    reporter = Reporter()
    cases = _validate_corpus(corpus, reporter)
    if not cases:
        for error in reporter.errors:
            print(f"FAIL {error}", file=sys.stderr)
        print("corpus is unusable", file=sys.stderr)
        return 2

    _check_reference_commit(corpus, reporter)
    for case in cases:
        _verify_case(case, reporter)

    legacy = sum(1 for case in cases if case["coverage"] == "legacy-v6")
    target = len(cases) - legacy
    if reporter.errors:
        for error in reporter.errors:
            print(f"FAIL {error}", file=sys.stderr)
        print(
            f"{len(reporter.errors)} check(s) failed across {len(cases)} cases "
            f"({legacy} legacy-v6, {target} target-v15)",
            file=sys.stderr,
        )
        return 1
    print(
        f"ok: {len(cases)} cases verified against the reference implementation "
        f"({legacy} legacy-v6, {target} target-v15)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
