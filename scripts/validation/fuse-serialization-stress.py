#!/usr/bin/env python3
"""Low-memory concurrent read stress for Phase 6.5 FUSE serialization profiling."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shlex
import shutil
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib.common import (  # noqa: E402
    env_flag,
    parse_json_stdout,
    positive_float,
    positive_int,
    resolve_vfs_bin,
    run_subprocess,
)


CONCURRENT_READ_WORKLOAD = r'''
import argparse
import hashlib
import json
import os
import threading
import time
from pathlib import Path


def positive_int(value):
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return parsed


parser = argparse.ArgumentParser()
parser.add_argument("--threads", type=positive_int, required=True)
parser.add_argument("--iterations", type=positive_int, required=True)
parser.add_argument("--read-bytes", type=positive_int, required=True)
args = parser.parse_args()

root = Path.cwd()
files = sorted(
    path
    for path in root.rglob("*")
    if path.is_file() and ".vfs" not in path.relative_to(root).parts
)
if not files:
    raise SystemExit("fixture has no files")

started = time.perf_counter()
results = [None] * args.threads


def worker(thread_index):
    digest = hashlib.sha256()
    stat_calls = 0
    open_read_calls = 0
    open_read_bytes = 0
    for iteration in range(args.iterations):
        path = files[(thread_index + iteration) % len(files)]
        rel = path.relative_to(root).as_posix()
        stat_result = os.stat(path)
        with path.open("rb") as handle:
            data = handle.read(args.read_bytes)
        digest.update(f"{thread_index}:{iteration}:{rel}:{stat_result.st_size}:".encode("utf-8"))
        digest.update(data)
        stat_calls += 1
        open_read_calls += 1
        open_read_bytes += len(data)
    results[thread_index] = {
        "digest": digest.hexdigest(),
        "stat_calls": stat_calls,
        "open_read_calls": open_read_calls,
        "open_read_bytes": open_read_bytes,
    }


threads = [threading.Thread(target=worker, args=(index,)) for index in range(args.threads)]
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()

combined = hashlib.sha256()
counts = {"stat_calls": 0, "open_read_calls": 0, "open_read_bytes": 0}
for item in results:
    combined.update(item["digest"].encode("ascii"))
    for key in counts:
        counts[key] += item[key]

print(json.dumps({
    "digest": combined.hexdigest(),
    "total_seconds": time.perf_counter() - started,
    "counts": counts,
    "parameters": {
        "threads": args.threads,
        "iterations": args.iterations,
        "read_bytes": args.read_bytes,
        "file_count": len(files),
    },
}, sort_keys=True))
'''


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run a tiny native-vs-Vfs threaded read workload and capture "
            "FUSE read/write lane and adapter lock profile counters."
        )
    )
    parser.add_argument("--files", type=positive_int, default=8, help="fixture file count")
    parser.add_argument(
        "--file-size-bytes",
        type=positive_int,
        default=4096,
        help="bytes per fixture file",
    )
    parser.add_argument("--threads", type=positive_int, default=4, help="reader thread count")
    parser.add_argument(
        "--iterations",
        type=positive_int,
        default=50,
        help="read/stat iterations per thread",
    )
    parser.add_argument(
        "--read-bytes",
        type=positive_int,
        default=1024,
        help="bytes read per open/read/close operation",
    )
    parser.add_argument(
        "--vfs-bin",
        default=os.environ.get("VFS_BIN"),
        help="vfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=positive_float(os.environ.get("FUSE_SERIALIZATION_STRESS_TIMEOUT", "90")),
        help="per-command timeout in seconds",
    )
    parser.add_argument(
        "--profile",
        action="store_true",
        default=True,
        help="enable VFS_PROFILE=1 for Vfs invocation (default: enabled)",
    )
    parser.add_argument("--session", default=f"fuse-serialization-{uuid.uuid4().hex}")
    parser.add_argument("--output", help="write JSON result to this file")
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=env_flag("FUSE_SERIALIZATION_STRESS_KEEP_TEMP"),
        help="keep temporary fixture and isolated HOME",
    )
    parser.add_argument("--json-indent", type=int, default=2)
    return parser.parse_args(argv)


def max_profile_counters(summaries: list[dict[str, Any]]) -> dict[str, int]:
    counters: dict[str, int] = {}
    for summary in summaries:
        value = summary.get("counters")
        if not isinstance(value, dict):
            continue
        for key, item in value.items():
            if isinstance(item, int):
                counters[key] = max(counters.get(key, 0), item)
    return counters


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
    return env


def create_fixture(root: Path, files: int, file_size: int) -> None:
    root.mkdir(parents=True, exist_ok=True)
    for index in range(files):
        seed = hashlib.sha256(f"vfs-phase65-serialization-{index}".encode()).digest()
        data = (seed * ((file_size // len(seed)) + 1))[:file_size]
        (root / f"file_{index:04d}.dat").write_bytes(data)


def workload_argv(args: argparse.Namespace, workload_script: Path) -> list[str]:
    return [
        sys.executable,
        str(workload_script),
        "--threads",
        str(args.threads),
        "--iterations",
        str(args.iterations),
        "--read-bytes",
        str(args.read_bytes),
    ]


def default_output_path() -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.gettempdir()) / f"vfs-fuse-serialization-stress-{stamp}.json"


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    temp_root = Path(tempfile.mkdtemp(prefix="vfs-fuse-serialization-stress-"))
    if not args.keep_temp:
        temp_manager = tempfile.TemporaryDirectory(prefix="vfs-fuse-serialization-stress-home-")

    output_path = Path(args.output).expanduser() if args.output else default_output_path()
    exit_code = 0
    try:
        vfs_bin = resolve_vfs_bin(args.vfs_bin, repo_root)
        env_root = Path(temp_manager.name) if temp_manager is not None else temp_root
        env = prepare_environment(env_root, args.profile)
        native_root = temp_root / "native"
        vfs_root = temp_root / "vfs"
        workload_script = temp_root / "concurrent_read_workload.py"
        workload_script.write_text(CONCURRENT_READ_WORKLOAD, encoding="utf-8")
        create_fixture(native_root, args.files, args.file_size_bytes)
        shutil.copytree(native_root, vfs_root)

        workload = workload_argv(args, workload_script)
        vfs_command = " ".join(shlex.quote(part) for part in workload)
        vfs_argv = [
            vfs_bin,
            "init",
            "--force",
            "--base",
            str(vfs_root),
            "--backend",
            "fuse",
            "--command",
            vfs_command,
            args.session,
        ]
        native_run = run_subprocess(workload, native_root, env, args.timeout)
        vfs_run = run_subprocess(vfs_argv, vfs_root, env, args.timeout)
        native_workload = parse_json_stdout(native_run)
        vfs_workload = parse_json_stdout(vfs_run)
        profile_counters = max_profile_counters(vfs_run["profile_summaries"])
        profile_counters_present = (
            len(vfs_run["profile_summaries"]) > 0
            and "fuse_adapter_lock_wait_count" in profile_counters
            and "fuse_adapter_lock_wait_nanos" in profile_counters
            and "fuse_read_lane_wait_count" in profile_counters
            and "fuse_read_lane_wait_nanos" in profile_counters
            and "fuse_write_lane_wait_count" in profile_counters
            and "fuse_write_lane_wait_nanos" in profile_counters
            and "fuse_read_lane_max_concurrent" in profile_counters
            and "fuse_exclusive_fallback_count" in profile_counters
        )
        wait_count = profile_counters.get("fuse_adapter_lock_wait_count", 0)
        wait_nanos = profile_counters.get("fuse_adapter_lock_wait_nanos", 0)
        read_lane_wait_count = profile_counters.get("fuse_read_lane_wait_count", 0)
        read_lane_wait_nanos = profile_counters.get("fuse_read_lane_wait_nanos", 0)
        exclusive_fallback_count = profile_counters.get("fuse_exclusive_fallback_count", 0)
        equivalent = (
            native_workload is not None
            and vfs_workload is not None
            and native_workload.get("digest") == vfs_workload.get("digest")
            and native_workload.get("counts") == vfs_workload.get("counts")
        )
        if native_run["returncode"] != 0 or vfs_run["returncode"] != 0 or not equivalent:
            exit_code = 1
        if args.profile and not profile_counters_present:
            exit_code = 1

        result: dict[str, Any] = {
            "schema_version": 1,
            "benchmark": "phase65-fuse-serialization-stress",
            "command": {
                "argv": [str(Path(__file__).resolve())] + argv,
                "workload_argv": workload,
                "vfs_argv": vfs_argv,
            },
            "parameters": {
                "files": args.files,
                "file_size_bytes": args.file_size_bytes,
                "threads": args.threads,
                "iterations": args.iterations,
                "read_bytes": args.read_bytes,
            },
            "native": {"run": native_run, "workload": native_workload},
            "vfs": {
                "run": vfs_run,
                "workload": vfs_workload,
                "profile_counters": profile_counters,
            },
            "summary": {
                "equivalent": equivalent,
                "native_seconds": native_run["duration_seconds"],
                "vfs_seconds": vfs_run["duration_seconds"],
                "ratio": (
                    vfs_run["duration_seconds"] / native_run["duration_seconds"]
                    if native_run["duration_seconds"] > 0
                    else None
                ),
                "fuse_adapter_lock_wait_count": wait_count,
                "fuse_adapter_lock_wait_nanos": wait_nanos,
                "profile_counters_present": profile_counters_present,
                "fuse_adapter_lock_wait_avg_nanos": (
                    wait_nanos / wait_count if wait_count else None
                ),
                "fuse_read_lane_wait_count": read_lane_wait_count,
                "fuse_read_lane_wait_nanos": read_lane_wait_nanos,
                "fuse_read_lane_wait_avg_nanos": (
                    read_lane_wait_nanos / read_lane_wait_count
                    if read_lane_wait_count
                    else None
                ),
                "fuse_write_lane_wait_count": profile_counters.get(
                    "fuse_write_lane_wait_count", 0
                ),
                "fuse_write_lane_wait_nanos": profile_counters.get(
                    "fuse_write_lane_wait_nanos", 0
                ),
                "fuse_read_lane_max_concurrent": profile_counters.get(
                    "fuse_read_lane_max_concurrent", 0
                ),
                "fuse_exclusive_fallback_count": exclusive_fallback_count,
                "backend_serialized_observed": exclusive_fallback_count > 0,
                "read_lane_counter_semantics": "admission through the FUSE read lane; backend global serialization is indicated separately by fuse_exclusive_fallback_count",
            },
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "phase65-fuse-serialization-stress",
            "error": str(exc),
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }

    output_path.parent.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    output_path.write_text(payload, encoding="utf-8")
    sys.stdout.write(payload)
    print(f"Wrote FUSE serialization stress JSON to {output_path}", file=sys.stderr)

    if temp_manager is not None:
        temp_manager.cleanup()
    if not args.keep_temp:
        shutil.rmtree(temp_root, ignore_errors=True)
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
