#!/usr/bin/env python3
"""Phase 5.5 native-vs-AgentFS read-path profiling benchmark."""

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
from statistics import mean
from typing import Any, Optional


OUTPUT_TAIL_CHARS = 4000


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


parser = argparse.ArgumentParser()
parser.add_argument("--max-files", type=positive_int, required=True)
parser.add_argument("--max-dirs", type=positive_int, required=True)
parser.add_argument("--scan-bytes", type=positive_int, required=True)
parser.add_argument("--stat-iterations", type=positive_int, required=True)
parser.add_argument("--readdir-iterations", type=positive_int, required=True)
parser.add_argument("--open-iterations", type=positive_int, required=True)
parser.add_argument("--open-read-bytes", type=positive_int, required=True)
args = parser.parse_args()

root = Path.cwd()
all_files = sorted(path for path in root.rglob("*") if path.is_file())
all_dirs = sorted(path for path in root.rglob("*") if path.is_dir())
files = all_files[: args.max_files]
dirs = [root] + all_dirs[: max(0, args.max_dirs - 1)]
digest = hashlib.sha256()
phase_seconds = {}
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
}

started_total = time.perf_counter()

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
    },
}, sort_keys=True))
'''


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be > 0")
    return parsed


def env_flag(name: str) -> bool:
    value = os.environ.get(name, "")
    return value.lower() in {"1", "true", "yes", "on"}


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
            "AgentFS overlay, with cold/warm and startup/steady-state timing splits."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Fast smoke with profile summaries
  AGENTFS_PROFILE=1 scripts/validation/read-path-benchmark.py --files 8 --dirs 3 \\
    --stat-iterations 1 --readdir-iterations 1 --open-iterations 1 --timeout 60

  # Larger bounded read-path run
  scripts/validation/read-path-benchmark.py --files 256 --dirs 32 --file-size-bytes 8192

Environment:
  AGENTFS_BIN      path/name of agentfs executable
  AGENTFS_PROFILE  set to 1 to collect AgentFS profile summaries
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
        "--modes",
        type=parse_modes,
        default=parse_modes(os.environ.get("READ_PATH_BENCHMARK_MODES", "cold,warm")),
        help="comma-separated modes to run: cold,warm (default: cold,warm)",
    )
    parser.add_argument(
        "--agentfs-bin",
        default=os.environ.get("AGENTFS_BIN"),
        help="agentfs executable path/name (default: repo target binary, building cli if needed)",
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
        default=env_flag("AGENTFS_PROFILE"),
        help="enable AGENTFS_PROFILE=1 for AgentFS invocations",
    )
    parser.add_argument(
        "--session-prefix",
        default=None,
        help="AgentFS run session prefix (default: generated unique prefix)",
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
        help="write JSON result to this file; defaults to /tmp/agentfs-read-path-benchmark-*.json",
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


def create_fixture(root: Path, file_count: int, dir_count: int, file_size: int) -> None:
    root.mkdir(parents=True, exist_ok=True)
    dirs = []
    for index in range(dir_count):
        directory = root / f"dir_{index:03d}"
        directory.mkdir(parents=True, exist_ok=True)
        dirs.append(directory)

    for index in range(file_count):
        directory = dirs[index % len(dirs)]
        seed = hashlib.sha256(f"agentfs-phase55-read-{index}".encode("utf-8")).digest()
        data = (seed * ((file_size // len(seed)) + 1))[:file_size]
        (directory / f"file_{index:05d}.dat").write_bytes(data)

    nested = root / "nested" / "a" / "b"
    nested.mkdir(parents=True, exist_ok=True)
    (nested / "leaf.txt").write_text("agentfs read-path benchmark\n", encoding="utf-8")


def copy_fixture(source: Path, destination: Path) -> None:
    shutil.copytree(source, destination, symlinks=True)


def workload_argv(args: argparse.Namespace) -> list[str]:
    return [
        sys.executable,
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


def compare_workloads(native: Optional[dict[str, Any]], agentfs: Optional[dict[str, Any]]) -> dict[str, Any]:
    if native is None or agentfs is None:
        return {"checked": False, "equivalent": False, "reason": "missing JSON workload output"}
    equivalent = (
        native.get("digest") == agentfs.get("digest")
        and native.get("counts") == agentfs.get("counts")
        and native.get("parameters") == agentfs.get("parameters")
    )
    return {
        "checked": True,
        "equivalent": equivalent,
        "native_digest": native.get("digest"),
        "agentfs_digest": agentfs.get("digest"),
    }


def mode_summary(native_run: dict[str, Any], agentfs_run: dict[str, Any]) -> dict[str, Any]:
    native_seconds = native_run["duration_seconds"]
    agentfs_seconds = agentfs_run["duration_seconds"]
    return {
        "native_seconds": native_seconds,
        "agentfs_seconds": agentfs_seconds,
        "ratio": (agentfs_seconds / native_seconds) if native_seconds > 0 else None,
    }


def default_output_path() -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.gettempdir()) / f"agentfs-read-path-benchmark-{stamp}-{uuid.uuid4().hex[:8]}.json"


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        temp_root = Path(tempfile.mkdtemp(prefix="agentfs-read-path-benchmark-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="agentfs-read-path-benchmark-")
        temp_root = Path(temp_manager.name)

    exit_code = 0
    output_path = Path(args.output).expanduser() if args.output else default_output_path()
    result: dict[str, Any]
    try:
        agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = prepare_environment(temp_root, args.profile)
        source_root = temp_root / "source"
        native_root = temp_root / "native"
        agentfs_base_root = temp_root / "agentfs-base"
        create_fixture(source_root, args.files, args.dirs, args.file_size_bytes)
        copy_fixture(source_root, native_root)
        copy_fixture(source_root, agentfs_base_root)

        base_workload = workload_argv(args)
        session_prefix = args.session_prefix or f"read-path-{uuid.uuid4().hex}"
        modes = []
        for mode in args.modes:
            session = f"{session_prefix}-{mode}"
            native_warmup = None
            agentfs_warmup = None
            if mode == "warm":
                native_warmup = run_subprocess(base_workload, native_root, env, args.timeout)
                agentfs_warmup = run_subprocess(
                    [agentfs_bin, "run", "--session", session, "--no-default-allows", "--"] + base_workload,
                    agentfs_base_root,
                    env,
                    args.timeout,
                )

            native_run = run_subprocess(base_workload, native_root, env, args.timeout)
            agentfs_run = run_subprocess(
                [agentfs_bin, "run", "--session", session, "--no-default-allows", "--"] + base_workload,
                agentfs_base_root,
                env,
                args.timeout,
            )

            native_workload = parse_json_stdout(native_run)
            agentfs_workload = parse_json_stdout(agentfs_run)
            equivalence = compare_workloads(native_workload, agentfs_workload)
            profile_summaries = []
            if agentfs_warmup is not None:
                profile_summaries.extend(agentfs_warmup.get("profile_summaries", []))
            profile_summaries.extend(agentfs_run.get("profile_summaries", []))

            if native_run["returncode"] != 0 or agentfs_run["returncode"] != 0:
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
                "agentfs": {
                    "warmup": agentfs_warmup,
                    "run": agentfs_run,
                    "workload": agentfs_workload,
                    "timing": split_timing(agentfs_run, agentfs_workload),
                    "profile_summaries": profile_summaries,
                    "profile_counters": profile_counter_summary(profile_summaries),
                },
                "summary": mode_summary(native_run, agentfs_run),
                "steady_state": {
                    "native_workload_seconds": native_workload.get("total_seconds") if native_workload else None,
                    "agentfs_workload_seconds": agentfs_workload.get("total_seconds") if agentfs_workload else None,
                    "ratio": (
                        agentfs_workload["total_seconds"] / native_workload["total_seconds"]
                        if native_workload
                        and agentfs_workload
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
                "agentfs_prefix": [agentfs_bin, "run", "--session", "<session>", "--no-default-allows", "--"],
            },
            "environment": {
                "AGENTFS_PROFILE": "1" if args.profile else os.environ.get("AGENTFS_PROFILE"),
                "AGENTFS_BIN": args.agentfs_bin,
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
                "modes": args.modes,
            },
            "agentfs": {
                "bin": agentfs_bin,
                "profile_enabled": args.profile,
                "profile_summary_count": sum(
                    mode["agentfs"]["profile_counters"]["summary_count"] for mode in modes
                ),
            },
            "summary": {
                "native_seconds": mean([mode["summary"]["native_seconds"] for mode in modes]),
                "agentfs_seconds": mean([mode["summary"]["agentfs_seconds"] for mode in modes]),
                "ratio": (
                    mean([mode["summary"]["agentfs_seconds"] for mode in modes])
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
