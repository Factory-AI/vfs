#!/usr/bin/env python3
"""Phase 5.5 native-vs-Vfs read-path profiling benchmark."""

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
from statistics import mean
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
import os
import stat as stat_module
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


parser = argparse.ArgumentParser()
parser.add_argument("--max-files", type=positive_int, required=True)
parser.add_argument("--max-dirs", type=positive_int, required=True)
parser.add_argument("--scan-bytes", type=positive_int, required=True)
parser.add_argument("--stat-iterations", type=positive_int, required=True)
parser.add_argument("--readdir-iterations", type=positive_int, required=True)
parser.add_argument("--open-iterations", type=positive_int, required=True)
parser.add_argument("--open-read-bytes", type=positive_int, required=True)
parser.add_argument("--repeated-read-iterations", type=non_negative_int, required=True)
parser.add_argument("--repeated-read-files", type=positive_int, required=True)
args = parser.parse_args()

root = Path.cwd()
started_total = time.perf_counter()
started = time.perf_counter()
all_files = sorted(path for path in root.rglob("*") if path.is_file())
all_dirs = sorted(path for path in root.rglob("*") if path.is_dir())
files = all_files[: args.max_files]
dirs = [root] + all_dirs[: max(0, args.max_dirs - 1)]
digest = hashlib.sha256()
phase_seconds = {
    "tree_discovery": time.perf_counter() - started,
}
counts = {
    "scan_files": 0,
    "scan_bytes": 0,
    "stat_calls": 0,
    "lstat_calls": 0,
    "readdir_calls": 0,
    "readdir_entries": 0,
    "readdir_plus_calls": 0,
    "readdir_plus_entries": 0,
    "open_read_close_calls": 0,
    "open_read_close_bytes": 0,
    "repeated_read_only_base_open_read_close_calls": 0,
    "repeated_read_only_base_open_read_close_bytes": 0,
}

started = time.perf_counter()
for path in files:
    rel = path.relative_to(root).as_posix()
    data = path.read_bytes()[: args.scan_bytes]
    digest.update(b"scan\0")
    digest.update(rel.encode("utf-8"))
    digest.update(b"\0")
    digest.update(data)
    counts["scan_files"] += 1
    counts["scan_bytes"] += len(data)
phase_seconds["bounded_file_scan"] = time.perf_counter() - started

started = time.perf_counter()
for _ in range(args.stat_iterations):
    for path in files:
        stat_result = os.stat(path)
        lstat_result = os.lstat(path)
        digest.update(b"stat\0")
        digest.update(path.relative_to(root).as_posix().encode("utf-8"))
        digest.update(
            f":{stat_result.st_size}:{stat_module.S_IFMT(lstat_result.st_mode)}".encode("ascii")
        )
        counts["stat_calls"] += 1
        counts["lstat_calls"] += 1
phase_seconds["stat_lstat_storm"] = time.perf_counter() - started

started = time.perf_counter()
for _ in range(args.readdir_iterations):
    for path in dirs:
        names = sorted(os.listdir(path))
        digest.update(b"readdir\0")
        digest.update(path.relative_to(root).as_posix().encode("utf-8"))
        digest.update(b"\0")
        digest.update("\0".join(names).encode("utf-8"))
        counts["readdir_calls"] += 1
        counts["readdir_entries"] += len(names)
phase_seconds["readdir_storm"] = time.perf_counter() - started

started = time.perf_counter()
for _ in range(args.readdir_iterations):
    for path in dirs:
        with os.scandir(path) as iterator:
            entries = []
            for entry in iterator:
                stat_result = entry.stat(follow_symlinks=False)
                mode_type = stat_module.S_IFMT(stat_result.st_mode)
                if stat_module.S_ISREG(stat_result.st_mode):
                    size = stat_result.st_size
                else:
                    size = 0
                entries.append((entry.name, size, mode_type))
        entries.sort()
        digest.update(b"readdir_plus\0")
        digest.update(path.relative_to(root).as_posix().encode("utf-8"))
        digest.update(b"\0")
        digest.update(json.dumps(entries, separators=(",", ":")).encode("utf-8"))
        counts["readdir_plus_calls"] += 1
        counts["readdir_plus_entries"] += len(entries)
phase_seconds["readdir_plus_storm"] = time.perf_counter() - started

started = time.perf_counter()
for _ in range(args.open_iterations):
    for path in files:
        with path.open("rb") as handle:
            data = handle.read(args.open_read_bytes)
        digest.update(b"open-read-close\0")
        digest.update(path.relative_to(root).as_posix().encode("utf-8"))
        digest.update(b"\0")
        digest.update(data)
        counts["open_read_close_calls"] += 1
        counts["open_read_close_bytes"] += len(data)
phase_seconds["open_read_close_loop"] = time.perf_counter() - started

started = time.perf_counter()
if args.repeated_read_iterations:
    repeat_files = files[: args.repeated_read_files]
    for _ in range(args.repeated_read_iterations):
        for path in repeat_files:
            with path.open("rb") as handle:
                data = handle.read(args.open_read_bytes)
            digest.update(b"repeated-open-read-close\0")
            digest.update(path.relative_to(root).as_posix().encode("utf-8"))
            digest.update(b"\0")
            digest.update(data)
            counts["repeated_read_only_base_open_read_close_calls"] += 1
            counts["repeated_read_only_base_open_read_close_bytes"] += len(data)
phase_seconds["repeated_read_only_base_open_read_close_loop"] = time.perf_counter() - started

print(json.dumps({
    "digest": digest.hexdigest(),
    "phase_seconds": phase_seconds,
    "total_seconds": time.perf_counter() - started_total,
    "counts": counts,
    "parameters": {
        "max_files": args.max_files,
        "max_dirs": args.max_dirs,
        "scan_bytes": args.scan_bytes,
        "stat_iterations": args.stat_iterations,
        "readdir_iterations": args.readdir_iterations,
        "open_iterations": args.open_iterations,
        "open_read_bytes": args.open_read_bytes,
        "repeated_read_iterations": args.repeated_read_iterations,
        "repeated_read_files": args.repeated_read_files,
    },
}, sort_keys=True))
'''


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return parsed


def parse_modes(value: str) -> list[str]:
    modes = [mode.strip() for mode in value.split(",") if mode.strip()]
    if not modes:
        raise argparse.ArgumentTypeError("must include at least one mode")
    invalid = [mode for mode in modes if mode not in {"cold", "warm"}]
    if invalid:
        raise argparse.ArgumentTypeError(f"invalid mode(s): {', '.join(invalid)}")
    return list(dict.fromkeys(modes))


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Compare read-heavy filesystem operations on native storage and an "
            "Vfs overlay, with cold/warm and startup/steady-state timing splits."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Fast smoke with profile summaries
  VFS_PROFILE=1 scripts/validation/read-path-benchmark.py --files 8 --dirs 3 \\
    --stat-iterations 1 --readdir-iterations 1 --open-iterations 1 --timeout 60

  # Larger bounded read-path run
  scripts/validation/read-path-benchmark.py --files 256 --dirs 32 --file-size-bytes 8192

Environment:
  VFS_BIN      path/name of vfs executable
  VFS_PROFILE  set to 1 to collect Vfs profile summaries
""",
    )
    parser.add_argument("--files", type=positive_int, default=64, help="fixture file count")
    parser.add_argument("--dirs", type=positive_int, default=8, help="fixture directory count")
    parser.add_argument(
        "--file-size-bytes",
        type=positive_int,
        default=4096,
        help="bytes per fixture file",
    )
    parser.add_argument(
        "--scan-bytes",
        type=positive_int,
        default=1024,
        help="maximum bytes read per file during bounded scan",
    )
    parser.add_argument(
        "--stat-iterations",
        type=positive_int,
        default=4,
        help="stat/lstat storm iterations",
    )
    parser.add_argument(
        "--readdir-iterations",
        type=positive_int,
        default=8,
        help="readdir and readdir_plus storm iterations",
    )
    parser.add_argument(
        "--open-iterations",
        type=positive_int,
        default=3,
        help="open/read/close loop iterations",
    )
    parser.add_argument(
        "--open-read-bytes",
        type=positive_int,
        default=512,
        help="bytes read per open/read/close operation",
    )
    parser.add_argument(
        "--repeated-read-iterations",
        type=non_negative_int,
        default=0,
        help="extra repeated read-only open/read/close iterations over a stable file set",
    )
    parser.add_argument(
        "--repeated-read-files",
        type=positive_int,
        default=1,
        help="number of files used by --repeated-read-iterations",
    )
    parser.add_argument(
        "--modes",
        type=parse_modes,
        default=parse_modes(os.environ.get("READ_PATH_BENCHMARK_MODES", "cold,warm")),
        help="comma-separated modes to run: cold,warm (default: cold,warm)",
    )
    parser.add_argument(
        "--vfs-bin",
        default=os.environ.get("VFS_BIN"),
        help="vfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=positive_float(os.environ.get("READ_PATH_BENCHMARK_TIMEOUT", "120")),
        help="per-command timeout in seconds (default: 120)",
    )
    parser.add_argument(
        "--profile",
        action="store_true",
        default=env_flag("VFS_PROFILE"),
        help="enable VFS_PROFILE=1 for Vfs invocations",
    )
    parser.add_argument(
        "--session-prefix",
        default=None,
        help="Vfs run session prefix (default: generated unique prefix)",
    )
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=env_flag("READ_PATH_BENCHMARK_KEEP_TEMP"),
        help="keep temporary fixture trees and isolated HOME after the run",
    )
    parser.add_argument(
        "--output",
        default=None,
        help="write JSON result to this file; defaults to /tmp/vfs-read-path-benchmark-*.json",
    )
    parser.add_argument(
        "--json-indent",
        type=int,
        default=2,
        help="JSON indentation level (default: 2)",
    )
    return parser.parse_args(argv)


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


def create_fixture(root: Path, file_count: int, dir_count: int, file_size: int) -> None:
    root.mkdir(parents=True, exist_ok=True)
    dirs = []
    for index in range(dir_count):
        directory = root / f"dir_{index:03d}"
        directory.mkdir(parents=True, exist_ok=True)
        dirs.append(directory)

    for index in range(file_count):
        directory = dirs[index % len(dirs)]
        seed = hashlib.sha256(f"vfs-phase55-read-{index}".encode("utf-8")).digest()
        data = (seed * ((file_size // len(seed)) + 1))[:file_size]
        (directory / f"file_{index:05d}.dat").write_bytes(data)

    nested = root / "nested" / "a" / "b"
    nested.mkdir(parents=True, exist_ok=True)
    (nested / "leaf.txt").write_text("vfs read-path benchmark\n", encoding="utf-8")


def copy_fixture(source: Path, destination: Path) -> None:
    shutil.copytree(source, destination, symlinks=True)


def workload_argv(args: argparse.Namespace) -> list[str]:
    return [
        sandbox_python(),
        "-c",
        READ_WORKLOAD,
        "--max-files",
        str(args.files),
        "--max-dirs",
        str(args.dirs + 4),
        "--scan-bytes",
        str(args.scan_bytes),
        "--stat-iterations",
        str(args.stat_iterations),
        "--readdir-iterations",
        str(args.readdir_iterations),
        "--open-iterations",
        str(args.open_iterations),
        "--open-read-bytes",
        str(args.open_read_bytes),
        "--repeated-read-iterations",
        str(args.repeated_read_iterations),
        "--repeated-read-files",
        str(args.repeated_read_files),
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


def compare_workloads(native: Optional[dict[str, Any]], vfs: Optional[dict[str, Any]]) -> dict[str, Any]:
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


def mode_summary(native_run: dict[str, Any], vfs_run: dict[str, Any]) -> dict[str, Any]:
    native_seconds = native_run["duration_seconds"]
    vfs_seconds = vfs_run["duration_seconds"]
    return {
        "native_seconds": native_seconds,
        "vfs_seconds": vfs_seconds,
        "ratio": (vfs_seconds / native_seconds) if native_seconds > 0 else None,
    }


def default_output_path() -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.gettempdir()) / f"vfs-read-path-benchmark-{stamp}-{uuid.uuid4().hex[:8]}.json"


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        temp_root = Path(tempfile.mkdtemp(prefix="vfs-read-path-benchmark-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="vfs-read-path-benchmark-")
        temp_root = Path(temp_manager.name)

    exit_code = 0
    output_path = Path(args.output).expanduser() if args.output else default_output_path()
    result: dict[str, Any]
    try:
        vfs_bin = resolve_vfs_bin(args.vfs_bin, repo_root)
        env = prepare_environment(temp_root, args.profile)
        source_root = temp_root / "source"
        native_root = temp_root / "native"
        vfs_base_root = temp_root / "vfs-base"
        create_fixture(source_root, args.files, args.dirs, args.file_size_bytes)
        copy_fixture(source_root, native_root)
        copy_fixture(source_root, vfs_base_root)

        base_workload = workload_argv(args)
        session_prefix = args.session_prefix or f"read-path-{uuid.uuid4().hex}"
        modes = []
        for mode in args.modes:
            session = f"{session_prefix}-{mode}"
            native_warmup = None
            vfs_warmup = None
            if mode == "warm":
                native_warmup = run_subprocess(base_workload, native_root, env, args.timeout)
                vfs_warmup = run_subprocess(
                    [vfs_bin, "run", "--session", session, "--no-default-allows", "--"] + base_workload,
                    vfs_base_root,
                    env,
                    args.timeout,
                )

            native_run = run_subprocess(base_workload, native_root, env, args.timeout)
            vfs_run = run_subprocess(
                [vfs_bin, "run", "--session", session, "--no-default-allows", "--"] + base_workload,
                vfs_base_root,
                env,
                args.timeout,
            )

            native_workload = parse_json_stdout(native_run)
            vfs_workload = parse_json_stdout(vfs_run)
            equivalence = compare_workloads(native_workload, vfs_workload)
            profile_summaries = []
            if vfs_warmup is not None:
                profile_summaries.extend(vfs_warmup.get("profile_summaries", []))
            profile_summaries.extend(vfs_run.get("profile_summaries", []))

            if native_run["returncode"] != 0 or vfs_run["returncode"] != 0:
                exit_code = 1
            if equivalence["checked"] and not equivalence["equivalent"]:
                exit_code = 1

            mode_record = {
                "mode": mode,
                "session": session,
                "native": {
                    "warmup": native_warmup,
                    "run": native_run,
                    "workload": native_workload,
                    "timing": split_timing(native_run, native_workload),
                },
                "vfs": {
                    "warmup": vfs_warmup,
                    "run": vfs_run,
                    "workload": vfs_workload,
                    "timing": split_timing(vfs_run, vfs_workload),
                    "profile_summaries": profile_summaries,
                    "profile_counters": profile_counter_summary(profile_summaries),
                },
                "summary": mode_summary(native_run, vfs_run),
                "steady_state": {
                    "native_workload_seconds": native_workload.get("total_seconds") if native_workload else None,
                    "vfs_workload_seconds": vfs_workload.get("total_seconds") if vfs_workload else None,
                    "ratio": (
                        vfs_workload["total_seconds"] / native_workload["total_seconds"]
                        if native_workload
                        and vfs_workload
                        and native_workload.get("total_seconds", 0) > 0
                        else None
                    ),
                },
                "equivalence": equivalence,
            }
            modes.append(mode_record)

        result = {
            "schema_version": 1,
            "benchmark": "phase55-read-path",
            "git_commit": git_commit(repo_root),
            "command": {
                "argv": [str(Path(__file__).resolve())] + argv,
                "workload_argv": base_workload,
                "vfs_prefix": [vfs_bin, "run", "--session", "<session>", "--no-default-allows", "--"],
            },
            "environment": {
                "VFS_PROFILE": "1" if args.profile else os.environ.get("VFS_PROFILE"),
                "VFS_BIN": args.vfs_bin,
            },
            "parameters": {
                "files": args.files,
                "dirs": args.dirs,
                "file_size_bytes": args.file_size_bytes,
                "scan_bytes": args.scan_bytes,
                "stat_iterations": args.stat_iterations,
                "readdir_iterations": args.readdir_iterations,
                "open_iterations": args.open_iterations,
                "open_read_bytes": args.open_read_bytes,
                "repeated_read_iterations": args.repeated_read_iterations,
                "repeated_read_files": args.repeated_read_files,
                "modes": args.modes,
            },
            "vfs": {
                "bin": vfs_bin,
                "profile_enabled": args.profile,
                "profile_summary_count": sum(
                    mode["vfs"]["profile_counters"]["summary_count"] for mode in modes
                ),
            },
            "summary": {
                "native_seconds": mean([mode["summary"]["native_seconds"] for mode in modes]),
                "vfs_seconds": mean([mode["summary"]["vfs_seconds"] for mode in modes]),
                "ratio": (
                    mean([mode["summary"]["vfs_seconds"] for mode in modes])
                    / mean([mode["summary"]["native_seconds"] for mode in modes])
                    if mean([mode["summary"]["native_seconds"] for mode in modes]) > 0
                    else None
                ),
                "all_equivalent": all(mode["equivalence"].get("equivalent") for mode in modes),
            },
            "modes": modes,
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "phase55-read-path",
            "error": str(exc),
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(payload, encoding="utf-8")
    sys.stdout.write(payload)
    print(f"Wrote read-path benchmark JSON to {output_path}", file=sys.stderr)

    if temp_manager is not None:
        temp_manager.cleanup()

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
