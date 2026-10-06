#!/usr/bin/env python3
"""Optional real-provider parallel smoke in a disposable private Git project.

Requires an installed OpenCode and an explicitly supplied read-only auth file.
Uses at most two tasks, manual acceptance, bounded deadlines and no permission
approval. Only sanitized evidence survives; credentials/state are temporary.
"""
import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import sqlite3
import subprocess
import tempfile
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bridge", type=Path, required=True)
    parser.add_argument("--opencode", type=Path, required=True)
    parser.add_argument("--auth-source", type=Path, required=True)
    parser.add_argument("--model", default="opencode-go/minimax-m2.7")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = {"scope": "two real parallel worktree tasks, manual acceptance",
              "model": args.model, "outcome": "failed"}
    with tempfile.TemporaryDirectory(prefix="bridge-live-parallel-") as temp:
        root = Path(temp)
        root.chmod(0o700)
        env = dict(os.environ, PATH=str(args.opencode.resolve().parent) + os.pathsep + os.environ["PATH"],
                   PYTHONDONTWRITEBYTECODE="1", GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
                   GIT_OPTIONAL_LOCKS="0",
                   AB_ROUND_DEADLINE="180", AB_HTTP_TIMEOUT="10", AB_POLL_INTERVAL="0.25",
                   OPENCODE_CONFIG_CONTENT=json.dumps({"permission": {"external_directory": "deny"}}))
        for key in ("HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME"):
            directory = root / key.lower()
            directory.mkdir(mode=0o700)
            env[key] = str(directory)
        auth = Path(env["XDG_DATA_HOME"]) / "opencode/auth.json"
        auth.parent.mkdir(mode=0o700)
        auth.write_bytes(args.auth_source.read_bytes())
        auth.chmod(0o600)
        main = root / "main"
        main.mkdir()
        release = threading.Event()
        arrivals, completions = {}, {}
        lock = threading.Lock()

        class Barrier(BaseHTTPRequestHandler):
            def do_GET(self):
                key = self.path.removeprefix("/")
                if key not in ("left", "right"):
                    self.send_error(404)
                    return
                with lock:
                    arrivals.setdefault(key, time.monotonic())
                if not release.wait(160):
                    self.send_error(408)
                    return
                with lock:
                    completions[key] = time.monotonic()
                self.send_response(200)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"ok")

            def log_message(self, *_):
                pass

        http = ThreadingHTTPServer(("127.0.0.1", 0), Barrier)
        threading.Thread(target=http.serve_forever, daemon=True).start()
        (main / "rendezvous.py").write_text(
            "import sys,urllib.request\n"
            f"urllib.request.urlopen('http://127.0.0.1:{http.server_port}/'+sys.argv[1],timeout=165).read()\n")
        (main / "check.py").write_text(
            "import sys\nfrom pathlib import Path\np=Path(sys.argv[1])\nassert p.read_bytes()==(p.stem+'\\n').encode()\n")

        def git(*argv):
            return subprocess.check_output(["git", *argv], cwd=main, env=env, stderr=subprocess.DEVNULL).strip()

        git("init", "-q")
        git("add", ".")
        git("-c", "user.name=Live proof", "-c", "user.email=proof@example.invalid", "commit", "-qm", "baseline")
        original = (git("rev-parse", "HEAD"), hashlib.sha256((main / ".git/index").read_bytes()).hexdigest())
        config = root / "projects.toml"
        config.write_text("[projects.proof]\nworkspace=" + json.dumps(str(main)) +
                          '\nopencode_url="http://127.0.0.1:49999"\npassword_file=' + json.dumps(str(root / "password")) +
                          '\nmax_rounds=3\nexecution_mode="worktree"\ndelivery_mode="manual"\nallow_parallel_writers=true\nmax_active_tasks=2\nauto_approve_state_directory=true\nopencode_model=' + json.dumps(args.model) + "\n")
        base = [str(args.bridge.resolve()), "--project", "proof", "--config", str(config), "--state-root", str(root / "state")]
        subprocess.run([base[0], "setup", *base[1:]], env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        tasks = []
        selector = selectors.DefaultSelector()
        seq = 0
        with (root / "mcp.log").open("w") as log:
            process = subprocess.Popen([base[0], "mcp", *base[1:]], cwd=main, env=env,
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True)
            selector.register(process.stdout, selectors.EVENT_READ)

            def rpc(method, params=None):
                nonlocal seq
                seq += 1
                process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": seq, "method": method,
                                               "params": params or {}}) + "\n")
                process.stdin.flush()
                if not selector.select(20):
                    raise TimeoutError("rpc")
                response = json.loads(process.stdout.readline())
                assert response["id"] == seq and "error" not in response
                return response["result"]

            def tool(name, arguments):
                return rpc("tools/call", {"name": name, "arguments": arguments})["structuredContent"]

            try:
                report["opencode_version"] = subprocess.check_output([str(args.opencode), "--version"], env=env, text=True).strip()
                rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                   "clientInfo": {"name": "parallel-proof", "version": "1"}})
                process.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
                process.stdin.flush()
                for key in ("left", "right"):
                    submitted = tool("submit_task", {"request_id": key,
                        "task": f"In the exact checkout provided, first run python3 -B rendezvous.py {key}. This intentional local barrier may wait for the other task; wait for it to finish. Then immediately create the NEW relative file {key}.txt with exactly {key} followed by one newline. Do not search for it, inspect parent/external directories or change any other file. Run python3 -B check.py {key}.txt and finish.",
                        "allowed_paths": [key + ".txt"], "test_commands": [f"python3 -B check.py {key}.txt"]})
                    assert "error" not in submitted
                    tasks.append(submitted["task_id"])
                deadline = time.monotonic() + 170
                while len(arrivals) != 2:
                    for task in tasks:
                        status = tool("task_status", {"task_id": task})["status"]
                        if status in ("needs_user", "failed", "delivery_unknown", "closed"):
                            report["blocked_status"] = status
                            raise RuntimeError("task_blocked")
                    if time.monotonic() >= deadline:
                        raise TimeoutError("barrier")
                    time.sleep(0.5)
                refused = tool("submit_task", {"request_id": "overlap", "task": "Do not execute",
                    "allowed_paths": ["left.txt"], "test_commands": ["python3 -B check.py left.txt"]})
                assert "error" in refused
                report["third_overlapping_task_refused"] = True
                release.set()
                for task in tasks:
                    while True:
                        state = tool("task_status", {"task_id": task})
                        if state["status"] == "awaiting_review":
                            assert state["verification"]["status"] == "passed"
                            break
                        if state["status"] in ("needs_user", "failed", "delivery_unknown", "closed"):
                            report["blocked_status"] = state["status"]
                            raise RuntimeError("task_blocked")
                        if time.monotonic() >= deadline:
                            raise TimeoutError("review")
                        time.sleep(0.5)
                    assert tool("accept_task", {"task_id": task})["status"] == "accepted"
                assert len(completions) == 2 and max(arrivals.values()) < min(completions.values())
                report["execution_intervals_overlap"] = True
                assert git("status", "--porcelain") == b""
                assert original == (git("rev-parse", "HEAD"), hashlib.sha256((main / ".git/index").read_bytes()).hexdigest())
                with sqlite3.connect(root / "state/proof/state.sqlite") as db:
                    rounds = db.execute("SELECT COUNT(*),SUM(attempted) FROM rounds").fetchone()
                    assert rounds == (2, 2)
                    assert db.execute("SELECT COUNT(*) FROM active_writers").fetchone()[0] == 0
                    servers = db.execute("SELECT server_port,path FROM worktrees").fetchall()
                    assert len(servers) == 2 and len({r[0] for r in servers}) == 2 and len({r[1] for r in servers}) == 2
                report.update(outcome="passed", rounds=2, attempted=2, reservations_after_accept=0,
                              distinct_checkouts_and_ports=True, main_head_index_content_unchanged=True)
            except Exception as exc:
                report["failure_type"] = type(exc).__name__
            finally:
                release.set()
                for task in tasks:
                    try:
                        tool("close_task", {"task_id": task, "reason": "finish disposable parallel proof"})
                    except Exception:
                        pass
                cleanup_deadline = time.monotonic() + 10
                while tasks and time.monotonic() < cleanup_deadline:
                    try:
                        if all(tool("task_status", {"task_id": task})["status"] in ("closed", "accepted") for task in tasks):
                            break
                    except Exception:
                        break
                    time.sleep(0.1)
                process.stdin.close()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=5)
                selector.close()
                # Signal only identity-checked records in this fresh private state.
                for path in (root / "state/proof/worktrees").glob("*/runtime/opencode.process.json"):
                    try:
                        record = json.loads(path.read_text())
                        pid = record["pid"]
                        fd = os.pidfd_open(pid)
                        try:
                            stat = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
                            assert stat[19] == record["start"] and record["project_id"] == "proof"
                            assert Path(record["checkout"]).is_relative_to(root)
                            assert Path(f"/proc/{pid}/cwd").resolve() == Path(record["checkout"])
                            assert Path("/proc/sys/kernel/random/boot_id").read_text().strip() == record["boot_id"]
                            signal.pidfd_send_signal(fd, signal.SIGTERM)
                            deadline = time.monotonic() + 5
                            while time.monotonic() < deadline:
                                try:
                                    current = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
                                except FileNotFoundError:
                                    break
                                if current[0] in ("Z", "X") or current[19] != record["start"]:
                                    break
                                time.sleep(0.05)
                            else:
                                signal.pidfd_send_signal(fd, signal.SIGKILL)
                        finally:
                            os.close(fd)
                    except (OSError, ValueError, AssertionError, KeyError):
                        pass
                http.shutdown()
                http.server_close()
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report), flush=True)
    return int(report["outcome"] != "passed")


if __name__ == "__main__":
    raise SystemExit(main())
