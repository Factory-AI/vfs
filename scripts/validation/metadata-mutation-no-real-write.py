#!/usr/bin/env python3
"""Validate that metadata-class mutations never touch the real base tree.

Exercises create / overwrite / truncate / rename / unlink / chmod / utimens and a
concurrent read-after-write through an AgentFS mount, then asserts:

  1. the host base tree is byte- and metadata-identical before and after the run
     (every mutation must land in the single delta database, never the base);
  2. a fresh AgentFS run over the same session database reproduces every mutation
     (proving the virtual state is fully persisted in the single file, with no
     hidden host-side state).

This complements partial-origin-no-real-write.py (which covers in-place writes to
a large base file) with the discrete metadata operations relevant to the
metadata-reduction and io_uring work.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any, Optional


OUTPUT_TAIL_CHARS = 4000

# Operates inside the mount (cwd == base tree root). Performs each mutation class
# and prints a JSON object describing what it observed through the mount.
MUTATION_WORKLOAD = r'''
import json
import os
import sys
import threading
import time
from pathlib import Path

root = Path.cwd()
obs = {}


def read_text(rel):
    return (root / rel).read_text(encoding="utf-8")


# create
created = root / "created.txt"
created.write_text("created-payload\n", encoding="utf-8")
obs["create"] = {"exists": created.exists(), "content": read_text("created.txt")}

# overwrite
overwrite = root / "overwrite.txt"
overwrite.write_text("overwritten-payload\n", encoding="utf-8")
obs["overwrite"] = {"content": read_text("overwrite.txt")}

# truncate
trunc = root / "truncate.txt"
with trunc.open("r+b") as handle:
    handle.truncate(4)
obs["truncate"] = {"size": trunc.stat().st_size}

# rename
src = root / "rename_src.txt"
dst = root / "rename_dst.txt"
os.rename(src, dst)
obs["rename"] = {
    "src_exists": src.exists(),
    "dst_exists": dst.exists(),
    "dst_content": read_text("rename_dst.txt"),
}

# unlink
unlink = root / "unlink.txt"
os.unlink(unlink)
obs["unlink"] = {"exists": unlink.exists()}

# chmod
chmod = root / "chmod.txt"
os.chmod(chmod, 0o640)
obs["chmod"] = {"mode": oct(chmod.stat().st_mode & 0o777)}

# utimens
utimes = root / "utimes.txt"
target = 1_400_000_000
os.utime(utimes, (target, target))
obs["utimens"] = {"mtime": int(utimes.stat().st_mtime)}

# concurrent read-after-write
concurrent = root / "concurrent.txt"
final_payload = "concurrent-final\n"
errors = []


def writer():
    for i in range(50):
        concurrent.write_text(f"concurrent-{i}\n", encoding="utf-8")
    concurrent.write_text(final_payload, encoding="utf-8")


def reader():
    for _ in range(50):
        try:
            concurrent.read_text(encoding="utf-8")
        except Exception as exc:  # noqa: BLE001
            errors.append(str(exc))
        time.sleep(0.001)


tw = threading.Thread(target=writer)
tr = threading.Thread(target=reader)
tw.start()
tr.start()
tw.join()
tr.join()
obs["concurrent"] = {
    "final_content": read_text("concurrent.txt"),
    "expected": final_payload,
    "reader_errors": errors,
}

print(json.dumps(obs, sort_keys=True))
'''


# Runs on remount (cwd == base tree root). Reads back the virtual state and prints
# a JSON object so the harness can confirm the single-file DB reproduced it.
VERIFY_WORKLOAD = r'''
import json
import os
from pathlib import Path

root = Path.cwd()
out = {}


def maybe_read(rel):
    path = root / rel
    if not path.exists():
        return None
    return path.read_text(encoding="utf-8")


out["created_content"] = maybe_read("created.txt")
out["overwrite_content"] = maybe_read("overwrite.txt")
trunc = root / "truncate.txt"
out["truncate_size"] = trunc.stat().st_size if trunc.exists() else None
out["rename_src_exists"] = (root / "rename_src.txt").exists()
out["rename_dst_content"] = maybe_read("rename_dst.txt")
out["unlink_exists"] = (root / "unlink.txt").exists()
chmod = root / "chmod.txt"
out["chmod_mode"] = oct(chmod.stat().st_mode & 0o777) if chmod.exists() else None
utimes = root / "utimes.txt"
out["utimens_mtime"] = int(utimes.stat().st_mtime) if utimes.exists() else None
out["concurrent_content"] = maybe_read("concurrent.txt")

print(json.dumps(out, sort_keys=True))
'''


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be > 0")
    return parsed


def env_flag(name: str) -> bool:
    return os.environ.get(name, "").lower() in {"1", "true", "yes", "on"}


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agentfs-bin", default=os.environ.get("AGENTFS_BIN"))
    parser.add_argument("--session", default=None)
    parser.add_argument("--timeout", type=positive_float, default=120.0)
    parser.add_argument("--output", default=None)
    parser.add_argument("--json-indent", type=int, default=2)
    parser.add_argument(
        "--profile",
        dest="profile",
        action="store_true",
        default=env_flag("AGENTFS_PROFILE"),
    )
    parser.add_argument("--keep-temp", action="store_true", default=env_flag("KEEP_TEMP"))
    return parser.parse_args(argv)


def tail_text(value: Any) -> str:
    if value is None:
        return ""
    text = value.decode("utf-8", errors="replace") if isinstance(value, bytes) else str(value)
    return text if len(text) <= OUTPUT_TAIL_CHARS else text[-OUTPUT_TAIL_CHARS:]


def terminate_process_tree(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is not None:
        return
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    except Exception:
        proc.terminate()
    try:
        proc.wait(timeout=5)
        return
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        return
    except Exception:
        proc.kill()


def run_subprocess(argv: list[str], cwd: Path, env: dict[str, str], timeout: float) -> dict[str, Any]:
    started = time.perf_counter()
    proc = subprocess.Popen(
        argv,
        cwd=str(cwd),
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        stdout, stderr = proc.communicate(timeout=timeout)
        timed_out = False
    except subprocess.TimeoutExpired:
        terminate_process_tree(proc)
        try:
            stdout, stderr = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            stdout, stderr = "", "process timed out"
        timed_out = True
    return {
        "argv": argv,
        "returncode": proc.returncode,
        "timed_out": timed_out,
        "duration_seconds": time.perf_counter() - started,
        "stdout_tail": tail_text(stdout),
        "stderr_tail": tail_text(stderr),
    }


def parse_json_stdout(run: dict[str, Any]) -> Optional[dict[str, Any]]:
    for line in reversed(run.get("stdout_tail", "").splitlines()):
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            return value
    return None


def resolve_agentfs_bin(agentfs_bin: Optional[str], repo_root: Path) -> str:
    if agentfs_bin:
        candidate = Path(agentfs_bin).expanduser()
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate.resolve())
        if os.sep not in agentfs_bin:
            found = shutil.which(agentfs_bin)
            if found:
                return found
        raise RuntimeError(f"agentfs binary not found or not executable: {agentfs_bin}")
    for candidate in (
        repo_root / "cli" / "target" / "release" / "agentfs",
        repo_root / "cli" / "target" / "debug" / "agentfs",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    raise RuntimeError("no agentfs binary found; pass --agentfs-bin or set AGENTFS_BIN")


def prepare_environment(temp_root: Path, profile: bool) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    if profile:
        env["AGENTFS_PROFILE"] = "1"
    else:
        env.pop("AGENTFS_PROFILE", None)
    home = temp_root / "home"
    for path in (home, home / ".config", home / ".cache", home / ".local" / "share"):
        path.mkdir(parents=True, exist_ok=True)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(home / ".config")
    env["XDG_CACHE_HOME"] = str(home / ".cache")
    env["XDG_DATA_HOME"] = str(home / ".local" / "share")
    tmp = temp_root / "tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(tmp)
    return env


def build_base_tree(root: Path) -> None:
    root.mkdir(parents=True, exist_ok=True)
    files = {
        "overwrite.txt": "original-overwrite\n",
        "truncate.txt": "0123456789abcdef\n",
        "rename_src.txt": "rename-payload\n",
        "unlink.txt": "unlink-payload\n",
        "chmod.txt": "chmod-payload\n",
        "utimes.txt": "utimes-payload\n",
        "concurrent.txt": "concurrent-initial\n",
    }
    for name, content in files.items():
        (root / name).write_text(content, encoding="utf-8")
    os.chmod(root / "chmod.txt", 0o644)
    os.utime(root / "utimes.txt", (1_300_000_000, 1_300_000_000))


def tree_hash(root: Path) -> dict[str, Any]:
    """Hash content and stable metadata for every entry under root (host side)."""
    digest = hashlib.sha256()
    file_count = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(filenames):
            path = Path(dirpath) / name
            rel = path.relative_to(root).as_posix()
            stat = path.lstat()
            digest.update(rel.encode("utf-8"))
            digest.update(b"\0")
            digest.update(
                f"{stat.st_mode}:{stat.st_size}:{stat.st_mtime_ns}".encode("utf-8")
            )
            digest.update(b"\0")
            with path.open("rb") as handle:
                digest.update(handle.read())
            digest.update(b"\0")
            file_count += 1
    return {"sha256": digest.hexdigest(), "file_count": file_count}


def agentfs_run_command(agentfs_bin: str, session: str, workload: str) -> list[str]:
    return [
        agentfs_bin,
        "run",
        "--session",
        session,
        "--no-default-allows",
        "--",
        sys.executable,
        "-c",
        workload,
    ]


def evaluate(mutation: Optional[dict[str, Any]], verify: Optional[dict[str, Any]]) -> dict[str, Any]:
    checks: dict[str, Any] = {}
    if isinstance(mutation, dict):
        checks["mutation_create"] = mutation.get("create", {}).get("exists") is True
        checks["mutation_overwrite"] = (
            mutation.get("overwrite", {}).get("content") == "overwritten-payload\n"
        )
        checks["mutation_truncate"] = mutation.get("truncate", {}).get("size") == 4
        rename = mutation.get("rename", {})
        checks["mutation_rename"] = (
            rename.get("src_exists") is False and rename.get("dst_exists") is True
        )
        checks["mutation_unlink"] = mutation.get("unlink", {}).get("exists") is False
        checks["mutation_chmod"] = mutation.get("chmod", {}).get("mode") == "0o640"
        checks["mutation_utimens"] = mutation.get("utimens", {}).get("mtime") == 1_400_000_000
        concurrent = mutation.get("concurrent", {})
        checks["mutation_concurrent"] = (
            concurrent.get("final_content") == concurrent.get("expected")
            and not concurrent.get("reader_errors")
        )
    else:
        checks["mutation_json_present"] = False

    if isinstance(verify, dict):
        checks["remount_create"] = verify.get("created_content") == "created-payload\n"
        checks["remount_overwrite"] = verify.get("overwrite_content") == "overwritten-payload\n"
        checks["remount_truncate"] = verify.get("truncate_size") == 4
        checks["remount_rename"] = (
            verify.get("rename_src_exists") is False
            and verify.get("rename_dst_content") == "rename-payload\n"
        )
        checks["remount_unlink"] = verify.get("unlink_exists") is False
        checks["remount_chmod"] = verify.get("chmod_mode") == "0o640"
        checks["remount_utimens"] = verify.get("utimens_mtime") == 1_400_000_000
        checks["remount_concurrent"] = verify.get("concurrent_content") == "concurrent-final\n"
    else:
        checks["remount_json_present"] = False
    return checks


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]

    if args.keep_temp:
        temp_root = Path(tempfile.mkdtemp(prefix="agentfs-mutation-no-real-write-"))
        temp_manager = None
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="agentfs-mutation-no-real-write-")
        temp_root = Path(temp_manager.name)

    exit_code = 0
    try:
        agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = prepare_environment(temp_root, args.profile)
        session = args.session or f"mutation-no-real-write-{uuid.uuid4().hex}"
        base_root = temp_root / "base"
        build_base_tree(base_root)

        before = tree_hash(base_root)
        mutation_run = run_subprocess(
            agentfs_run_command(agentfs_bin, session, MUTATION_WORKLOAD),
            base_root,
            env,
            args.timeout,
        )
        after = tree_hash(base_root)
        verify_run = run_subprocess(
            agentfs_run_command(agentfs_bin, session, VERIFY_WORKLOAD),
            base_root,
            env,
            args.timeout,
        )
        after_remount = tree_hash(base_root)

        mutation_json = parse_json_stdout(mutation_run)
        verify_json = parse_json_stdout(verify_run)
        db_path = Path(env["HOME"]) / ".agentfs" / "run" / session / "delta.db"

        checks = evaluate(mutation_json, verify_json)
        checks["agentfs_mutation_rc_zero"] = mutation_run["returncode"] == 0
        checks["agentfs_verify_rc_zero"] = verify_run["returncode"] == 0
        checks["base_unchanged_after_mutation"] = before["sha256"] == after["sha256"]
        checks["base_unchanged_after_remount"] = before["sha256"] == after_remount["sha256"]
        passed = all(bool(v) for v in checks.values())
        if not passed:
            exit_code = 1

        result = {
            "schema_version": 1,
            "benchmark": "metadata-mutation-no-real-write",
            "agentfs": {
                "bin": agentfs_bin,
                "session": session,
                "db_path": str(db_path),
                "profile_enabled": args.profile,
            },
            "base_tree": {
                "before": before,
                "after_mutation": after,
                "after_remount": after_remount,
            },
            "mutation_run": {
                "returncode": mutation_run["returncode"],
                "duration_seconds": mutation_run["duration_seconds"],
                "stderr_tail": mutation_run["stderr_tail"],
                "result": mutation_json,
            },
            "verify_run": {
                "returncode": verify_run["returncode"],
                "duration_seconds": verify_run["duration_seconds"],
                "stderr_tail": verify_run["stderr_tail"],
                "result": verify_json,
            },
            "checks": checks,
            "passed": passed,
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
        }
    except Exception as exc:  # noqa: BLE001
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "metadata-mutation-no-real-write",
            "error": str(exc),
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "passed": False,
        }

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(payload, encoding="utf-8")
        print(f"Wrote mutation-no-real-write JSON to {args.output}", file=sys.stderr)
    else:
        sys.stdout.write(payload)

    if temp_manager is not None:
        temp_manager.cleanup()
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
