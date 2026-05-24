#!/usr/bin/env python3
"""Low-memory concurrent read stress for Phase 6.5 FUSE serialization profiling."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shlex
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
    if path.is_file() and ".agentfs" not in path.relative_to(root).parts
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


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run a tiny native-vs-AgentFS threaded read workload and capture "
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
        "--agentfs-bin",
        default=os.environ.get("AGENTFS_BIN"),
        help="agentfs executable path/name (default: repo target binary, building cli if needed)",
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
        help="enable AGENTFS_PROFILE=1 for AgentFS invocation (default: enabled)",
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


def tail_text(value: Any) -> str:
    text = value.decode("utf-8", errors="replace") if isinstance(value, bytes) else str(value or "")
    return text if len(text) <= OUTPUT_TAIL_CHARS else text[-OUTPUT_TAIL_CHARS:]


def extract_profile_summaries(stderr: Any) -> list[dict[str, Any]]:
    text = stderr.decode("utf-8", errors="replace") if isinstance(stderr, bytes) else str(stderr or "")
    summaries: list[dict[str, Any]] = []
    for line in text.splitlines():
        if "agentfs_profile_summary" not in line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and value.get("event") == "agentfs_profile_summary":
            summaries.append(value)
    return summaries


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


def terminate_process_tree(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is not None:
        return
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except Exception:
        proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
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
        stdout, stderr = proc.communicate(timeout=5)
        timed_out = True
    return {
        "argv": argv,
        "cwd": str(cwd),
        "duration_seconds": time.perf_counter() - started,
        "returncode": proc.returncode,
        "timed_out": timed_out,
        "stdout_tail": tail_text(stdout),
        "stderr_tail": tail_text(stderr),
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
        path = Path(agentfs_bin).expanduser()
        if path.is_file() and os.access(path, os.X_OK):
            return str(path.resolve())
        if os.sep not in agentfs_bin:
            found = shutil.which(agentfs_bin)
            if found:
                return found
        raise RuntimeError(f"agentfs executable not found: {agentfs_bin}")

    for path in (
        repo_root / "cli" / "target" / "debug" / "agentfs",
        repo_root / "cli" / "target" / "release" / "agentfs",
    ):
        if path.is_file() and os.access(path, os.X_OK):
            return str(path)

    build = subprocess.run(
        [
            "cargo",
            "build",
            "--manifest-path",
            str(repo_root / "cli" / "Cargo.toml"),
            "--no-default-features",
        ],
        cwd=str(repo_root),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if build.returncode != 0:
        raise RuntimeError(f"failed to build agentfs\n{tail_text(build.stderr)}")
    built = repo_root / "cli" / "target" / "debug" / "agentfs"
    if not built.is_file():
        raise RuntimeError(f"built agentfs binary missing: {built}")
    return str(built)


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
    return env


def create_fixture(root: Path, files: int, file_size: int) -> None:
    root.mkdir(parents=True, exist_ok=True)
    for index in range(files):
        seed = hashlib.sha256(f"agentfs-phase65-serialization-{index}".encode()).digest()
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
    return Path(tempfile.gettempdir()) / f"agentfs-fuse-serialization-stress-{stamp}.json"


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    temp_root = Path(tempfile.mkdtemp(prefix="agentfs-fuse-serialization-stress-"))
    if not args.keep_temp:
        temp_manager = tempfile.TemporaryDirectory(prefix="agentfs-fuse-serialization-stress-home-")

    output_path = Path(args.output).expanduser() if args.output else default_output_path()
    exit_code = 0
    try:
        agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env_root = Path(temp_manager.name) if temp_manager is not None else temp_root
        env = prepare_environment(env_root, args.profile)
        native_root = temp_root / "native"
        agentfs_root = temp_root / "agentfs"
        workload_script = temp_root / "concurrent_read_workload.py"
        workload_script.write_text(CONCURRENT_READ_WORKLOAD, encoding="utf-8")
        create_fixture(native_root, args.files, args.file_size_bytes)
        shutil.copytree(native_root, agentfs_root)

        workload = workload_argv(args, workload_script)
        agentfs_command = " ".join(shlex.quote(part) for part in workload)
        agentfs_argv = [
            agentfs_bin,
            "init",
            "--force",
            "--base",
            str(agentfs_root),
            "--backend",
            "fuse",
            "--command",
            agentfs_command,
            args.session,
        ]
        native_run = run_subprocess(workload, native_root, env, args.timeout)
        agentfs_run = run_subprocess(agentfs_argv, agentfs_root, env, args.timeout)
        native_workload = parse_json_stdout(native_run)
        agentfs_workload = parse_json_stdout(agentfs_run)
        profile_counters = max_profile_counters(agentfs_run["profile_summaries"])
        profile_counters_present = (
            len(agentfs_run["profile_summaries"]) > 0
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
            and agentfs_workload is not None
            and native_workload.get("digest") == agentfs_workload.get("digest")
            and native_workload.get("counts") == agentfs_workload.get("counts")
        )
        if native_run["returncode"] != 0 or agentfs_run["returncode"] != 0 or not equivalent:
            exit_code = 1
        if args.profile and not profile_counters_present:
            exit_code = 1

        result: dict[str, Any] = {
            "schema_version": 1,
            "benchmark": "phase65-fuse-serialization-stress",
            "command": {
                "argv": [str(Path(__file__).resolve())] + argv,
                "workload_argv": workload,
                "agentfs_argv": agentfs_argv,
            },
            "parameters": {
                "files": args.files,
                "file_size_bytes": args.file_size_bytes,
                "threads": args.threads,
                "iterations": args.iterations,
                "read_bytes": args.read_bytes,
            },
            "native": {"run": native_run, "workload": native_workload},
            "agentfs": {
                "run": agentfs_run,
                "workload": agentfs_workload,
                "profile_counters": profile_counters,
            },
            "summary": {
                "equivalent": equivalent,
                "native_seconds": native_run["duration_seconds"],
                "agentfs_seconds": agentfs_run["duration_seconds"],
                "ratio": (
                    agentfs_run["duration_seconds"] / native_run["duration_seconds"]
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
