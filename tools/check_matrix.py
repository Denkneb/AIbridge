#!/usr/bin/env python3
"""Rust and frozen contract matrix; never opens the reference runtime state.

Source checks run on temporary Git clones at both historical pins. Exit zero
means all selected suites passed; verifier skips remain visible in their logs.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PINS = {
    "v15": "86c65b55cc7cca0b9e917a36f4f6c317eac4cc1a",
    "v17": "e52a46158cbeb4f3ae35063d395c05ea0ce144bc",
}
FIXTURES = ["docs/fixtures/sqlite/verify.py", "docs/fixtures/sqlite/delta/verify.py"]
SOURCES = {
    "v15": ["docs/fixtures/config/verify.py", "docs/fixtures/mcp/verify.py",
            "docs/fixtures/security/verify.py", "docs/fixtures/runtime/verify.py",
            "docs/fixtures/sqlite/parity/verify_parity.py"],
    "v17": ["docs/verify_contract_manifest.py", "docs/fixtures/config/verify_v17.py",
            "docs/fixtures/mcp/verify_v17.py", "docs/fixtures/runtime/verify_v17.py",
            "docs/fixtures/sqlite/delta/verify_parity.py"],
}


def git(*args, env):
    return subprocess.check_output(["git", "-c", "core.hooksPath=/dev/null", *args],
                                   env=env, text=True, stderr=subprocess.PIPE).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--reference", type=Path, help="read-only pinned Python source repository")
    mode.add_argument("--fixtures-only", action="store_true", help="self-contained checks without Python source parity")
    parser.add_argument("--skip-rust", action="store_true", help="Rust checks already run separately")
    parser.add_argument("--offline", action="store_true", help="use Cargo's local cache")
    parser.add_argument("--logs", type=Path, default=ROOT / "target/check-matrix")
    args = parser.parse_args()
    logs = args.logs.resolve()
    logs.mkdir(parents=True, exist_ok=True)
    (logs / "summary.json").unlink(missing_ok=True)
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1", GIT_OPTIONAL_LOCKS="0")
    reports = []

    def run(name, argv, check_env):
        log = logs / (name + ".log")
        print("running " + name, flush=True)
        with log.open("w") as out:
            result = subprocess.run(argv, cwd=ROOT, env=check_env, stdout=out, stderr=subprocess.STDOUT)
        lines = log.read_text(errors="replace").splitlines()
        report = {"suite": name, "exit_code": result.returncode, "log": str(log)}
        for line in lines:
            match = re.fullmatch(r"(?:summary: )?(\d+) passed, (\d+) failed, (\d+) skipped", line)
            if match:
                report["counts"] = dict(zip(("passed", "failed", "skipped"), map(int, match.groups())))
        reports.append(report)
        print("\n".join(lines[-14:]), flush=True)

    if not args.skip_rust:
        run("rust-format", ["cargo", "fmt", "--all", "--check"], env)
        cargo = ["cargo"] + (["--offline"] if args.offline else [])
        run("rust-clippy", cargo + ["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"], env)
        run("rust-tests", cargo + ["test", "--workspace"], env)
    for path in FIXTURES:
        run("fixtures-" + Path(path).parent.name, [sys.executable, "-B", path], env)

    if args.reference:
        reference = args.reference.resolve()
        if git("-C", str(reference), "rev-parse", "HEAD", env=env) != PINS["v17"]:
            raise ValueError("reference HEAD differs from frozen v17 pin")
        if git("-C", str(reference), "status", "--porcelain", env=env):
            raise ValueError("reference source tree is not clean")
        with tempfile.TemporaryDirectory(prefix="bridge-matrix-") as temp:
            base = Path(temp)
            isolated = dict(env, HOME=str(base / "home"), XDG_CONFIG_HOME=str(base / "config"),
                            XDG_DATA_HOME=str(base / "data"), XDG_CACHE_HOME=str(base / "cache"))
            for key in ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"):
                Path(isolated[key]).mkdir()
            for version, pin in PINS.items():
                checkout = base / version
                git("clone", "--shared", "--no-checkout", str(reference), str(checkout), env=env)
                git("-C", str(checkout), "checkout", "--detach", pin, env=env)
                venv = reference / ".venv"
                if (venv / "bin/python").is_file():
                    (checkout / ".venv").symlink_to(venv, target_is_directory=True)
                    with (checkout / ".git/info/exclude").open("a") as f:
                        f.write("\n.venv\n")
                check_env = dict(isolated, AGENT_BRIDGE_REFERENCE=str(checkout),
                                 AGENT_BRIDGE_PYTHON_REPO=str(checkout), PYTHONPATH=str(checkout / "src"))
                for path in SOURCES[version]:
                    interpreter = str(venv / "bin/python") if (venv / "bin/python").is_file() else sys.executable
                    argv = [interpreter, "-B", path]
                    if path.endswith("mcp/verify.py"):
                        argv += ["--python-repo", str(checkout)]
                    if path == "docs/verify_contract_manifest.py":
                        argv += ["--source", str(checkout)]
                    label = version + "-" + Path(path).parent.name + "-" + Path(path).stem
                    run(label, argv, check_env)
    summary = {"scope": "full-source" if args.reference else "self-contained",
               "rust_selected": not args.skip_rust, "suites": reports,
               "note": "Verifier skips are reported separately in logs and are not parity passes."}
    (logs / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return int(any(r["exit_code"] != 0 for r in reports))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as exc:
        print("matrix infrastructure error: " + str(exc), file=sys.stderr)
        raise SystemExit(2)
