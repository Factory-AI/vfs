#!/usr/bin/env python3
"""Phase 6 low-memory validation and benchmark gate orchestrator."""

from __future__ import annotations

import argparse
import json
import os
import shlex
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


FACTORY_BOUNDED_READ = r'''
import hashlib
import json
import os
from pathlib import Path

root = Path.cwd()
max_files = int(os.environ.get("PHASE6_FACTORY_MAX_FILES", "512"))
scan_bytes = int(os.environ.get("PHASE6_FACTORY_SCAN_BYTES", "4096"))
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
            "Run Phase 6 validation gates with smoke defaults: optional "
            "factory-mono bounded reads, read-path profiling, default and "
            "partial-origin large-edit gates, no-real-write validation, and "
            "materialization if the CLI command exists."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Fast smoke gates
  scripts/validation/phase6-validation.py --timeout 60

  # Include factory-mono bounded read gate, 3 iterations by default
  scripts/validation/phase6-validation.py --factory-source /path/to/factory-mono

  # Full Phase 6 gate sizes
  scripts/validation/phase6-validation.py --full-gates --factory-source /path/to/factory-mono
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
        default=positive_float(os.environ.get("PHASE6_VALIDATION_TIMEOUT", "120")),
        help="per-command timeout in seconds (default: 120)",
    )
    parser.add_argument(
        "--full-gates",
        action="store_true",
        default=env_flag("PHASE6_FULL_GATES"),
        help="use full benchmark sizes (200 MiB large edit, larger read-path fixture)",
    )
    parser.add_argument(
        "--file-size-mib",
        type=positive_int,
        default=None,
        help="large-edit/no-real-write file size in MiB (default: 1 smoke, 200 with --full-gates)",
    )
    parser.add_argument(
        "--factory-source",
        default=os.environ.get("PHASE6_FACTORY_SOURCE"),
        help="optional factory-mono/source tree for bounded read gate",
    )
    parser.add_argument(
        "--factory-command",
        default=os.environ.get("PHASE6_FACTORY_COMMAND"),
        help="optional bounded read command; defaults to a dependency-free Python scan",
    )
    parser.add_argument(
        "--factory-iterations",
        type=positive_int,
        default=positive_int(os.environ.get("PHASE6_FACTORY_ITERATIONS", "3")),
        help="factory bounded read iterations when --factory-source is provided (default: 3)",
    )
    parser.add_argument(
        "--factory-max-files",
        type=positive_int,
        default=positive_int(os.environ.get("PHASE6_FACTORY_MAX_FILES", "512")),
        help="default factory bounded read max file count (default: 512)",
    )
    parser.add_argument(
        "--factory-scan-bytes",
        type=positive_int,
        default=positive_int(os.environ.get("PHASE6_FACTORY_SCAN_BYTES", "4096")),
        help="default factory bounded read bytes per file (default: 4096)",
    )
    parser.add_argument(
        "--read-path-files",
        type=positive_int,
        default=None,
        help="read-path fixture file count (default: 8 smoke, 256 full)",
    )
    parser.add_argument(
        "--read-path-dirs",
        type=positive_int,
        default=None,
        help="read-path fixture directory count (default: 3 smoke, 32 full)",
    )
    parser.add_argument(
        "--read-path-file-size-bytes",
        type=positive_int,
        default=None,
        help="read-path fixture bytes per file (default: 4096 smoke, 8192 full)",
    )
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=env_flag("PHASE6_VALIDATION_KEEP_TEMP"),
        help="keep temporary JSON outputs and materialization work after the run",
    )
    parser.add_argument(
        "--output",
        help="write JSON result to this file; defaults to /tmp/agentfs-phase6-validation-*.json",
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


def default_output_path() -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.gettempdir()) / f"agentfs-phase6-validation-{stamp}-{uuid.uuid4().hex[:8]}.json"


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
                    "SELECT COUNT(*), "
                    "COALESCE(SUM(CASE WHEN storage_kind = 1 THEN LENGTH(data_inline) ELSE 0 END), 0) "
                    "FROM fs_inode"
                ).fetchone()
                result["fs_inode_rows"] = int(row[0])
                result["fs_inline_bytes"] = int(row[1])
            if table_exists(conn, "fs_partial_origin"):
                row = conn.execute("SELECT COUNT(*) FROM fs_partial_origin").fetchone()
                result["fs_partial_origin_rows"] = int(row[0])
            if table_exists(conn, "fs_chunk_override"):
                row = conn.execute("SELECT COUNT(*) FROM fs_chunk_override").fetchone()
                result["fs_chunk_override_rows"] = int(row[0])
            if table_exists(conn, "fs_materialized"):
                row = conn.execute("SELECT COUNT(*) FROM fs_materialized").fetchone()
                result["fs_materialized_rows"] = int(row[0])
            result["portability_status"] = portability_status(result)
            return result
        finally:
            conn.close()
    except Exception as exc:
        return {"inspectable": False, "reason": str(exc)}


def portability_status(inspect: Optional[dict[str, Any]]) -> Optional[dict[str, Any]]:
    if not inspect or not inspect.get("inspectable", False):
        return None
    partial_origin_rows = int(inspect.get("fs_partial_origin_rows", 0) or 0)
    override_rows = int(inspect.get("fs_chunk_override_rows", 0) or 0)
    stored_bytes = int(inspect.get("fs_data_bytes", 0) or 0) + int(
        inspect.get("fs_inline_bytes", 0) or 0
    )
    return {
        "portable": partial_origin_rows == 0,
        "origin_backed": partial_origin_rows > 0,
        "partial_origin_rows": partial_origin_rows,
        "override_rows": override_rows,
        "stored_bytes": stored_bytes,
        "materialized_rows": inspect.get("fs_materialized_rows"),
    }


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def child_env(agentfs_bin: str) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    env["AGENTFS_BIN"] = agentfs_bin
    return env


def run_json_command(
    name: str,
    argv: list[str],
    cwd: Path,
    env: dict[str, str],
    timeout: float,
    output_path: Path,
) -> dict[str, Any]:
    run = run_subprocess(argv + ["--output", str(output_path)], cwd, env, timeout)
    payload = load_json(output_path) if output_path.exists() else None
    return {
        "name": name,
        "status": "passed" if run["returncode"] == 0 else "failed",
        "run": run,
        "json_path": str(output_path),
        "result": payload,
    }


def factory_command(args: argparse.Namespace) -> str:
    if args.factory_command:
        return args.factory_command
    env_prefix = (
        f"PHASE6_FACTORY_MAX_FILES={shlex.quote(str(args.factory_max_files))} "
        f"PHASE6_FACTORY_SCAN_BYTES={shlex.quote(str(args.factory_scan_bytes))} "
    )
    return env_prefix + " ".join([shlex.quote(sys.executable), "-c", shlex.quote(FACTORY_BOUNDED_READ)])


def run_factory_bounded_read(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
) -> dict[str, Any]:
    if not args.factory_source:
        return {
            "name": "factory_bounded_read",
            "status": "skipped",
            "reason": "--factory-source not provided",
        }
    source = Path(args.factory_source).expanduser().resolve()
    output_path = output_dir / "factory-bounded-read.json"
    command = factory_command(args)
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
        command,
        "--output",
        str(output_path),
    ]
    run = run_subprocess(argv, repo_root, env, args.timeout * args.factory_iterations + 30)
    payload = load_json(output_path) if output_path.exists() else None
    status = "passed" if run["returncode"] == 0 else "failed"
    if status == "passed" and payload:
        ratio = payload.get("summary", {}).get("ratio")
        equivalent = all(
            iteration.get("equivalence", {}).get("equivalent") is True
            for iteration in payload.get("iterations", [])
        )
        if ratio is None or ratio > 6.0 or not equivalent:
            status = "failed"
    return {
        "name": "factory_bounded_read",
        "status": status,
        "run": run,
        "json_path": str(output_path),
        "result": payload,
    }


def run_read_path(
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
    if status == "passed" and payload:
        summary = payload.get("summary", {})
        if summary.get("ratio") is None or summary.get("ratio", 999.0) > 5.0:
            status = "failed"
        if summary.get("all_equivalent") is not True:
            status = "failed"
        for mode in payload.get("modes", []):
            counters = mode.get("agentfs", {}).get("profile_counters", {}).get("max_counters", {})
            if counters.get("chunk_read_queries", 0) != 0 or counters.get("chunk_read_chunks", 0) != 0:
                status = "failed"
    return {
        "name": "read_path_profile",
        "status": status,
        "run": run,
        "json_path": str(output_path),
        "result": payload,
    }


def large_edit_argv(repo_root: Path, args: argparse.Namespace, file_size_mib: int, partial_origin: bool) -> list[str]:
    return [
        sys.executable,
        str(repo_root / "scripts" / "validation" / "large-edit-benchmark.py"),
        "--file-size-mib",
        str(file_size_mib),
        "--timeout",
        str(args.timeout),
        "--partial-origin" if partial_origin else "--no-partial-origin",
    ]


def run_large_edit(
    name: str,
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
    file_size_mib: int,
    partial_origin: bool,
) -> dict[str, Any]:
    output_path = output_dir / f"{name}.json"
    run = run_subprocess(
        large_edit_argv(repo_root, args, file_size_mib, partial_origin) + ["--output", str(output_path)],
        repo_root,
        env,
        args.timeout * 2 + 30,
    )
    payload = load_json(output_path) if output_path.exists() else None
    status = "passed" if run["returncode"] == 0 else "failed"
    if status == "passed" and payload:
        correctness = payload.get("correctness", {})
        if correctness.get("passed") is not True:
            status = "failed"
        if partial_origin:
            inspect = payload.get("database", {}).get("inspect_after", {})
            stored = int(inspect.get("fs_data_bytes", 0) or 0)
            override_rows = int(inspect.get("fs_chunk_override_rows", 0) or 0)
            native_seconds = payload.get("native", {}).get("run", {}).get("duration_seconds", 0)
            agentfs_seconds = payload.get("agentfs_overlay", {}).get("run", {}).get("duration_seconds", 0)
            ratio = (agentfs_seconds / native_seconds) if native_seconds else None
            if stored > 128 * 1024 or override_rows > 1 or ratio is None or ratio > 15.0:
                status = "failed"
    return {
        "name": name,
        "status": status,
        "run": run,
        "json_path": str(output_path),
        "result": payload,
    }


def run_no_real_write(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
    file_size_mib: int,
) -> dict[str, Any]:
    output_path = output_dir / "partial-origin-no-real-write.json"
    argv = [
        sys.executable,
        str(repo_root / "scripts" / "validation" / "partial-origin-no-real-write.py"),
        "--file-size-mib",
        str(file_size_mib),
        "--timeout",
        str(args.timeout),
        "--output",
        str(output_path),
    ]
    run = run_subprocess(argv, repo_root, env, args.timeout * 2 + 30)
    payload = load_json(output_path) if output_path.exists() else None
    return {
        "name": "partial_origin_no_real_write",
        "status": "passed" if run["returncode"] == 0 else "failed",
        "run": run,
        "json_path": str(output_path),
        "result": payload,
    }


def materialize_help(agentfs_bin: str, repo_root: Path, env: dict[str, str]) -> dict[str, Any]:
    run = run_subprocess([agentfs_bin, "materialize", "--help"], repo_root, env, 30)
    return {
        "available": run["returncode"] == 0,
        "probe": run,
    }


def run_materialize_if_available(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
    file_size_mib: int,
    agentfs_bin: str,
) -> dict[str, Any]:
    help_result = materialize_help(agentfs_bin, repo_root, env)
    if not help_result["available"]:
        return {
            "name": "materialize_benchmark",
            "status": "skipped",
            "reason": "agentfs materialize command is not available",
            "probe": help_result["probe"],
        }

    setup_output = output_dir / "materialize-setup-large-edit.json"
    setup = run_subprocess(
        large_edit_argv(repo_root, args, file_size_mib, True)
        + ["--keep-temp", "--output", str(setup_output)],
        repo_root,
        env,
        args.timeout * 2 + 30,
    )
    setup_payload = load_json(setup_output) if setup_output.exists() else None
    if setup["returncode"] != 0 or not setup_payload:
        return {
            "name": "materialize_benchmark",
            "status": "failed",
            "setup": setup,
            "setup_json_path": str(setup_output),
            "setup_result": setup_payload,
        }

    db_path = setup_payload.get("agentfs", {}).get("db_path")
    target_db = output_dir / "materialized.db"
    command = [agentfs_bin, "materialize", str(db_path), "--output", str(target_db), "--verify"]
    run = run_subprocess(command, repo_root, env, args.timeout * 2 + 30)
    inspect = inspect_db(target_db)
    port_status = portability_status(inspect)
    status = (
        "passed"
        if run["returncode"] == 0
        and port_status is not None
        and int(port_status.get("partial_origin_rows", 1) or 0) == 0
        else "failed"
    )
    return {
        "name": "materialize_benchmark",
        "status": status,
        "setup": setup,
        "setup_json_path": str(setup_output),
        "setup_result": setup_payload,
        "run": run,
        "target_db": str(target_db),
        "inspect_after": inspect,
        "portability_status": port_status,
    }


def run_passed(record: dict[str, Any], *, allow_skipped: bool) -> bool:
    if record.get("status") == "passed":
        return True
    return allow_skipped and record.get("status") == "skipped"


def extract_portability(runs: dict[str, dict[str, Any]]) -> dict[str, Any]:
    def large_edit_status(name: str) -> Optional[dict[str, Any]]:
        result = runs.get(name, {}).get("result")
        if not isinstance(result, dict):
            return None
        inspect = result.get("database", {}).get("inspect_after")
        if not isinstance(inspect, dict):
            return None
        return inspect.get("portability_status") or portability_status(inspect)

    return {
        "large_edit_default": large_edit_status("large_edit_default"),
        "large_edit_partial_origin": large_edit_status("large_edit_partial_origin"),
        "partial_origin_no_real_write": (
            runs.get("partial_origin_no_real_write", {})
            .get("result", {})
            .get("database", {})
            .get("portability_status")
        ),
        "materialize": runs.get("materialize_benchmark", {}).get("portability_status"),
    }


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    output_path = Path(args.output).expanduser() if args.output else default_output_path()
    file_size_mib = args.file_size_mib or (200 if args.full_gates else 1)

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        output_dir = Path(tempfile.mkdtemp(prefix="agentfs-phase6-validation-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="agentfs-phase6-validation-")
        output_dir = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    try:
        agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = child_env(agentfs_bin)
        runs: dict[str, dict[str, Any]] = {}
        runs["factory_bounded_read"] = run_factory_bounded_read(args, repo_root, env, output_dir)
        runs["read_path_profile"] = run_read_path(args, repo_root, env, output_dir)
        runs["large_edit_default"] = run_large_edit(
            "large_edit_default", args, repo_root, env, output_dir, file_size_mib, False
        )
        runs["large_edit_partial_origin"] = run_large_edit(
            "large_edit_partial_origin", args, repo_root, env, output_dir, file_size_mib, True
        )
        runs["partial_origin_no_real_write"] = run_no_real_write(
            args, repo_root, env, output_dir, file_size_mib
        )
        runs["materialize_benchmark"] = run_materialize_if_available(
            args, repo_root, env, output_dir, file_size_mib, agentfs_bin
        )

        failed = [
            name
            for name, record in runs.items()
            if not run_passed(record, allow_skipped=not args.full_gates)
        ]
        if failed:
            exit_code = 1

        result = {
            "schema_version": 1,
            "benchmark": "phase6-validation-gates",
            "git_commit": git_commit(repo_root),
            "mode": "full" if args.full_gates else "smoke",
            "parameters": {
                "file_size_mib": file_size_mib,
                "timeout_seconds": args.timeout,
                "factory_source": str(Path(args.factory_source).expanduser().resolve())
                if args.factory_source
                else None,
                "factory_iterations": args.factory_iterations if args.factory_source else 0,
                "factory_max_files": args.factory_max_files,
                "factory_scan_bytes": args.factory_scan_bytes,
            },
            "agentfs": {
                "bin": agentfs_bin,
            },
            "summary": {
                "passed": exit_code == 0,
                "failed_gates": failed,
                "skipped_gates": [
                    name for name, record in runs.items() if record.get("status") == "skipped"
                ],
                "portability_status": extract_portability(runs),
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
            "benchmark": "phase6-validation-gates",
            "error": str(exc),
            "output_dir": str(output_dir),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(payload, encoding="utf-8")
    sys.stdout.write(payload)
    print(f"Wrote Phase 6 validation JSON to {output_path}", file=sys.stderr)

    if temp_manager is not None:
        temp_manager.cleanup()

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
