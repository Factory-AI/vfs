#!/usr/bin/env python3
"""Phase 5 large base-file single-byte edit DB-growth benchmark."""

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
PARTIAL_ORIGIN_ENV = "AGENTFS_OVERLAY_PARTIAL_ORIGIN"


EDIT_WORKLOAD = r'''
import hashlib
import json
import os
import sys
from pathlib import Path

path = Path(sys.argv[1])
offset = int(sys.argv[2])

before_size = path.stat().st_size
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

digest = hashlib.sha256()
with path.open("rb") as handle:
    while True:
        chunk = handle.read(1024 * 1024)
        if not chunk:
            break
        digest.update(chunk)

print(json.dumps({
    "path": str(path),
    "size": path.stat().st_size,
    "size_before": before_size,
    "offset": offset,
    "old_byte": old[0],
    "new_byte": new[0],
    "sha256": digest.hexdigest(),
}, sort_keys=True))
'''


READONLY_WARMUP = r'''
import json
from pathlib import Path

root = Path(".")
entries = sorted(path.name for path in root.iterdir())

print(json.dumps({
    "path": str(root),
    "entries": entries,
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
            "Compare a native single-byte edit to the same edit through an "
            "AgentFS overlay and report delta DB growth."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Spec-sized copy-up benchmark (200 MiB base file)
  scripts/validation/large-edit-benchmark.py --file-size-mib 200

  # Fast smoke
  scripts/validation/large-edit-benchmark.py --file-size-mib 1 --timeout 60

Environment:
  AGENTFS_BIN      path/name of agentfs executable
  AGENTFS_PROFILE  set to 1 to collect AgentFS profile summaries
  AGENTFS_OVERLAY_PARTIAL_ORIGIN
                   set to 1 to enable experimental partial-origin copy-up
""",
    )
    parser.add_argument(
        "--file-size-mib",
        type=positive_int,
        default=positive_int(os.environ.get("LARGE_EDIT_FILE_SIZE_MIB", "200")),
        help="base file size in MiB (default: 200)",
    )
    parser.add_argument(
        "--offset",
        type=non_negative_int,
        help="byte offset to edit (default: middle of the file)",
    )
    parser.add_argument(
        "--agentfs-bin",
        default=os.environ.get("AGENTFS_BIN"),
        help="agentfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=positive_float(os.environ.get("LARGE_EDIT_TIMEOUT", "180")),
        help="per-command timeout in seconds (default: 180)",
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
        help="enable AGENTFS_PROFILE=1 for AgentFS invocations",
    )
    partial_origin_default = env_flag(PARTIAL_ORIGIN_ENV)
    partial_origin_group = parser.add_mutually_exclusive_group()
    partial_origin_group.add_argument(
        "--partial-origin",
        dest="partial_origin",
        action="store_true",
        help=f"enable {PARTIAL_ORIGIN_ENV}=1 for AgentFS overlay invocations",
    )
    partial_origin_group.add_argument(
        "--no-partial-origin",
        dest="partial_origin",
        action="store_false",
        help=f"disable {PARTIAL_ORIGIN_ENV} for AgentFS overlay invocations",
    )
    parser.set_defaults(partial_origin=partial_origin_default)
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=env_flag("LARGE_EDIT_KEEP_TEMP"),
        help="keep temporary native/base trees and isolated HOME after the run",
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
    if stderr is None:
        return []
    if isinstance(stderr, bytes):
        text = stderr.decode("utf-8", errors="replace")
    else:
        text = str(stderr)

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
            seed = hashlib.sha256(f"agentfs-phase5-large-edit-{block_index}".encode()).digest()
            block = (seed * ((ONE_MIB // len(seed)) + 1))[: min(ONE_MIB, size_bytes - written)]
            handle.write(block)
            digest.update(block)
            written += len(block)
            block_index += 1
    return digest.hexdigest()


def copy_base_tree(source: Path, destination: Path) -> None:
    shutil.copytree(source, destination, symlinks=True)


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(ONE_MIB)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def parse_json_stdout(run: dict[str, Any]) -> Optional[dict[str, Any]]:
    for line in reversed(run["stdout_tail"].splitlines()):
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


def db_artifacts(db_path: Path) -> dict[str, Any]:
    artifacts = []
    total = 0
    for path in (db_path, db_path.with_name(db_path.name + "-wal"), db_path.with_name(db_path.name + "-shm")):
        if path.exists():
            size = path.stat().st_size
            artifacts.append({"path": str(path), "bytes": size})
            total += size
    return {"path": str(db_path), "total_bytes": total, "artifacts": artifacts}


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
                    "SELECT COUNT(*), COALESCE(SUM(CASE WHEN storage_kind = 1 THEN 1 ELSE 0 END), 0) FROM fs_inode"
                ).fetchone()
                result["fs_inode_rows"] = int(row[0])
                result["inline_inode_rows"] = int(row[1])
            if table_exists(conn, "fs_origin"):
                row = conn.execute("SELECT COUNT(*) FROM fs_origin").fetchone()
                result["fs_origin_rows"] = int(row[0])
            if table_exists(conn, "fs_partial_origin"):
                row = conn.execute("SELECT COUNT(*) FROM fs_partial_origin").fetchone()
                result["fs_partial_origin_rows"] = int(row[0])
            if table_exists(conn, "fs_origin_v2"):
                row = conn.execute("SELECT COUNT(*) FROM fs_origin_v2").fetchone()
                result["fs_origin_v2_rows"] = int(row[0])
            if table_exists(conn, "fs_chunk_override"):
                row = conn.execute("SELECT COUNT(*) FROM fs_chunk_override").fetchone()
                result["fs_chunk_override_rows"] = int(row[0])
            if table_exists(conn, "fs_config"):
                result["fs_config"] = {
                    str(key): str(value)
                    for key, value in conn.execute("SELECT key, value FROM fs_config").fetchall()
                }
            return result
        finally:
            conn.close()
    except Exception as exc:
        return {"inspectable": False, "reason": str(exc)}


def prepare_environment(temp_root: Path, profile: bool, partial_origin: bool) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    if profile:
        env["AGENTFS_PROFILE"] = "1"
    if partial_origin:
        env[PARTIAL_ORIGIN_ENV] = "1"
    else:
        env.pop(PARTIAL_ORIGIN_ENV, None)

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
        temp_root = Path(tempfile.mkdtemp(prefix="agentfs-large-edit-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="agentfs-large-edit-")
        temp_root = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    try:
        agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = prepare_environment(temp_root, args.profile, args.partial_origin)
        session = args.session or f"large-edit-{uuid.uuid4()}"

        source_root = temp_root / "source"
        native_root = temp_root / "native"
        agentfs_base_root = temp_root / "agentfs-base"
        source_file = source_root / "large.bin"
        original_sha = create_large_file(source_file, file_size_bytes)
        copy_base_tree(source_root, native_root)
        copy_base_tree(source_root, agentfs_base_root)

        db_path = Path(env["HOME"]) / ".agentfs" / "run" / session / "delta.db"
        warmup_command = [
            agentfs_bin,
            "run",
            "--session",
            session,
            "--no-default-allows",
            "--",
            sys.executable,
            "-c",
            READONLY_WARMUP,
        ]
        warmup = run_subprocess(warmup_command, agentfs_base_root, env, args.timeout)
        db_before = db_artifacts(db_path)
        inspect_before = inspect_db(db_path)

        native_command = [sys.executable, "-c", EDIT_WORKLOAD, "large.bin", str(offset)]
        agentfs_command = [
            agentfs_bin,
            "run",
            "--session",
            session,
            "--no-default-allows",
            "--",
        ] + native_command

        native = run_subprocess(native_command, native_root, env, args.timeout)
        agentfs = run_subprocess(agentfs_command, agentfs_base_root, env, args.timeout)

        db_after = db_artifacts(db_path)
        inspect_after = inspect_db(db_path)

        native_json = parse_json_stdout(native)
        agentfs_json = parse_json_stdout(agentfs)
        agentfs_base_sha_after = hash_file(agentfs_base_root / "large.bin")
        native_sha_after = hash_file(native_root / "large.bin")
        comparable_fields = ("size", "size_before", "offset", "old_byte", "new_byte", "sha256")
        outputs_match = (
            native_json is not None
            and agentfs_json is not None
            and all(native_json.get(field) == agentfs_json.get(field) for field in comparable_fields)
        )
        correctness = {
            "native_returncode_zero": native["returncode"] == 0,
            "agentfs_returncode_zero": agentfs["returncode"] == 0,
            "warmup_returncode_zero": warmup["returncode"] == 0,
            "outputs_match": outputs_match,
            "agentfs_base_unchanged": agentfs_base_sha_after == original_sha,
            "native_file_changed": native_sha_after != original_sha,
            "passed": (
                warmup["returncode"] == 0
                and native["returncode"] == 0
                and agentfs["returncode"] == 0
                and outputs_match
                and agentfs_base_sha_after == original_sha
                and native_sha_after != original_sha
            ),
        }
        if not correctness["passed"]:
            exit_code = 1

        result = {
            "schema_version": 1,
            "benchmark": "phase5-large-base-single-byte-edit",
            "git_commit": git_commit(repo_root),
            "parameters": {
                "file_size_bytes": file_size_bytes,
                "file_size_mib": args.file_size_mib,
                "offset": offset,
                "edit_width_bytes": 1,
            },
            "agentfs": {
                "bin": agentfs_bin,
                "session": session,
                "db_path": str(db_path),
                "profile_enabled": args.profile,
                "partial_origin_enabled": args.partial_origin,
                "env_flags": {
                    PARTIAL_ORIGIN_ENV: env.get(PARTIAL_ORIGIN_ENV),
                },
                "profile_summary_count": len(warmup["profile_summaries"]) + len(agentfs["profile_summaries"]),
            },
            "database": {
                "before_edit": db_before,
                "after_edit": db_after,
                "growth_bytes": db_after["total_bytes"] - db_before["total_bytes"],
                "inspect_before": inspect_before,
                "inspect_after": inspect_after,
            },
            "native": {
                "duration_seconds": native["duration_seconds"],
                "run": native,
                "result": native_json,
            },
            "agentfs_overlay": {
                "duration_seconds": agentfs["duration_seconds"],
                "warmup": warmup,
                "run": agentfs,
                "result": agentfs_json,
            },
            "base_file": {
                "original_sha256": original_sha,
                "native_sha256_after": native_sha_after,
                "agentfs_base_sha256_after": agentfs_base_sha_after,
            },
            "correctness": correctness,
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
        }
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "phase5-large-base-single-byte-edit",
            "error": str(exc),
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
        }

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(payload, encoding="utf-8")
        print(f"Wrote large edit benchmark JSON to {args.output}", file=sys.stderr)
    else:
        sys.stdout.write(payload)

    if temp_manager is not None:
        temp_manager.cleanup()

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
