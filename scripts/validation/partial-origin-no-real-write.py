#!/usr/bin/env python3
"""Validate that partial-origin writes never mutate the real base file."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any, Optional


OUTPUT_TAIL_CHARS = 4000
ONE_MIB = 1024 * 1024


WRITE_WORKLOAD = r'''
import json
import os
import sys
from pathlib import Path

path = Path(sys.argv[1])
offset = int(sys.argv[2])

before = path.stat()
with path.open("r+b", buffering=0) as handle:
    handle.seek(offset)
    old = handle.read(1)
    if not old:
        raise RuntimeError(f"offset {offset} is outside {path}")
    new = bytes([(old[0] + 1) % 256])
    handle.seek(offset)
    handle.write(new)
    handle.flush()
    os.fsync(handle.fileno())

after = path.stat()
print(json.dumps({
    "path": str(path),
    "offset": offset,
    "old_byte": old[0],
    "new_byte": new[0],
    "size_before": before.st_size,
    "size_after": after.st_size,
}, sort_keys=True))
'''


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return parsed


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be > 0")
    return parsed


def env_flag(name: str) -> bool:
    value = os.environ.get(name, "")
    return value.lower() in {"1", "true", "yes", "on"}


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run a partial-origin write through agentfs run and fail if sampled "
            "base-file bytes or stable metadata change."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Fast smoke
  scripts/validation/partial-origin-no-real-write.py --file-size-mib 1 --timeout 60

  # Full gate-sized sample around a 200 MiB base file
  scripts/validation/partial-origin-no-real-write.py --file-size-mib 200 --timeout 180

Environment:
  AGENTFS_BIN      path/name of agentfs executable
  AGENTFS_PROFILE  set to 1 to collect AgentFS profile summaries
""",
    )
    parser.add_argument(
        "--file-size-mib",
        type=positive_int,
        default=positive_int(os.environ.get("NO_REAL_WRITE_FILE_SIZE_MIB", "1")),
        help="base file size in MiB (default: 1)",
    )
    parser.add_argument(
        "--offset",
        type=non_negative_int,
        help="byte offset to edit (default: middle of the file)",
    )
    parser.add_argument(
        "--sample-bytes",
        type=positive_int,
        default=positive_int(os.environ.get("NO_REAL_WRITE_SAMPLE_BYTES", "4096")),
        help="bytes to hash at each sampled range (default: 4096)",
    )
    parser.add_argument(
        "--agentfs-bin",
        default=os.environ.get("AGENTFS_BIN"),
        help="agentfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=positive_float(os.environ.get("NO_REAL_WRITE_TIMEOUT", "120")),
        help="per-command timeout in seconds (default: 120)",
    )
    parser.add_argument(
        "--session",
        default=None,
        help="AgentFS run session id (default: generated unique id)",
    )
    parser.add_argument(
        "--profile",
        action="store_true",
        default=env_flag("AGENTFS_PROFILE"),
        help="enable AGENTFS_PROFILE=1 for AgentFS invocation",
    )
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=env_flag("NO_REAL_WRITE_KEEP_TEMP"),
        help="keep temporary source tree and isolated HOME after the run",
    )
    parser.add_argument(
        "--output",
        help="write JSON result to this file instead of stdout",
    )
    parser.add_argument(
        "--json-indent",
        type=int,
        default=2,
        help="JSON indentation level (default: 2)",
    )
    return parser.parse_args(argv)


def tail_text(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        text = value.decode("utf-8", errors="replace")
    else:
        text = str(value)
    if len(text) <= OUTPUT_TAIL_CHARS:
        return text
    return text[-OUTPUT_TAIL_CHARS:]


def extract_profile_summaries(stderr: Any) -> list[dict[str, Any]]:
    text = tail_text(stderr)
    summaries: list[dict[str, Any]] = []
    for line in text.splitlines():
        line = line.strip()
        if not line or "agentfs_profile_summary" not in line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and value.get("event") == "agentfs_profile_summary":
            summaries.append(value)
    return summaries


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


def run_subprocess(
    argv: list[str],
    cwd: Path,
    env: dict[str, str],
    timeout: float,
) -> dict[str, Any]:
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
            if proc.stdout is not None:
                proc.stdout.close()
            if proc.stderr is not None:
                proc.stderr.close()
            stdout, stderr = "", "process timed out; output pipes were closed after termination"
        timed_out = True

    return {
        "argv": argv,
        "cwd": str(cwd),
        "duration_seconds": time.perf_counter() - started,
        "returncode": proc.returncode,
        "timed_out": timed_out,
        "stdout_tail": tail_text(stdout),
        "stderr_tail": tail_text(stderr),
        "stdout_bytes": len((stdout or "").encode("utf-8", errors="replace")),
        "stderr_bytes": len((stderr or "").encode("utf-8", errors="replace")),
        "profile_summaries": extract_profile_summaries(stderr),
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
        candidate_path = Path(agentfs_bin).expanduser()
        if candidate_path.is_file() and os.access(candidate_path, os.X_OK):
            return str(candidate_path.resolve())
        if os.sep not in agentfs_bin:
            found = shutil.which(agentfs_bin)
            if found:
                return found
        raise RuntimeError(f"configured agentfs executable not found or not executable: {agentfs_bin}")

    for candidate_path in (
        repo_root / "cli" / "target" / "debug" / "agentfs",
        repo_root / "cli" / "target" / "release" / "agentfs",
    ):
        if candidate_path.is_file() and os.access(candidate_path, os.X_OK):
            return str(candidate_path)

    build = subprocess.run(
        ["cargo", "build", "--manifest-path", str(repo_root / "cli" / "Cargo.toml")],
        cwd=str(repo_root / "cli"),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if build.returncode != 0:
        raise RuntimeError(
            "failed to build repo-local agentfs binary; set AGENTFS_BIN to an explicit binary\n"
            f"stdout:\n{tail_text(build.stdout)}\n"
            f"stderr:\n{tail_text(build.stderr)}"
        )

    built = repo_root / "cli" / "target" / "debug" / "agentfs"
    if built.is_file() and os.access(built, os.X_OK):
        return str(built)

    raise RuntimeError(f"repo-local build completed but binary was not found: {built}")


def git_commit(repo_root: Path) -> Optional[str]:
    proc = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=str(repo_root),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    if proc.returncode == 0:
        return proc.stdout.strip()
    return None


def create_large_file(path: Path, size_bytes: int) -> str:
    path.parent.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha256()
    written = 0
    block_index = 0
    with path.open("wb") as handle:
        while written < size_bytes:
            seed = hashlib.sha256(f"agentfs-phase6-no-real-write-{block_index}".encode()).digest()
            block = (seed * ((ONE_MIB // len(seed)) + 1))[: min(ONE_MIB, size_bytes - written)]
            handle.write(block)
            digest.update(block)
            written += len(block)
            block_index += 1
    return digest.hexdigest()


def sample_ranges(size: int, sample_bytes: int, edit_offset: int) -> list[tuple[int, int]]:
    starts = [0, max(0, size // 2 - sample_bytes // 2), max(0, size - sample_bytes)]
    starts.append(max(0, min(edit_offset, max(0, size - sample_bytes))))
    ranges: list[tuple[int, int]] = []
    seen: set[tuple[int, int]] = set()
    for start in starts:
        length = max(0, min(sample_bytes, size - start))
        item = (start, length)
        if length > 0 and item not in seen:
            ranges.append(item)
            seen.add(item)
    return ranges


def sample_base(path: Path, sample_bytes: int, edit_offset: int) -> dict[str, Any]:
    stat = path.stat()
    samples = []
    with path.open("rb") as handle:
        for start, length in sample_ranges(stat.st_size, sample_bytes, edit_offset):
            handle.seek(start)
            data = handle.read(length)
            samples.append(
                {
                    "offset": start,
                    "bytes": len(data),
                    "sha256": hashlib.sha256(data).hexdigest(),
                }
            )
    return {
        "path": str(path),
        "stable_stat": {
            "size": stat.st_size,
            "mode": stat.st_mode,
            "mtime_ns": stat.st_mtime_ns,
        },
        "samples": samples,
    }


def table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
        (name,),
    ).fetchone()
    return row is not None


def inspect_db(db_path: Path) -> dict[str, Any]:
    if not db_path.exists():
        return {"inspectable": False, "reason": "database file does not exist"}

    try:
        conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        conn.execute("PRAGMA query_only = ON")
        try:
            result: dict[str, Any] = {"inspectable": True}
            if table_exists(conn, "fs_data"):
                row = conn.execute(
                    "SELECT COUNT(*), COALESCE(SUM(LENGTH(data)), 0) FROM fs_data"
                ).fetchone()
                result["fs_data_rows"] = int(row[0])
                result["fs_data_bytes"] = int(row[1])
            if table_exists(conn, "fs_inode"):
                row = conn.execute(
                    "SELECT COALESCE(SUM(CASE WHEN storage_kind = 1 THEN LENGTH(data_inline) ELSE 0 END), 0) "
                    "FROM fs_inode"
                ).fetchone()
                result["fs_inline_bytes"] = int(row[0])
            if table_exists(conn, "fs_partial_origin"):
                row = conn.execute("SELECT COUNT(*) FROM fs_partial_origin").fetchone()
                result["fs_partial_origin_rows"] = int(row[0])
            if table_exists(conn, "fs_chunk_override"):
                row = conn.execute("SELECT COUNT(*) FROM fs_chunk_override").fetchone()
                result["fs_chunk_override_rows"] = int(row[0])
            partial_origin_rows = int(result.get("fs_partial_origin_rows", 0) or 0)
            override_rows = int(result.get("fs_chunk_override_rows", 0) or 0)
            result["portability_status"] = {
                "portable": partial_origin_rows == 0,
                "origin_backed": partial_origin_rows > 0,
                "partial_origin_rows": partial_origin_rows,
                "override_rows": override_rows,
                "stored_bytes": int(result.get("fs_data_bytes", 0) or 0)
                + int(result.get("fs_inline_bytes", 0) or 0),
                "materialized_rows": None,
            }
            return result
        finally:
            conn.close()
    except Exception as exc:
        return {"inspectable": False, "reason": str(exc)}


def prepare_environment(temp_root: Path, profile: bool) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    if profile:
        env["AGENTFS_PROFILE"] = "1"

    home = temp_root / "home"
    for path in (home, home / ".config", home / ".cache", home / ".local" / "share"):
        path.mkdir(parents=True, exist_ok=True)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(home / ".config")
    env["XDG_CACHE_HOME"] = str(home / ".cache")
    env["XDG_DATA_HOME"] = str(home / ".local" / "share")

    temp_dir = temp_root / "tmp"
    temp_dir.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(temp_dir)
    env["TMP"] = str(temp_dir)
    env["TEMP"] = str(temp_dir)
    return env


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    file_size_bytes = args.file_size_mib * ONE_MIB
    offset = args.offset if args.offset is not None else file_size_bytes // 2
    if offset >= file_size_bytes:
        raise SystemExit("--offset must be smaller than --file-size-mib bytes")

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        temp_root = Path(tempfile.mkdtemp(prefix="agentfs-no-real-write-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="agentfs-no-real-write-")
        temp_root = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    try:
        agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = prepare_environment(temp_root, args.profile)
        session = args.session or f"no-real-write-{uuid.uuid4()}"
        source_root = temp_root / "base"
        base_file = source_root / "large.bin"
        original_sha = create_large_file(base_file, file_size_bytes)
        before_sample = sample_base(base_file, args.sample_bytes, offset)

        command = [
            agentfs_bin,
            "run",
            "--session",
            session,
            "--no-default-allows",
            "--partial-origin",
            "on",
            "--",
            sys.executable,
            "-c",
            WRITE_WORKLOAD,
            "large.bin",
            str(offset),
        ]
        run = run_subprocess(command, source_root, env, args.timeout)
        after_sample = sample_base(base_file, args.sample_bytes, offset)
        workload_json = parse_json_stdout(run)
        db_path = Path(env["HOME"]) / ".agentfs" / "run" / session / "delta.db"
        db_inspect = inspect_db(db_path)

        base_sample_unchanged = before_sample["samples"] == after_sample["samples"]
        base_metadata_unchanged = before_sample["stable_stat"] == after_sample["stable_stat"]
        correctness = {
            "agentfs_returncode_zero": run["returncode"] == 0,
            "workload_json_present": workload_json is not None,
            "base_sample_unchanged": base_sample_unchanged,
            "base_metadata_unchanged": base_metadata_unchanged,
            "partial_origin_rows_present": int(
                db_inspect.get("portability_status", {}).get("partial_origin_rows", 0) or 0
            )
            > 0,
            "override_rows_present": int(
                db_inspect.get("portability_status", {}).get("override_rows", 0) or 0
            )
            > 0,
        }
        correctness["passed"] = all(correctness.values())
        if not correctness["passed"]:
            exit_code = 1

        result = {
            "schema_version": 1,
            "benchmark": "phase6-partial-origin-no-real-write",
            "git_commit": git_commit(repo_root),
            "parameters": {
                "file_size_bytes": file_size_bytes,
                "file_size_mib": args.file_size_mib,
                "offset": offset,
                "edit_width_bytes": 1,
                "sample_bytes": args.sample_bytes,
            },
            "agentfs": {
                "bin": agentfs_bin,
                "session": session,
                "db_path": str(db_path),
                "profile_enabled": args.profile,
                "partial_origin_cli": "on",
                "partial_origin_enabled": True,
                "profile_summary_count": len(run["profile_summaries"]),
            },
            "database": {
                "inspect_after": db_inspect,
                "portability_status": db_inspect.get("portability_status"),
            },
            "base_file": {
                "path": str(base_file),
                "original_sha256": original_sha,
                "before_sample": before_sample,
                "after_sample": after_sample,
            },
            "agentfs_overlay": {
                "duration_seconds": run["duration_seconds"],
                "run": run,
                "result": workload_json,
            },
            "correctness": correctness,
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
        }
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "phase6-partial-origin-no-real-write",
            "error": str(exc),
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
        }

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(payload, encoding="utf-8")
        print(f"Wrote no-real-write JSON to {args.output}", file=sys.stderr)
    else:
        sys.stdout.write(payload)

    if temp_manager is not None:
        temp_manager.cleanup()

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
