#!/usr/bin/env python3
"""Phase 7 principle-preserving Git workload validation gates."""

from __future__ import annotations

import argparse
import json
import os
import signal
import shutil
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

GIT_PHASE_THRESHOLDS = {
    "clone": 3.0,
    "checkout": 3.0,
    "clone_checkout": 3.0,
    "status": 2.0,
    "read": 2.0,
    "search": 2.0,
    "read_search": 2.0,
    "edit": 2.0,
    "diff": 2.0,
}

REQUIRED_GIT_PHASES = ("clone", "checkout", "status", "read_search", "edit", "diff")


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
            "Run Phase 7 principle gates: strict-portable Git workload when "
            "available, no-real-write/base-hash checks, portable integrity, "
            "backup/materialize verification, strict-mode partial-origin row "
            "checks, and performance threshold reporting."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Fast smoke over available gates
  scripts/validation/phase7-validation.py --timeout 60

  # Full Phase 7 gate policy; skipped required gates fail
  scripts/validation/phase7-validation.py --full-gates --timeout 180
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
        default=positive_float(os.environ.get("PHASE7_VALIDATION_TIMEOUT", "120")),
        help="per-command timeout in seconds (default: 120)",
    )
    parser.add_argument(
        "--smoke",
        action="store_true",
        help="explicitly run smoke policy (default; overrides PHASE7_FULL_GATES)",
    )
    parser.add_argument(
        "--full-gates",
        action="store_true",
        default=env_flag("PHASE7_FULL_GATES"),
        help="enforce full Phase 7 required-gate and performance-threshold policy",
    )
    parser.add_argument(
        "--require-git-workload",
        action="store_true",
        default=env_flag("PHASE7_REQUIRE_GIT_WORKLOAD"),
        help="treat the git workload benchmark as required even outside --full-gates",
    )
    parser.add_argument(
        "--git-workload-script",
        default=os.environ.get("PHASE7_GIT_WORKLOAD_SCRIPT"),
        help="path to git-workload-benchmark.py (default: scripts/validation/git-workload-benchmark.py)",
    )
    parser.add_argument(
        "--strict-file-size-mib",
        type=positive_int,
        default=None,
        help="strict portable large-edit fixture size (default: 1 smoke, 200 full)",
    )
    parser.add_argument(
        "--no-real-write-file-size-mib",
        type=positive_int,
        default=None,
        help="no-real-write fixture size (default: 1 smoke, 200 full)",
    )
    parser.add_argument(
        "--materialize-file-size-mib",
        type=positive_int,
        default=None,
        help="partial-origin materialize fixture size (default: 1 smoke, 200 full)",
    )
    parser.add_argument("--base-read-file-size-bytes", type=positive_int, default=None)
    parser.add_argument("--base-read-iterations", type=positive_int, default=None)
    parser.add_argument("--base-read-bytes", type=positive_int, default=None)
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=env_flag("PHASE7_VALIDATION_KEEP_TEMP"),
        help="keep temporary JSON outputs and child benchmark temp trees",
    )
    parser.add_argument("--output", help="write JSON result to this file")
    parser.add_argument("--json-indent", type=int, default=2)
    args = parser.parse_args(argv)
    if args.smoke:
        args.full_gates = False
    return args


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
    *,
    keep_stdout: bool = False,
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

    result = {
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
    if keep_stdout:
        result["stdout"] = stdout or ""
    return result


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
    return Path(tempfile.gettempdir()) / f"agentfs-phase7-validation-{stamp}-{uuid.uuid4().hex[:8]}.json"


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def parse_json_text(text: str) -> Optional[dict[str, Any]]:
    text = text.strip()
    if not text:
        return None
    try:
        value = json.loads(text)
        return value if isinstance(value, dict) else None
    except json.JSONDecodeError:
        pass
    start = text.find("{")
    end = text.rfind("}")
    if start >= 0 and end > start:
        try:
            value = json.loads(text[start : end + 1])
            return value if isinstance(value, dict) else None
        except json.JSONDecodeError:
            return None
    return None


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
    return {
        "portable": partial_origin_rows == 0,
        "origin_backed": partial_origin_rows > 0,
        "partial_origin_rows": partial_origin_rows,
        "override_rows": override_rows,
        "stored_bytes": stored_bytes,
        "materialized_rows": inspect.get("fs_materialized_rows"),
    }


def inspect_db(db_path: Path) -> dict[str, Any]:
    if not db_path.exists():
        return {"inspectable": False, "reason": "database file does not exist", "path": str(db_path)}

    try:
        conn = sqlite3.connect(f"file:{db_path}?mode=ro&immutable=1", uri=True)
        conn.execute("PRAGMA query_only = ON")
        try:
            result: dict[str, Any] = {"inspectable": True, "path": str(db_path)}
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
            result["fs_origin_rows"] = optional_count(conn, "fs_origin")
            result["fs_partial_origin_rows"] = optional_count(conn, "fs_partial_origin")
            result["fs_chunk_override_rows"] = optional_count(conn, "fs_chunk_override")
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
        return {"inspectable": False, "reason": str(exc), "path": str(db_path)}


def child_env(agentfs_bin: str, output_dir: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    env["AGENTFS_BIN"] = agentfs_bin
    child_tmp = output_dir / "child-tmp"
    child_tmp.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(child_tmp)
    env["TMP"] = str(child_tmp)
    env["TEMP"] = str(child_tmp)
    return env


def gate_required(args: argparse.Namespace, *, git_workload: bool = False) -> bool:
    if git_workload:
        return bool(args.full_gates or args.require_git_workload)
    return bool(args.full_gates)


def skipped_gate(name: str, reason: str, required: bool = False) -> dict[str, Any]:
    return {
        "name": name,
        "status": "skipped",
        "required": required,
        "reason": reason,
    }


def missing_script_gate(name: str, script: Path, required: bool) -> dict[str, Any]:
    return skipped_gate(name, f"script not found: {script}", required)


def run_json_script(
    name: str,
    script: Path,
    argv: list[str],
    repo_root: Path,
    env: dict[str, str],
    timeout: float,
    output_path: Path,
    required: bool,
) -> dict[str, Any]:
    if not script.is_file():
        return missing_script_gate(name, script, required)
    run = run_subprocess(argv + ["--output", str(output_path)], repo_root, env, timeout)
    payload = load_json(output_path) if output_path.exists() else None
    status = "passed" if run["returncode"] == 0 and isinstance(payload, dict) else "failed"
    return {
        "name": name,
        "status": status,
        "required": required,
        "run": run,
        "json_path": str(output_path),
        "result": payload,
    }


def gate_truth(payload: Any, keys: list[tuple[str, ...]]) -> Optional[bool]:
    if not isinstance(payload, dict):
        return None
    for path in keys:
        current: Any = payload
        for key in path:
            if not isinstance(current, dict) or key not in current:
                current = None
                break
            current = current[key]
        if isinstance(current, bool):
            return current
    return None


def recursive_key_values(value: Any, target_key: str) -> list[Any]:
    found: list[Any] = []
    if isinstance(value, dict):
        for key, child in value.items():
            if key == target_key:
                found.append(child)
            found.extend(recursive_key_values(child, target_key))
    elif isinstance(value, list):
        for child in value:
            found.extend(recursive_key_values(child, target_key))
    return found


def collect_db_paths(value: Any) -> list[Path]:
    paths: list[Path] = []
    if isinstance(value, dict):
        for key, child in value.items():
            if isinstance(child, str) and (
                key.endswith(("db_path", "database_path")) or child.endswith(".db")
            ):
                candidate = Path(child)
                if candidate.name.endswith(".db"):
                    paths.append(candidate)
            else:
                paths.extend(collect_db_paths(child))
    elif isinstance(value, list):
        for child in value:
            paths.extend(collect_db_paths(child))

    unique: list[Path] = []
    seen = set()
    for path in paths:
        text = str(path)
        if text not in seen:
            unique.append(path)
            seen.add(text)
    return unique


def threshold_for_phase(phase: str) -> Optional[float]:
    normalized = phase.lower().replace("-", "_")
    if normalized in GIT_PHASE_THRESHOLDS:
        return GIT_PHASE_THRESHOLDS[normalized]
    for key, threshold in GIT_PHASE_THRESHOLDS.items():
        if key in normalized:
            return threshold
    return None


def extract_phase_ratios(payload: Any) -> list[dict[str, Any]]:
    ratios: list[dict[str, Any]] = []

    def walk(value: Any, path: list[str]) -> None:
        if isinstance(value, dict):
            ratio_value = value.get("ratio")
            if isinstance(ratio_value, (int, float)):
                phase = path[-1] if path else "summary"
                threshold = threshold_for_phase(phase)
                ratios.append(
                    {
                        "phase": phase,
                        "ratio": float(ratio_value),
                        "threshold": threshold,
                        "passed": threshold is None or float(ratio_value) <= threshold,
                    }
                )
            for key, child in value.items():
                walk(child, path + [str(key)])
        elif isinstance(value, list):
            for index, child in enumerate(value):
                phase = None
                if isinstance(child, dict):
                    raw_phase = child.get("phase") or child.get("name") or child.get("mode")
                    if isinstance(raw_phase, str):
                        phase = raw_phase
                walk(child, path + [phase or str(index)])

    walk(payload, [])
    deduped: list[dict[str, Any]] = []
    seen = set()
    for item in ratios:
        key = (item["phase"], item["ratio"], item["threshold"])
        if key not in seen:
            deduped.append(item)
            seen.add(key)
    return deduped


def run_git_workload(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
    agentfs_bin: str,
) -> dict[str, Any]:
    script = (
        Path(args.git_workload_script).expanduser()
        if args.git_workload_script
        else repo_root / "scripts" / "validation" / "git-workload-benchmark.py"
    )
    if not script.is_absolute():
        script = (repo_root / script).resolve()
    required = gate_required(args, git_workload=True)
    if not script.is_file():
        return missing_script_gate("git_workload_benchmark", script, required)

    help_run = run_subprocess([sys.executable, str(script), "--help"], repo_root, env, 30)
    help_text = help_run.get("stdout_tail", "") + help_run.get("stderr_tail", "")

    output_path = output_dir / "git-workload-benchmark.json"
    argv = [sys.executable, str(script)]
    optional_args = [
        ("--agentfs-bin", agentfs_bin),
        ("--timeout", str(args.timeout)),
        ("--profile", None),
        ("--strict-portable", None),
    ]
    if args.full_gates:
        optional_args.append(("--full-gates", None))
    for flag, value in optional_args:
        if flag in help_text:
            argv.append(flag)
            if value is not None:
                argv.append(value)
    argv.extend(["--output", str(output_path)])

    run = run_subprocess(argv, repo_root, env, args.timeout * 4 + 60)
    payload = load_json(output_path) if output_path.exists() else None

    correctness_ok = gate_truth(
        payload,
        [
            ("summary", "passed"),
            ("correctness", "passed"),
            ("summary", "all_correct"),
        ],
    )
    if correctness_ok is None:
        correctness_ok = False

    base_unchanged_values = [
        value for value in recursive_key_values(payload, "agentfs_base_unchanged") if isinstance(value, bool)
    ]
    base_unchanged = all(base_unchanged_values) if base_unchanged_values else None

    db_inspections = [inspect_db(path) for path in collect_db_paths(payload)]
    inspected_db_count = sum(1 for item in db_inspections if item.get("inspectable"))
    partial_rows = [
        int(item.get("portability_status", {}).get("partial_origin_rows", 0) or 0)
        for item in db_inspections
        if item.get("inspectable")
    ]
    no_partial_rows = all(count == 0 for count in partial_rows) if partial_rows else None
    all_inspected_portable = (
        all(item.get("portability_status", {}).get("portable") is True for item in db_inspections)
        if inspected_db_count > 0
        else None
    )

    performance = extract_phase_ratios(payload)
    threshold_failures = [item for item in performance if item.get("passed") is False]
    phase_by_name = {str(item.get("phase")): item for item in performance}
    missing_required_phases = [
        phase
        for phase in REQUIRED_GIT_PHASES
        if not isinstance(phase_by_name.get(phase, {}).get("ratio"), (int, float))
    ]

    status = "passed" if run["returncode"] == 0 and isinstance(payload, dict) and correctness_ok else "failed"
    if args.full_gates:
        if (
            base_unchanged is not True
            or no_partial_rows is not True
            or all_inspected_portable is not True
            or inspected_db_count == 0
            or missing_required_phases
            or threshold_failures
        ):
            status = "failed"

    return {
        "name": "git_workload_benchmark",
        "status": status,
        "required": required,
        "run": run,
        "json_path": str(output_path),
        "result": payload,
        "probe": help_run,
        "gate": {
            "correctness_ok": correctness_ok,
            "base_unchanged": base_unchanged,
            "no_partial_origin_rows": no_partial_rows,
            "inspected_db_count": inspected_db_count,
            "all_inspected_portable": all_inspected_portable,
            "db_inspections": db_inspections,
            "performance_thresholds": performance,
            "threshold_failures": threshold_failures,
            "missing_required_phases": missing_required_phases,
            "strict_portable_policy": "partial-origin rows are forbidden for a passing full Git gate",
        },
    }


def run_strict_large_edit(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
) -> dict[str, Any]:
    script = repo_root / "scripts" / "validation" / "large-edit-benchmark.py"
    file_size = args.strict_file_size_mib or (200 if args.full_gates else 1)
    output_path = output_dir / "strict-large-edit.json"
    argv = [
        sys.executable,
        str(script),
        "--file-size-mib",
        str(file_size),
        "--timeout",
        str(args.timeout),
        "--no-partial-origin",
        "--profile",
        "--keep-temp",
    ]
    record = run_json_script(
        "strict_portable_large_edit",
        script,
        argv,
        repo_root,
        env,
        args.timeout * 2 + 60,
        output_path,
        gate_required(args),
    )
    payload = record.get("result")
    gate: dict[str, Any] = {}
    if isinstance(payload, dict):
        correctness = payload.get("correctness", {})
        inspect = payload.get("database", {}).get("inspect_after", {})
        portability = inspect.get("portability_status") or portability_status(inspect) if isinstance(inspect, dict) else {}
        native_seconds = payload.get("native", {}).get("duration_seconds")
        agentfs_seconds = payload.get("agentfs_overlay", {}).get("duration_seconds")
        ratio_value = (
            float(agentfs_seconds) / float(native_seconds)
            if isinstance(native_seconds, (int, float))
            and isinstance(agentfs_seconds, (int, float))
            and float(native_seconds) > 0
            else None
        )
        gate = {
            "correctness_passed": correctness.get("passed") is True,
            "base_unchanged": correctness.get("agentfs_base_unchanged") is True,
            "partial_origin_enabled": payload.get("agentfs", {}).get("partial_origin_enabled"),
            "partial_origin_rows": int(portability.get("partial_origin_rows", 0) or 0),
            "portable": portability.get("portable"),
            "ratio": ratio_value,
        }
        if record["status"] == "passed" and (
            gate["correctness_passed"] is not True
            or gate["base_unchanged"] is not True
            or gate["partial_origin_enabled"] is not False
            or gate["partial_origin_rows"] != 0
        ):
            record["status"] = "failed"
    record["gate"] = gate
    return record


def strict_db_path(record: dict[str, Any]) -> Optional[Path]:
    payload = record.get("result")
    if isinstance(payload, dict):
        raw = payload.get("agentfs", {}).get("db_path")
        if isinstance(raw, str):
            return Path(raw)
    return None


def run_strict_partial_rows_check(record: dict[str, Any], args: argparse.Namespace) -> dict[str, Any]:
    db_path = strict_db_path(record)
    if db_path is None:
        return skipped_gate(
            "strict_no_partial_origin_rows",
            "strict portable benchmark did not produce an AgentFS database path",
            gate_required(args),
        )
    inspect = inspect_db(db_path)
    partial_rows = int(inspect.get("portability_status", {}).get("partial_origin_rows", 1) or 0)
    return {
        "name": "strict_no_partial_origin_rows",
        "status": "passed" if inspect.get("inspectable") and partial_rows == 0 else "failed",
        "required": gate_required(args),
        "db_path": str(db_path),
        "inspect": inspect,
        "gate": {"partial_origin_rows": partial_rows},
    }


def run_integrity(
    name: str,
    db_path: Optional[Path],
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    agentfs_bin: str,
) -> dict[str, Any]:
    if db_path is None:
        return skipped_gate(name, "no database path available", gate_required(args))
    argv = [agentfs_bin, "integrity", str(db_path), "--json", "--require-portable"]
    run = run_subprocess(argv, repo_root, env, args.timeout, keep_stdout=True)
    report = parse_json_text(str(run.get("stdout", "")))
    ok = run["returncode"] == 0 and isinstance(report, dict) and report.get("ok") is True
    return {
        "name": name,
        "status": "passed" if ok else "failed",
        "required": gate_required(args),
        "run": {key: value for key, value in run.items() if key != "stdout"},
        "report": report,
        "gate": {
            "require_portable": True,
            "ok": report.get("ok") if isinstance(report, dict) else None,
            "portable": report.get("portable") if isinstance(report, dict) else None,
            "partial_origin_rows": report.get("partial_origin_rows") if isinstance(report, dict) else None,
        },
    }


def sidecar_status(db_path: Path) -> dict[str, Any]:
    wal = db_path.with_name(db_path.name + "-wal")
    if wal.exists() and wal.stat().st_size == 0:
        wal.unlink()
    shm = db_path.with_name(db_path.name + "-shm")
    if shm.exists():
        shm.unlink()

    sidecars = []
    for suffix in ("-wal", "-shm"):
        path = db_path.with_name(db_path.name + suffix)
        sidecars.append({"path": str(path), "exists": path.exists(), "bytes": path.stat().st_size if path.exists() else 0})
    no_nonempty_sidecars = all(int(item["bytes"]) == 0 for item in sidecars)
    return {
        "sidecars": sidecars,
        "single_main_db": no_nonempty_sidecars,
        "no_nonempty_sidecars": no_nonempty_sidecars,
        "strict_no_sidecar_files": all(not item["exists"] for item in sidecars),
    }


def run_backup(
    name: str,
    source_db: Optional[Path],
    target_db: Path,
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    agentfs_bin: str,
    *,
    materialize: bool,
) -> dict[str, Any]:
    if source_db is None:
        return skipped_gate(name, "no source database path available", gate_required(args))
    argv = [agentfs_bin, "backup", str(source_db), str(target_db), "--verify"]
    if materialize:
        argv.append("--materialize")
    run = run_subprocess(argv, repo_root, env, args.timeout * 2 + 30)
    sidecars = sidecar_status(target_db)
    inspect = inspect_db(target_db)
    portability = inspect.get("portability_status", {})
    ok = (
        run["returncode"] == 0
        and target_db.is_file()
        and inspect.get("inspectable") is True
        and portability.get("portable") is True
        and int(portability.get("partial_origin_rows", 1) or 0) == 0
        and sidecars["strict_no_sidecar_files"] is True
    )
    return {
        "name": name,
        "status": "passed" if ok else "failed",
        "required": gate_required(args),
        "run": run,
        "source_db": str(source_db),
        "target_db": str(target_db),
        "inspect_after": inspect,
        "sidecar_status": sidecars,
        "gate": {
            "verify": True,
            "materialize": materialize,
            "target_exists": target_db.is_file(),
            "portable": portability.get("portable"),
            "partial_origin_rows": portability.get("partial_origin_rows"),
            "single_main_db": sidecars["single_main_db"],
            "strict_no_sidecar_files": sidecars["strict_no_sidecar_files"],
        },
    }


def run_no_real_write(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
) -> dict[str, Any]:
    script = repo_root / "scripts" / "validation" / "partial-origin-no-real-write.py"
    file_size = args.no_real_write_file_size_mib or (200 if args.full_gates else 1)
    output_path = output_dir / "partial-origin-no-real-write.json"
    argv = [
        sys.executable,
        str(script),
        "--file-size-mib",
        str(file_size),
        "--timeout",
        str(args.timeout),
        "--profile",
    ]
    record = run_json_script(
        "partial_origin_no_real_write",
        script,
        argv,
        repo_root,
        env,
        args.timeout * 2 + 60,
        output_path,
        gate_required(args),
    )
    payload = record.get("result")
    if isinstance(payload, dict):
        correctness = payload.get("correctness", {})
        record["gate"] = {
            "correctness_passed": correctness.get("passed") is True,
            "base_sample_unchanged": correctness.get("base_sample_unchanged"),
            "base_metadata_unchanged": correctness.get("base_metadata_unchanged"),
            "partial_origin_rows_present": correctness.get("partial_origin_rows_present"),
            "override_rows_present": correctness.get("override_rows_present"),
        }
        if record["status"] == "passed" and correctness.get("passed") is not True:
            record["status"] = "failed"
    return record


def run_base_read(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
) -> dict[str, Any]:
    script = repo_root / "scripts" / "validation" / "base-read-benchmark.py"
    file_size = args.base_read_file_size_bytes or (1024 * 1024 if args.full_gates else 65536)
    iterations = args.base_read_iterations or (64 if args.full_gates else 8)
    read_bytes = args.base_read_bytes or (65536 if args.full_gates else 4096)
    output_path = output_dir / "base-read-benchmark.json"
    argv = [
        sys.executable,
        str(script),
        "--file-size-bytes",
        str(file_size),
        "--iterations",
        str(iterations),
        "--read-bytes",
        str(read_bytes),
        "--timeout",
        str(args.timeout),
        "--profile",
    ]
    record = run_json_script(
        "base_read_hash_and_cache",
        script,
        argv,
        repo_root,
        env,
        args.timeout * 2 + 60,
        output_path,
        gate_required(args),
    )
    payload = record.get("result")
    if isinstance(payload, dict):
        summary = payload.get("summary", {})
        ratio_value = summary.get("repeated_open_read_workload_ratio")
        threshold = 2.0 if args.full_gates else None
        gate = {
            "passed": summary.get("passed") is True,
            "repeated_open_read_workload_ratio": ratio_value,
            "repeated_open_read_threshold": threshold,
            "chunk_read_queries": summary.get("chunk_read_queries"),
            "chunk_read_chunks": summary.get("chunk_read_chunks"),
            "stale_reads": summary.get("stale_reads"),
            "base_unchanged": (
                payload.get("runs", {})
                .get("cache_invalidation", {})
                .get("base_file", {})
                .get("agentfs_base_unchanged")
            ),
        }
        if (
            record["status"] == "passed"
            and (
                gate["passed"] is not True
                or gate["chunk_read_queries"] != 0
                or gate["chunk_read_chunks"] != 0
                or gate["stale_reads"] != 0
                or gate["base_unchanged"] is not True
                or (
                    threshold is not None
                    and (not isinstance(ratio_value, (int, float)) or float(ratio_value) > threshold)
                )
            )
        ):
            record["status"] = "failed"
        record["gate"] = gate
    return record


def run_partial_setup(
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
) -> dict[str, Any]:
    script = repo_root / "scripts" / "validation" / "large-edit-benchmark.py"
    file_size = args.materialize_file_size_mib or (200 if args.full_gates else 1)
    output_path = output_dir / "partial-origin-materialize-setup.json"
    argv = [
        sys.executable,
        str(script),
        "--file-size-mib",
        str(file_size),
        "--timeout",
        str(args.timeout),
        "--partial-origin",
        "--profile",
        "--keep-temp",
    ]
    record = run_json_script(
        "partial_origin_materialize_setup",
        script,
        argv,
        repo_root,
        env,
        args.timeout * 2 + 60,
        output_path,
        gate_required(args),
    )
    payload = record.get("result")
    if isinstance(payload, dict):
        inspect = payload.get("database", {}).get("inspect_after", {})
        portability = inspect.get("portability_status") or portability_status(inspect) if isinstance(inspect, dict) else {}
        partial_rows = int(portability.get("partial_origin_rows", 0) or 0)
        correctness = payload.get("correctness", {})
        record["gate"] = {
            "correctness_passed": correctness.get("passed") is True,
            "partial_origin_enabled": payload.get("agentfs", {}).get("partial_origin_enabled"),
            "partial_origin_rows": partial_rows,
            "origin_backed": portability.get("origin_backed"),
        }
        if record["status"] == "passed" and (
            correctness.get("passed") is not True
            or payload.get("agentfs", {}).get("partial_origin_enabled") is not True
            or partial_rows <= 0
        ):
            record["status"] = "failed"
    return record


def run_materialize(
    source_record: dict[str, Any],
    args: argparse.Namespace,
    repo_root: Path,
    env: dict[str, str],
    output_dir: Path,
    agentfs_bin: str,
) -> dict[str, Any]:
    source_db = strict_db_path(source_record)
    if source_db is None:
        return skipped_gate("materialize_verify", "partial-origin setup did not produce a database path", gate_required(args))
    target_db = output_dir / "materialized.db"
    argv = [agentfs_bin, "materialize", str(source_db), "--output", str(target_db), "--verify"]
    run = run_subprocess(argv, repo_root, env, args.timeout * 2 + 30)
    sidecars = sidecar_status(target_db)
    inspect = inspect_db(target_db)
    portability = inspect.get("portability_status", {})
    ok = (
        run["returncode"] == 0
        and target_db.is_file()
        and inspect.get("inspectable") is True
        and portability.get("portable") is True
        and int(portability.get("partial_origin_rows", 1) or 0) == 0
        and sidecars["single_main_db"] is True
    )
    return {
        "name": "materialize_verify",
        "status": "passed" if ok else "failed",
        "required": gate_required(args),
        "run": run,
        "source_db": str(source_db),
        "target_db": str(target_db),
        "inspect_after": inspect,
        "sidecar_status": sidecars,
        "gate": {
            "verify": True,
            "target_exists": target_db.is_file(),
            "portable": portability.get("portable"),
            "partial_origin_rows": portability.get("partial_origin_rows"),
            "single_main_db": sidecars["single_main_db"],
        },
    }


def run_passed(record: dict[str, Any], args: argparse.Namespace) -> bool:
    if record.get("status") == "passed":
        return True
    if record.get("status") == "skipped":
        return not bool(record.get("required")) and not args.full_gates
    return False


def gate_summary(runs: dict[str, dict[str, Any]]) -> dict[str, Any]:
    return {
        name: {
            "status": record.get("status"),
            "required": record.get("required"),
            **({"gate": record.get("gate")} if "gate" in record else {}),
        }
        for name, record in runs.items()
    }


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    output_path = Path(args.output).expanduser() if args.output else default_output_path()

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        output_dir = Path(tempfile.mkdtemp(prefix="agentfs-phase7-validation-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="agentfs-phase7-validation-")
        output_dir = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    try:
        agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = child_env(agentfs_bin, output_dir)
        runs: dict[str, dict[str, Any]] = {}

        runs["git_workload_benchmark"] = run_git_workload(args, repo_root, env, output_dir, agentfs_bin)
        runs["strict_portable_large_edit"] = run_strict_large_edit(args, repo_root, env, output_dir)
        strict_db = strict_db_path(runs["strict_portable_large_edit"])
        runs["strict_no_partial_origin_rows"] = run_strict_partial_rows_check(
            runs["strict_portable_large_edit"], args
        )
        runs["strict_portable_integrity"] = run_integrity(
            "strict_portable_integrity", strict_db, args, repo_root, env, agentfs_bin
        )
        runs["strict_backup_verify"] = run_backup(
            "strict_backup_verify",
            strict_db,
            output_dir / "strict-backup.db",
            args,
            repo_root,
            env,
            agentfs_bin,
            materialize=False,
        )
        runs["partial_origin_no_real_write"] = run_no_real_write(args, repo_root, env, output_dir)
        runs["base_read_hash_and_cache"] = run_base_read(args, repo_root, env, output_dir)
        runs["partial_origin_materialize_setup"] = run_partial_setup(args, repo_root, env, output_dir)
        partial_db = strict_db_path(runs["partial_origin_materialize_setup"])
        runs["materialize_verify"] = run_materialize(
            runs["partial_origin_materialize_setup"], args, repo_root, env, output_dir, agentfs_bin
        )
        runs["backup_materialize_verify"] = run_backup(
            "backup_materialize_verify",
            partial_db,
            output_dir / "materialized-backup.db",
            args,
            repo_root,
            env,
            agentfs_bin,
            materialize=True,
        )

        failed = [name for name, record in runs.items() if record.get("status") == "failed"]
        skipped_required = [
            name
            for name, record in runs.items()
            if record.get("status") == "skipped" and record.get("required")
        ]
        failed_or_required_skipped = [
            name for name, record in runs.items() if not run_passed(record, args)
        ]
        if failed_or_required_skipped:
            exit_code = 1

        result = {
            "schema_version": 1,
            "benchmark": "phase7-validation-gates",
            "git_commit": git_commit(repo_root),
            "mode": "full" if args.full_gates else "smoke",
            "parameters": {
                "timeout_seconds": args.timeout,
                "strict_file_size_mib": args.strict_file_size_mib or (200 if args.full_gates else 1),
                "no_real_write_file_size_mib": args.no_real_write_file_size_mib
                or (200 if args.full_gates else 1),
                "materialize_file_size_mib": args.materialize_file_size_mib or (200 if args.full_gates else 1),
                "require_git_workload": bool(args.require_git_workload),
            },
            "agentfs": {"bin": agentfs_bin},
            "policy": {
                "full_mode_skipped_required_gates_fail": True,
                "git_workload_absent_policy": (
                    "skipped in smoke unless --require-git-workload is set; required in full"
                ),
                "strict_mode_forbids_partial_origin_rows": True,
                "strict_portable_integrity_command": "agentfs integrity --json --require-portable",
            "backup_outputs_must_not_depend_on_nonempty_wal_or_shm_sidecars": True,
            },
            "summary": {
                "passed": exit_code == 0,
                "failed_gates": failed,
                "skipped_gates": [
                    name for name, record in runs.items() if record.get("status") == "skipped"
                ],
                "skipped_required_gates": skipped_required,
                "gates": gate_summary(runs),
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
            "benchmark": "phase7-validation-gates",
            "mode": "full" if args.full_gates else "smoke",
            "error": str(exc),
            "output_dir": str(output_dir),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(payload, encoding="utf-8")
    sys.stdout.write(payload)
    print(f"Wrote Phase 7 validation JSON to {output_path}", file=sys.stderr)

    if temp_manager is not None:
        temp_manager.cleanup()

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
