#!/usr/bin/env python3
"""Phase 6.5 native-vs-Vfs unchanged-base read benchmark."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
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
    git_commit,
    parse_json_stdout,
    positive_float,
    positive_int,
    resolve_vfs_bin,
    run_subprocess,
    sandbox_python,
)


READ_WORKLOAD = r'''
import argparse
import hashlib
import json
import time
from pathlib import Path


def positive_int(value):
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return parsed


parser = argparse.ArgumentParser()
parser.add_argument("--path", required=True)
parser.add_argument("--iterations", type=positive_int, required=True)
parser.add_argument("--read-bytes", type=positive_int, required=True)
args = parser.parse_args()

path = Path(args.path)
started = time.perf_counter()
digest = hashlib.sha256()
opens = 0
bytes_read = 0
for _ in range(args.iterations):
    with path.open("rb", buffering=0) as handle:
        data = handle.read(args.read_bytes)
    digest.update(data)
    opens += 1
    bytes_read += len(data)

print(json.dumps({
    "digest": digest.hexdigest(),
    "total_seconds": time.perf_counter() - started,
    "counts": {
        "open_read_close_calls": opens,
        "open_read_close_bytes": bytes_read,
    },
    "parameters": {
        "path": path.as_posix(),
        "iterations": args.iterations,
        "read_bytes": args.read_bytes,
    },
}, sort_keys=True))
'''


INVALIDATION_WORKLOAD = r'''
import argparse
import hashlib
import json
import os
import time
from pathlib import Path


def positive_int(value):
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return parsed


def non_negative_int(value):
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return parsed


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


parser = argparse.ArgumentParser()
parser.add_argument("--path", required=True)
parser.add_argument("--pre-read-iterations", type=positive_int, required=True)
parser.add_argument("--post-read-iterations", type=positive_int, required=True)
parser.add_argument("--read-bytes", type=positive_int, required=True)
parser.add_argument("--offset", type=non_negative_int, required=True)
args = parser.parse_args()

path = Path(args.path)
started = time.perf_counter()
before_reads = []
for _ in range(args.pre_read_iterations):
    with path.open("rb", buffering=0) as handle:
        before_reads.append(handle.read(args.read_bytes))

with path.open("r+b", buffering=0) as handle:
    handle.seek(args.offset)
    old = handle.read(1)
    if not old:
        raise RuntimeError(f"offset {args.offset} is outside {path}")
    new = bytes([(old[0] + 1) % 256])
    handle.seek(args.offset)
    handle.write(new)
    handle.flush()
    os.fsync(handle.fileno())

stale_reads = 0
post_read_digests = []
for _ in range(args.post_read_iterations):
    with path.open("rb", buffering=0) as handle:
        data = handle.read(args.read_bytes)
    post_read_digests.append(hashlib.sha256(data).hexdigest())
    if args.offset < len(data) and data[args.offset] != new[0]:
        stale_reads += 1

print(json.dumps({
    "sha256_after": sha256_file(path),
    "total_seconds": time.perf_counter() - started,
    "old_byte": old[0],
    "new_byte": new[0],
    "stale_reads": stale_reads,
    "mutation_visible": stale_reads == 0,
    "pre_read_digest": hashlib.sha256(b"".join(before_reads)).hexdigest(),
    "post_read_digests": post_read_digests,
    "parameters": {
        "path": path.as_posix(),
        "pre_read_iterations": args.pre_read_iterations,
        "post_read_iterations": args.post_read_iterations,
        "read_bytes": args.read_bytes,
        "offset": args.offset,
    },
}, sort_keys=True))
'''


PASSTHROUGH_COUNTER_KEYS = {
    "base_fast_open_eligible",
    "base_fast_open_keep_cache",
    "base_fast_open_passthrough_attempted",
    "base_fast_open_passthrough_succeeded",
    "base_fast_open_passthrough_fallback",
    "base_fast_open_rejected",
    "base_fast_inode_invalidations",
    "base_fast_stale_rejections",
}


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return parsed


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Compare repeated read-only open/read/close and read-after-mutate "
            "cache invalidation on native storage and an unchanged Vfs base file."
        )
    )
    parser.add_argument("--file-size-bytes", type=positive_int, default=65536)
    parser.add_argument("--iterations", type=positive_int, default=8)
    parser.add_argument("--read-bytes", type=positive_int, default=4096)
    parser.add_argument("--invalidation-pre-reads", type=positive_int, default=4)
    parser.add_argument("--invalidation-post-reads", type=positive_int, default=3)
    parser.add_argument("--mutation-offset", type=non_negative_int, default=0)
    parser.add_argument(
        "--vfs-bin",
        default=os.environ.get("VFS_BIN"),
        help="vfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=positive_float(os.environ.get("BASE_READ_BENCHMARK_TIMEOUT", "120")),
    )
    parser.add_argument("--profile", action="store_true", default=env_flag("VFS_PROFILE"))
    parser.add_argument("--session-prefix", default=None)
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=env_flag("BASE_READ_BENCHMARK_KEEP_TEMP"),
    )
    parser.add_argument("--output", help="write JSON result to this file instead of stdout")
    parser.add_argument("--json-indent", type=int, default=2)
    args = parser.parse_args(argv)
    if args.mutation_offset >= args.file_size_bytes:
        parser.error("--mutation-offset must be smaller than --file-size-bytes")
    if args.mutation_offset >= args.read_bytes:
        parser.error("--mutation-offset must be smaller than --read-bytes")
    return args


def profile_counter_summary(summaries: list[dict[str, Any]]) -> dict[str, Any]:
    by_source: dict[str, dict[str, Any]] = {}
    max_counters: dict[str, int] = {}
    for summary in summaries:
        counters = summary.get("counters")
        if not isinstance(counters, dict):
            continue
        source = str(summary.get("source", "unknown"))
        by_source[source] = counters
        for key, value in counters.items():
            if isinstance(value, int):
                max_counters[key] = max(max_counters.get(key, 0), value)
    return {"summary_count": len(summaries), "last_by_source": by_source, "max_counters": max_counters}


def passthrough_status(counters: dict[str, int]) -> dict[str, Any]:
    attempted = int(counters.get("base_fast_open_passthrough_attempted", 0) or 0)
    succeeded = int(counters.get("base_fast_open_passthrough_succeeded", 0) or 0)
    fallback = int(counters.get("base_fast_open_passthrough_fallback", 0) or 0)
    counters_present = any(key in counters for key in PASSTHROUGH_COUNTER_KEYS)

    if succeeded > 0:
        status = "supported"
    elif counters_present and attempted > 0 and fallback >= attempted:
        status = "fallback"
    elif counters_present:
        status = "not_observed"
    else:
        status = "not_instrumented"

    return {
        "status": status,
        "passthrough_supported": succeeded > 0,
        "counters_present": counters_present,
        "fallback_read_path": "passthrough" if succeeded > 0 else "hostfs",
        "counters": {key: int(counters.get(key, 0) or 0) for key in sorted(PASSTHROUGH_COUNTER_KEYS)},
    }


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


def create_fixture(root: Path, file_size_bytes: int) -> str:
    root.mkdir(parents=True, exist_ok=True)
    path = root / "hot.bin"
    digest = hashlib.sha256()
    written = 0
    index = 0
    with path.open("wb") as handle:
        while written < file_size_bytes:
            seed = hashlib.sha256(f"vfs-phase65-base-read-{index}".encode()).digest()
            block = (seed * 4096)[: min(65536, file_size_bytes - written)]
            handle.write(block)
            digest.update(block)
            written += len(block)
            index += 1
    return digest.hexdigest()


def copy_fixture(source: Path, destination: Path) -> None:
    shutil.copytree(source, destination, symlinks=True)


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def tree_hash(root: Path) -> dict[str, Any]:
    digest = hashlib.sha256()
    file_count = 0
    dir_count = 0
    symlink_count = 0
    total_bytes = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        filenames.sort()
        rel_dir = Path(dirpath).relative_to(root).as_posix()
        stat = Path(dirpath).lstat()
        digest.update(b"dir\0")
        digest.update(rel_dir.encode("utf-8"))
        digest.update(b"\0")
        digest.update(f"{stat.st_mode}:{stat.st_mtime_ns}:{stat.st_ctime_ns}".encode("ascii"))
        digest.update(b"\0")
        dir_count += 1
        for name in filenames:
            path = Path(dirpath) / name
            rel = path.relative_to(root).as_posix()
            stat = path.lstat()
            if path.is_symlink():
                digest.update(b"symlink\0")
                digest.update(rel.encode("utf-8"))
                digest.update(b"\0")
                digest.update(os.readlink(path).encode("utf-8", errors="surrogateescape"))
                digest.update(b"\0")
                symlink_count += 1
                continue
            digest.update(b"file\0")
            digest.update(rel.encode("utf-8"))
            digest.update(b"\0")
            digest.update(f"{stat.st_mode}:{stat.st_size}:{stat.st_mtime_ns}:{stat.st_ctime_ns}".encode("ascii"))
            digest.update(b"\0")
            file_count += 1
            total_bytes += stat.st_size
            with path.open("rb") as handle:
                while True:
                    chunk = handle.read(1024 * 1024)
                    if not chunk:
                        break
                    digest.update(chunk)
    return {
        "sha256": digest.hexdigest(),
        "files": file_count,
        "directories": dir_count,
        "symlinks": symlink_count,
        "bytes": total_bytes,
    }


def read_workload_argv(iterations: int, read_bytes: int) -> list[str]:
    return [
        sandbox_python(),
        "-c",
        READ_WORKLOAD,
        "--path",
        "hot.bin",
        "--iterations",
        str(iterations),
        "--read-bytes",
        str(read_bytes),
    ]


def invalidation_workload_argv(args: argparse.Namespace) -> list[str]:
    return [
        sandbox_python(),
        "-c",
        INVALIDATION_WORKLOAD,
        "--path",
        "hot.bin",
        "--pre-read-iterations",
        str(args.invalidation_pre_reads),
        "--post-read-iterations",
        str(args.invalidation_post_reads),
        "--read-bytes",
        str(args.read_bytes),
        "--offset",
        str(args.mutation_offset),
    ]


def split_timing(run: dict[str, Any], workload: Optional[dict[str, Any]]) -> dict[str, Any]:
    workload_seconds = None
    overhead_seconds = None
    if workload is not None and isinstance(workload.get("total_seconds"), (int, float)):
        workload_seconds = float(workload["total_seconds"])
        overhead_seconds = max(0.0, float(run["duration_seconds"]) - workload_seconds)
    return {
        "outer_seconds": run["duration_seconds"],
        "workload_seconds": workload_seconds,
        "startup_or_session_overhead_seconds": overhead_seconds,
    }


def compare_read_workloads(native: Optional[dict[str, Any]], vfs: Optional[dict[str, Any]]) -> dict[str, Any]:
    if native is None or vfs is None:
        return {"checked": False, "equivalent": False, "reason": "missing JSON workload output"}
    equivalent = (
        native.get("digest") == vfs.get("digest")
        and native.get("counts") == vfs.get("counts")
        and native.get("parameters") == vfs.get("parameters")
    )
    return {
        "checked": True,
        "equivalent": equivalent,
        "native_digest": native.get("digest"),
        "vfs_digest": vfs.get("digest"),
    }


def compare_invalidation_workloads(native: Optional[dict[str, Any]], vfs: Optional[dict[str, Any]]) -> dict[str, Any]:
    if native is None or vfs is None:
        return {"checked": False, "equivalent": False, "reason": "missing JSON workload output"}
    fields = ("sha256_after", "old_byte", "new_byte", "stale_reads", "mutation_visible")
    equivalent = all(native.get(field) == vfs.get(field) for field in fields)
    return {
        "checked": True,
        "equivalent": equivalent,
        "fields": {field: {"native": native.get(field), "vfs": vfs.get(field)} for field in fields},
    }


def ratio(vfs_seconds: Optional[float], native_seconds: Optional[float]) -> Optional[float]:
    if native_seconds is None or vfs_seconds is None or native_seconds <= 0:
        return None
    return vfs_seconds / native_seconds


def default_output_path() -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.gettempdir()) / f"vfs-base-read-benchmark-{stamp}-{uuid.uuid4().hex[:8]}.json"


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    output_path = Path(args.output).expanduser() if args.output else default_output_path()

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        temp_root = Path(tempfile.mkdtemp(prefix="vfs-base-read-benchmark-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="vfs-base-read-benchmark-")
        temp_root = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    try:
        vfs_bin = resolve_vfs_bin(args.vfs_bin, repo_root)
        env = prepare_environment(temp_root, args.profile)
        source_root = temp_root / "source"
        native_root = temp_root / "native"
        vfs_base_root = temp_root / "vfs-base"
        original_sha = create_fixture(source_root, args.file_size_bytes)
        copy_fixture(source_root, native_root)
        copy_fixture(source_root, vfs_base_root)
        vfs_base_before = tree_hash(vfs_base_root)

        session_prefix = args.session_prefix or f"base-read-{uuid.uuid4().hex}"
        read_argv = read_workload_argv(args.iterations, args.read_bytes)
        native_read_run = run_subprocess(read_argv, native_root, env, args.timeout)
        vfs_read_run = run_subprocess(
            [vfs_bin, "run", "--session", f"{session_prefix}-repeat", "--no-default-allows", "--"] + read_argv,
            vfs_base_root,
            env,
            args.timeout,
        )
        native_read_payload = parse_json_stdout(native_read_run)
        vfs_read_payload = parse_json_stdout(vfs_read_run)
        read_equivalence = compare_read_workloads(native_read_payload, vfs_read_payload)
        read_profile = profile_counter_summary(vfs_read_run.get("profile_summaries", []))
        read_counters = read_profile["max_counters"]
        repeated_passed = (
            native_read_run["returncode"] == 0
            and vfs_read_run["returncode"] == 0
            and read_equivalence.get("equivalent") is True
            and int(read_counters.get("chunk_read_queries", 0) or 0) == 0
            and int(read_counters.get("chunk_read_chunks", 0) or 0) == 0
        )

        invalidation_argv = invalidation_workload_argv(args)
        native_invalidation_run = run_subprocess(invalidation_argv, native_root, env, args.timeout)
        vfs_invalidation_run = run_subprocess(
            [vfs_bin, "run", "--session", f"{session_prefix}-invalidate", "--no-default-allows", "--"]
            + invalidation_argv,
            vfs_base_root,
            env,
            args.timeout,
        )
        native_invalidation_payload = parse_json_stdout(native_invalidation_run)
        vfs_invalidation_payload = parse_json_stdout(vfs_invalidation_run)
        invalidation_equivalence = compare_invalidation_workloads(
            native_invalidation_payload, vfs_invalidation_payload
        )
        vfs_base_sha_after = hash_file(vfs_base_root / "hot.bin")
        vfs_base_after = tree_hash(vfs_base_root)
        native_sha_after = hash_file(native_root / "hot.bin")
        stale_reads = (
            int(vfs_invalidation_payload.get("stale_reads", 1))
            if isinstance(vfs_invalidation_payload, dict)
            else 1
        )
        invalidation_profile = profile_counter_summary(vfs_invalidation_run.get("profile_summaries", []))
        invalidation_passed = (
            native_invalidation_run["returncode"] == 0
            and vfs_invalidation_run["returncode"] == 0
            and invalidation_equivalence.get("equivalent") is True
            and stale_reads == 0
            and vfs_base_after["sha256"] == vfs_base_before["sha256"]
            and native_sha_after != original_sha
        )

        if not repeated_passed or not invalidation_passed:
            exit_code = 1

        native_workload_seconds = (
            float(native_read_payload["total_seconds"])
            if native_read_payload and isinstance(native_read_payload.get("total_seconds"), (int, float))
            else None
        )
        vfs_workload_seconds = (
            float(vfs_read_payload["total_seconds"])
            if vfs_read_payload and isinstance(vfs_read_payload.get("total_seconds"), (int, float))
            else None
        )

        result = {
            "schema_version": 1,
            "benchmark": "phase65-base-read",
            "git_commit": git_commit(repo_root),
            "parameters": {
                "file_size_bytes": args.file_size_bytes,
                "iterations": args.iterations,
                "read_bytes": args.read_bytes,
                "invalidation_pre_reads": args.invalidation_pre_reads,
                "invalidation_post_reads": args.invalidation_post_reads,
                "mutation_offset": args.mutation_offset,
            },
            "vfs": {
                "bin": vfs_bin,
                "profile_enabled": args.profile,
                "passthrough": passthrough_status(read_counters),
            },
            "summary": {
                "passed": exit_code == 0,
                "failed_gates": [
                    name
                    for name, passed in (
                        ("repeated_read_only_open_read", repeated_passed),
                        ("cache_invalidation", invalidation_passed),
                    )
                    if not passed
                ],
                "repeated_open_read_ratio": ratio(vfs_read_run["duration_seconds"], native_read_run["duration_seconds"]),
                "repeated_open_read_workload_ratio": ratio(vfs_workload_seconds, native_workload_seconds),
                "chunk_read_queries": int(read_counters.get("chunk_read_queries", 0) or 0),
                "chunk_read_chunks": int(read_counters.get("chunk_read_chunks", 0) or 0),
                "stale_reads": stale_reads,
            },
            "runs": {
                "repeated_read_only_open_read": {
                    "status": "passed" if repeated_passed else "failed",
                    "native": {
                        "run": native_read_run,
                        "workload": native_read_payload,
                        "timing": split_timing(native_read_run, native_read_payload),
                    },
                    "vfs": {
                        "run": vfs_read_run,
                        "workload": vfs_read_payload,
                        "timing": split_timing(vfs_read_run, vfs_read_payload),
                        "profile_counters": read_profile,
                    },
                    "equivalence": read_equivalence,
                },
                "cache_invalidation": {
                    "status": "passed" if invalidation_passed else "failed",
                    "native": {
                        "run": native_invalidation_run,
                        "workload": native_invalidation_payload,
                        "timing": split_timing(native_invalidation_run, native_invalidation_payload),
                    },
                    "vfs": {
                        "run": vfs_invalidation_run,
                        "workload": vfs_invalidation_payload,
                        "timing": split_timing(vfs_invalidation_run, vfs_invalidation_payload),
                        "profile_counters": invalidation_profile,
                    },
                    "equivalence": invalidation_equivalence,
                    "base_file": {
                        "original_sha256": original_sha,
                        "native_sha256_after": native_sha_after,
                        "vfs_base_sha256_after": vfs_base_sha_after,
                        "vfs_base_unchanged": vfs_base_after["sha256"] == vfs_base_before["sha256"],
                        "vfs_base_tree_before": vfs_base_before,
                        "vfs_base_tree_after": vfs_base_after,
                    },
                },
            },
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "phase65-base-read",
            "error": str(exc),
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    if args.output:
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(payload, encoding="utf-8")
        print(f"Wrote base-read benchmark JSON to {output_path}", file=sys.stderr)
    else:
        sys.stdout.write(payload)

    if temp_manager is not None:
        temp_manager.cleanup()

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
