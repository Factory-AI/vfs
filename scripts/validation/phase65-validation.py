#!/usr/bin/env python3
"""Phase 6.5 read fast-path validation and benchmark gates."""

from __future__ import annotations

import argparse
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


FACTORY_BOUNDED_READ = r'''
import hashlib
import json
import os
from pathlib import Path

root = Path.cwd()
max_files = int(os.environ.get("PHASE65_FACTORY_MAX_FILES", "512"))
scan_bytes = int(os.environ.get("PHASE65_FACTORY_SCAN_BYTES", "4096"))
skip_names = {
    ".agentfs",
    ".direnv",
    ".git",
    ".next",
    ".turbo",
    "bazel-bin",
    "bazel-out",
    "bazel-testlogs",
    "dist",
    "node_modules",
    "target",
}
digest = hashlib.sha256()
files = 0
bytes_read = 0
dirs_seen = 0

for dirpath, dirnames, filenames in os.walk(root):
    dirnames[:] = sorted(name for name in dirnames if name not in skip_names)
    dirs_seen += 1
    for name in sorted(filenames):
        if files >= max_files:
            break
        path = Path(dirpath) / name
        try:
            stat = path.stat()
            with path.open("rb") as handle:
                data = handle.read(scan_bytes)
        except OSError:
            continue
        rel = path.relative_to(root).as_posix()
        digest.update(rel.encode("utf-8"))
        digest.update(b"\0")
        digest.update(str(stat.st_size).encode("ascii"))
        digest.update(b"\0")
        digest.update(data)
        files += 1
        bytes_read += len(data)
    if files >= max_files:
        break

print(json.dumps({
    "digest": digest.hexdigest(),
    "files": files,
    "bytes_read": bytes_read,
    "dirs_seen": dirs_seen,
    "max_files": max_files,
    "scan_bytes": scan_bytes,
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
            "Run Phase 6.5 validation gates: factory bounded read, controlled "
            "read/metadata, repeated unchanged-base open/read, cache invalidation, "
            "and optional passthrough profile metrics."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Fast smoke, low memory
  scripts/validation/phase65-validation.py --timeout 60

  # Full Phase 6.5 gates
  scripts/validation/phase65-validation.py --full-gates --factory-source /path/to/factory-mono
""",
    )
    parser.add_argument(
        "--agentfs-bin",
        default=os.environ.get("AGENTFS_BIN"),
        help="agentfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=positive_float(os.environ.get("PHASE65_VALIDATION_TIMEOUT", "120")),
        help="per-command timeout in seconds (default: 120)",
    )
    parser.add_argument(
        "--full-gates",
        action="store_true",
        default=env_flag("PHASE65_FULL_GATES"),
        help="enforce Phase 6.5 performance thresholds",
    )
    parser.add_argument(
        "--factory-source",
        default=os.environ.get("PHASE65_FACTORY_SOURCE") or os.environ.get("PHASE6_FACTORY_SOURCE"),
        help="optional factory-mono/source tree for bounded read gate",
    )
    parser.add_argument(
        "--factory-command",
        default=os.environ.get("PHASE65_FACTORY_COMMAND") or os.environ.get("PHASE6_FACTORY_COMMAND"),
        help="optional bounded read command; defaults to a dependency-free Python scan",
    )
    parser.add_argument(
        "--factory-iterations",
        type=positive_int,
        default=positive_int(os.environ.get("PHASE65_FACTORY_ITERATIONS", "3")),
    )
    parser.add_argument(
        "--factory-max-files",
        type=positive_int,
        default=positive_int(os.environ.get("PHASE65_FACTORY_MAX_FILES", "512")),
    )
    parser.add_argument(
        "--factory-scan-bytes",
        type=positive_int,
        default=positive_int(os.environ.get("PHASE65_FACTORY_SCAN_BYTES", "4096")),
    )
    parser.add_argument("--read-path-files", type=positive_int, default=None)
    parser.add_argument("--read-path-dirs", type=positive_int, default=None)
    parser.add_argument("--read-path-file-size-bytes", type=positive_int, default=None)
    parser.add_argument("--base-read-file-size-bytes", type=positive_int, default=None)
    parser.add_argument("--base-read-iterations", type=positive_int, default=None)
    parser.add_argument("--base-read-bytes", type=positive_int, default=None)
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=env_flag("PHASE65_VALIDATION_KEEP_TEMP"),
        help="keep temporary JSON outputs after the run",
    )
    parser.add_argument("--output", help="write JSON result to this file")
    parser.add_argument("--json-indent", type=int, default=2)
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


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def parse_json_stdout_tail(run: dict[str, Any]) -> Optional[dict[str, Any]]:
    for line in reversed(str(run.get("stdout_tail", "")).splitlines()):
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


def child_env(agentfs_bin: str) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    env["AGENTFS_BIN"] = agentfs_bin
    return env


def default_output_path() -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.gettempdir()) / f"agentfs-phase65-validation-{stamp}-{uuid.uuid4().hex[:8]}.json"


def factory_command(args: argparse.Namespace) -> str:
    if args.factory_command:
        return args.factory_command
    env_prefix = (
        f"PHASE65_FACTORY_MAX_FILES={shlex.quote(str(args.factory_max_files))} "
        f"PHASE65_FACTORY_SCAN_BYTES={shlex.quote(str(args.factory_scan_bytes))} "
    )
    return env_prefix + " ".join([shlex.quote(sys.executable), "-c", shlex.quote(FACTORY_BOUNDED_READ)])


def run_factory_bounded_read(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
) -> dict[str, Any]:
    if not args.factory_source:
        return {"name": "factory_bounded_read", "status": "skipped", "reason": "--factory-source not provided"}
    source = Path(args.factory_source).expanduser().resolve()
    output_path = output_dir / "factory-bounded-read.json"
    argv = [
        sys.executable,
        str(repo_root / "scripts" / "validation" / "workload-baseline.py"),
        "--mode",
        "command",
        "--source",
        str(source),
        "--in-place-native",
        "--compare-stdout",
        "--iterations",
        str(args.factory_iterations),
        "--timeout",
        str(args.timeout),
        "--command",
        factory_command(args),
        "--output",
        str(output_path),
    ]
    run = run_subprocess(argv, repo_root, env, args.timeout * args.factory_iterations + 30)
    payload = load_json(output_path) if output_path.exists() else None
    status = "passed" if run["returncode"] == 0 else "failed"
    ratio_value = payload.get("summary", {}).get("ratio") if isinstance(payload, dict) else None
    equivalent = (
        all(iteration.get("equivalence", {}).get("equivalent") is True for iteration in payload.get("iterations", []))
        if isinstance(payload, dict)
        else False
    )
    coverage_ok = False
    if isinstance(payload, dict):
        coverage_ok = True
        for iteration in payload.get("iterations", []):
            native_json = parse_json_stdout_tail(iteration.get("native", {}))
            agentfs_json = parse_json_stdout_tail(iteration.get("agentfs", {}))
            for workload_json in (native_json, agentfs_json):
                if (
                    not isinstance(workload_json, dict)
                    or int(workload_json.get("files", 0) or 0) <= 0
                    or int(workload_json.get("bytes_read", 0) or 0) <= 0
                ):
                    coverage_ok = False
    if status == "passed" and (ratio_value is None or not equivalent):
        status = "failed"
    if args.full_gates and status == "passed" and not coverage_ok:
        status = "failed"
    if args.full_gates and status == "passed" and ratio_value > 3.0:
        status = "failed"
    return {
        "name": "factory_bounded_read",
        "status": status,
        "run": run,
        "json_path": str(output_path),
        "result": payload,
        "gate": {
            "ratio": ratio_value,
            "threshold": 3.0 if args.full_gates else None,
            "equivalent": equivalent,
            "coverage_ok": coverage_ok,
        },
    }


def read_path_chunk_counters(payload: Optional[dict[str, Any]]) -> dict[str, Any]:
    counters: dict[str, Any] = {
        "chunk_read_queries": None,
        "chunk_read_chunks": None,
        "profile_counters_present": False,
    }
    if not isinstance(payload, dict):
        return counters
    for mode in payload.get("modes", []):
        profile_counters = mode.get("agentfs", {}).get("profile_counters", {})
        if int(profile_counters.get("summary_count", 0) or 0) <= 0:
            continue
        max_counters = profile_counters.get("max_counters", {})
        if not isinstance(max_counters, dict):
            continue
        if "chunk_read_queries" in max_counters and "chunk_read_chunks" in max_counters:
            counters["profile_counters_present"] = True
        for key in ("chunk_read_queries", "chunk_read_chunks"):
            value = max_counters.get(key)
            if isinstance(value, int):
                current = counters[key]
                counters[key] = value if current is None else max(current, value)
    return counters


def run_controlled_read_metadata(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
) -> dict[str, Any]:
    files = args.read_path_files or (256 if args.full_gates else 8)
    dirs = args.read_path_dirs or (32 if args.full_gates else 3)
    file_size = args.read_path_file_size_bytes or (8192 if args.full_gates else 4096)
    output_path = output_dir / "read-path-profile.json"
    argv = [
        sys.executable,
        str(repo_root / "scripts" / "validation" / "read-path-benchmark.py"),
        "--files",
        str(files),
        "--dirs",
        str(dirs),
        "--file-size-bytes",
        str(file_size),
        "--stat-iterations",
        "8" if args.full_gates else "1",
        "--readdir-iterations",
        "16" if args.full_gates else "1",
        "--open-iterations",
        "8" if args.full_gates else "1",
        "--timeout",
        str(args.timeout),
        "--profile",
        "--output",
        str(output_path),
    ]
    run = run_subprocess(argv, repo_root, env, args.timeout * 2 + 30)
    payload = load_json(output_path) if output_path.exists() else None
    status = "passed" if run["returncode"] == 0 else "failed"
    summary = payload.get("summary", {}) if isinstance(payload, dict) else {}
    ratio_value = summary.get("ratio")
    all_equivalent = summary.get("all_equivalent") is True
    chunk_counters = read_path_chunk_counters(payload)
    if status == "passed" and (ratio_value is None or not all_equivalent):
        status = "failed"
    if status == "passed" and (
        chunk_counters.get("chunk_read_queries") != 0 or chunk_counters.get("chunk_read_chunks") != 0
    ):
        status = "failed"
    if args.full_gates and status == "passed" and not chunk_counters.get("profile_counters_present"):
        status = "failed"
    if args.full_gates and status == "passed" and ratio_value > 3.0:
        status = "failed"
    return {
        "name": "controlled_read_metadata",
        "status": status,
        "run": run,
        "json_path": str(output_path),
        "result": payload,
        "gate": {
            "ratio": ratio_value,
            "threshold": 3.0 if args.full_gates else None,
            "all_equivalent": all_equivalent,
            **chunk_counters,
        },
    }


def run_base_read(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
) -> dict[str, Any]:
    file_size = args.base_read_file_size_bytes or (1024 * 1024 if args.full_gates else 65536)
    iterations = args.base_read_iterations or (64 if args.full_gates else 8)
    read_bytes = args.base_read_bytes or (65536 if args.full_gates else 4096)
    output_path = output_dir / "base-read-benchmark.json"
    argv = [
        sys.executable,
        str(repo_root / "scripts" / "validation" / "base-read-benchmark.py"),
        "--file-size-bytes",
        str(file_size),
        "--iterations",
        str(iterations),
        "--read-bytes",
        str(read_bytes),
        "--timeout",
        str(args.timeout),
        "--profile",
        "--output",
        str(output_path),
    ]
    run = run_subprocess(argv, repo_root, env, args.timeout * 2 + 30)
    payload = load_json(output_path) if output_path.exists() else None
    status = "passed" if run["returncode"] == 0 else "failed"
    summary = payload.get("summary", {}) if isinstance(payload, dict) else {}
    passthrough = payload.get("agentfs", {}).get("passthrough", {}) if isinstance(payload, dict) else {}
    repeated_ratio = summary.get("repeated_open_read_workload_ratio")
    chunk_read_queries = int(summary.get("chunk_read_queries", 1) or 0)
    chunk_read_chunks = int(summary.get("chunk_read_chunks", 1) or 0)
    stale_reads = int(summary.get("stale_reads", 1) or 0)

    if status == "passed" and (chunk_read_queries != 0 or chunk_read_chunks != 0 or stale_reads != 0):
        status = "failed"
    passthrough_supported = passthrough.get("passthrough_supported") is True
    if args.full_gates and status == "passed" and (repeated_ratio is None or repeated_ratio > 2.0):
        status = "failed"

    return {
        "name": "base_repeated_read_and_cache_invalidation",
        "status": status,
        "run": run,
        "json_path": str(output_path),
        "result": payload,
        "gate": {
            "repeated_open_read_ratio": repeated_ratio,
            "repeated_open_read_threshold": 2.0 if args.full_gates else None,
            "ratio_gate_applies": bool(args.full_gates),
            "chunk_read_queries": chunk_read_queries,
            "chunk_read_chunks": chunk_read_chunks,
            "stale_reads": stale_reads,
            "passthrough": passthrough,
        },
    }


def run_passed(record: dict[str, Any], *, full_gates: bool) -> bool:
    if record.get("status") == "passed":
        return True
    return record.get("status") == "skipped" and not full_gates


def default_gate_summary(runs: dict[str, dict[str, Any]]) -> dict[str, Any]:
    return {name: {"status": record.get("status"), **record.get("gate", {})} for name, record in runs.items()}


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    output_path = Path(args.output).expanduser() if args.output else default_output_path()

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        output_dir = Path(tempfile.mkdtemp(prefix="agentfs-phase65-validation-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="agentfs-phase65-validation-")
        output_dir = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    try:
        agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = child_env(agentfs_bin)
        runs: dict[str, dict[str, Any]] = {}
        runs["factory_bounded_read"] = run_factory_bounded_read(args, repo_root, env, output_dir)
        runs["controlled_read_metadata"] = run_controlled_read_metadata(args, repo_root, env, output_dir)
        runs["base_repeated_read_and_cache_invalidation"] = run_base_read(args, repo_root, env, output_dir)

        failed = [name for name, record in runs.items() if not run_passed(record, full_gates=args.full_gates)]
        if failed:
            exit_code = 1

        base_gate = runs["base_repeated_read_and_cache_invalidation"].get("gate", {})
        passthrough = base_gate.get("passthrough", {})
        result = {
            "schema_version": 1,
            "benchmark": "phase65-validation-gates",
            "git_commit": git_commit(repo_root),
            "mode": "full" if args.full_gates else "smoke",
            "parameters": {
                "timeout_seconds": args.timeout,
                "factory_source": str(Path(args.factory_source).expanduser().resolve()) if args.factory_source else None,
                "factory_iterations": args.factory_iterations if args.factory_source else 0,
                "factory_max_files": args.factory_max_files,
                "factory_scan_bytes": args.factory_scan_bytes,
            },
            "agentfs": {
                "bin": agentfs_bin,
                "passthrough": passthrough,
            },
            "summary": {
                "passed": exit_code == 0,
                "failed_gates": failed,
                "skipped_gates": [name for name, record in runs.items() if record.get("status") == "skipped"],
                "gates": default_gate_summary(runs),
            },
            "runs": runs,
            "output_dir": str(output_dir),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "phase65-validation-gates",
            "error": str(exc),
            "output_dir": str(output_dir),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(payload, encoding="utf-8")
    sys.stdout.write(payload)
    print(f"Wrote Phase 6.5 validation JSON to {output_path}", file=sys.stderr)

    if temp_manager is not None:
        temp_manager.cleanup()

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
