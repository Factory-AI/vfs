#!/usr/bin/env python3
"""Phase 5 large base-file single-byte edit DB-growth benchmark."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sqlite3
import sys
import tempfile
import uuid
from pathlib import Path
from typing import Any, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib.common import (  # noqa: E402
    env_flag,
    git_commit,
    parse_json_stdout,
    positive_float,
    positive_int,
    resolve_vfs_bin,
    run_subprocess,
    sandbox_python,
)


ONE_MIB = 1024 * 1024


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


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return parsed


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Compare a native single-byte edit to the same edit through an "
            "Vfs overlay and report delta DB growth."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Spec-sized copy-up benchmark (200 MiB base file)
  scripts/validation/large-edit-benchmark.py --file-size-mib 200

  # Fast smoke
  scripts/validation/large-edit-benchmark.py --file-size-mib 1 --timeout 60

Environment:
  VFS_BIN      path/name of vfs executable
  VFS_PROFILE  set to 1 to collect Vfs profile summaries
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
        "--vfs-bin",
        default=os.environ.get("VFS_BIN"),
        help="vfs executable path/name (default: repo target binary, building cli if needed)",
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
        help="Vfs run session id (default: generated unique id)",
    )
    parser.add_argument(
        "--profile",
        action="store_true",
        default=env_flag("VFS_PROFILE"),
        help="enable VFS_PROFILE=1 for Vfs invocations",
    )
    partial_origin_group = parser.add_mutually_exclusive_group()
    partial_origin_group.add_argument(
        "--partial-origin",
        dest="partial_origin",
        action="store_true",
        help="pass --partial-origin on to Vfs overlay invocations",
    )
    partial_origin_group.add_argument(
        "--no-partial-origin",
        dest="partial_origin",
        action="store_false",
        help="omit --partial-origin for Vfs overlay invocations",
    )
    parser.set_defaults(partial_origin=False)
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


def create_large_file(path: Path, size_bytes: int) -> str:
    path.parent.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha256()
    written = 0
    block_index = 0
    with path.open("wb") as handle:
        while written < size_bytes:
            seed = hashlib.sha256(f"vfs-phase5-large-edit-{block_index}".encode()).digest()
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


def optional_count(conn: sqlite3.Connection, table_name: str) -> Optional[int]:
    if not table_exists(conn, table_name):
        return None
    row = conn.execute(f"SELECT COUNT(*) FROM {table_name}").fetchone()
    return int(row[0])


def portability_status(inspect: dict[str, Any]) -> dict[str, Any]:
    partial_origin_rows = int(inspect.get("fs_partial_origin_rows", 0) or 0)
    override_rows = int(inspect.get("fs_chunk_override_rows", 0) or 0)
    stored_bytes = int(inspect.get("fs_data_bytes", 0) or 0) + int(
        inspect.get("fs_inline_bytes", 0) or 0
    )
    materialized_rows = inspect.get("fs_materialized_rows")
    return {
        "portable": partial_origin_rows == 0,
        "origin_backed": partial_origin_rows > 0,
        "partial_origin_rows": partial_origin_rows,
        "override_rows": override_rows,
        "stored_bytes": stored_bytes,
        "materialized_rows": materialized_rows,
    }


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
                    "SELECT COUNT(*), "
                    "COALESCE(SUM(CASE WHEN storage_kind = 1 THEN 1 ELSE 0 END), 0), "
                    "COALESCE(SUM(CASE WHEN storage_kind = 1 THEN LENGTH(data_inline) ELSE 0 END), 0) "
                    "FROM fs_inode"
                ).fetchone()
                result["fs_inode_rows"] = int(row[0])
                result["inline_inode_rows"] = int(row[1])
                result["fs_inline_bytes"] = int(row[2])
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
            result["fs_materialized_rows"] = optional_count(conn, "fs_materialized")
            if table_exists(conn, "fs_config"):
                result["fs_config"] = {
                    str(key): str(value)
                    for key, value in conn.execute("SELECT key, value FROM fs_config").fetchall()
                }
            result["portability_status"] = portability_status(result)
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
        env["VFS_PROFILE"] = "1"

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
        temp_root = Path(tempfile.mkdtemp(prefix="vfs-large-edit-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="vfs-large-edit-")
        temp_root = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    try:
        vfs_bin = resolve_vfs_bin(args.vfs_bin, repo_root)
        env = prepare_environment(temp_root, args.profile)
        session = args.session or f"large-edit-{uuid.uuid4()}"

        source_root = temp_root / "source"
        native_root = temp_root / "native"
        vfs_base_root = temp_root / "vfs-base"
        source_file = source_root / "large.bin"
        original_sha = create_large_file(source_file, file_size_bytes)
        copy_base_tree(source_root, native_root)
        copy_base_tree(source_root, vfs_base_root)

        db_path = Path(env["HOME"]) / ".vfs" / "run" / session / "delta.db"
        vfs_run_prefix = [
            vfs_bin,
            "run",
            "--session",
            session,
            "--no-default-allows",
        ]
        if args.partial_origin:
            vfs_run_prefix.extend(["--partial-origin", "on"])
        warmup_command = [
            *vfs_run_prefix,
            "--",
            sandbox_python(),
            "-c",
            READONLY_WARMUP,
        ]
        warmup = run_subprocess(warmup_command, vfs_base_root, env, args.timeout)
        db_before = db_artifacts(db_path)
        inspect_before = inspect_db(db_path)

        native_command = [sandbox_python(), "-c", EDIT_WORKLOAD, "large.bin", str(offset)]
        vfs_command = [*vfs_run_prefix, "--"] + native_command

        native = run_subprocess(native_command, native_root, env, args.timeout)
        vfs = run_subprocess(vfs_command, vfs_base_root, env, args.timeout)

        db_after = db_artifacts(db_path)
        inspect_after = inspect_db(db_path)

        native_json = parse_json_stdout(native)
        vfs_json = parse_json_stdout(vfs)
        vfs_base_sha_after = hash_file(vfs_base_root / "large.bin")
        native_sha_after = hash_file(native_root / "large.bin")
        comparable_fields = ("size", "size_before", "offset", "old_byte", "new_byte", "sha256")
        outputs_match = (
            native_json is not None
            and vfs_json is not None
            and all(native_json.get(field) == vfs_json.get(field) for field in comparable_fields)
        )
        correctness = {
            "native_returncode_zero": native["returncode"] == 0,
            "vfs_returncode_zero": vfs["returncode"] == 0,
            "warmup_returncode_zero": warmup["returncode"] == 0,
            "outputs_match": outputs_match,
            "vfs_base_unchanged": vfs_base_sha_after == original_sha,
            "native_file_changed": native_sha_after != original_sha,
            "passed": (
                warmup["returncode"] == 0
                and native["returncode"] == 0
                and vfs["returncode"] == 0
                and outputs_match
                and vfs_base_sha_after == original_sha
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
            "vfs": {
                "bin": vfs_bin,
                "session": session,
                "db_path": str(db_path),
                "profile_enabled": args.profile,
                "partial_origin_enabled": args.partial_origin,
                "partial_origin_cli": "on" if args.partial_origin else "omitted",
                "profile_summary_count": len(warmup["profile_summaries"]) + len(vfs["profile_summaries"]),
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
            "vfs_overlay": {
                "duration_seconds": vfs["duration_seconds"],
                "warmup": warmup,
                "run": vfs,
                "result": vfs_json,
            },
            "base_file": {
                "original_sha256": original_sha,
                "native_sha256_after": native_sha_after,
                "vfs_base_sha256_after": vfs_base_sha_after,
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
