#!/usr/bin/env python3
"""Phase 8 validation gate orchestrator.

Runs the Phase 8 correctness, principle, parallelism, crash-consistency, and
performance gates. The default mode is full Phase 8 policy; --smoke keeps the
same script plumbing but does not enforce Phase 8-only performance/parallel
targets.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any, Optional


def load_common() -> Any:
    common_path = Path(__file__).with_name("phase8-writeback-durability.py")
    spec = importlib.util.spec_from_file_location("phase8_writeback_durability_common", common_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load common helpers from {common_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


common = load_common()


def env_float(name: str, default: float) -> float:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    try:
        value = float(raw)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"{name} must be a float") from exc
    if value <= 0:
        raise argparse.ArgumentTypeError(f"{name} must be > 0")
    return value


# PHASE 8 TARGET: Git status/read_search/edit/diff must be <= 2.0x native.
# PHASE 8 TARGET: Git checkout must be <= 3.0x native.
# PHASE 8 TARGET: Git clone must be <= 5.0x native, with stretch target <= 3.0x.
# PHASE 8 TARGET: repeated read-only base workload must be <= 1.5x native.
PHASE8_TARGETS = {
    "clone": {
        "threshold": env_float("PHASE8_TARGET_CLONE", 5.0),
        "stretch": env_float("PHASE8_STRETCH_CLONE", 3.0),
    },
    "checkout": {
        "threshold": env_float("PHASE8_TARGET_CHECKOUT", 3.0),
        "stretch": env_float("PHASE8_STRETCH_CHECKOUT", 3.0),
    },
    "status": {
        "threshold": env_float("PHASE8_TARGET_STATUS", 2.0),
        "stretch": env_float("PHASE8_STRETCH_STATUS", 2.0),
    },
    "read_search": {
        "threshold": env_float("PHASE8_TARGET_READ_SEARCH", 2.0),
        "stretch": env_float("PHASE8_STRETCH_READ_SEARCH", 2.0),
    },
    "edit": {
        "threshold": env_float("PHASE8_TARGET_EDIT", 2.0),
        "stretch": env_float("PHASE8_STRETCH_EDIT", 2.0),
    },
    "diff": {
        "threshold": env_float("PHASE8_TARGET_DIFF", 2.0),
        "stretch": env_float("PHASE8_STRETCH_DIFF", 2.0),
    },
    "repeated-read": {
        "threshold": env_float("PHASE8_TARGET_REPEATED_READ", 1.5),
        "stretch": env_float("PHASE8_STRETCH_REPEATED_READ", 1.5),
    },
}


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Phase 8 validation gates and emit a final JSON report."
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--smoke", action="store_true", help="run smoke-sized gates without enforcing Phase 8 perf/parallel targets")
    mode.add_argument("--full", action="store_true", help="run full Phase 8 policy (default)")
    parser.add_argument(
        "--agentfs-bin",
        default=os.environ.get("AGENTFS_BIN"),
        help="agentfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=common.positive_float,
        default=common.positive_float(os.environ.get("PHASE8_VALIDATION_TIMEOUT", "120")),
        help="per-child-command timeout in seconds",
    )
    parser.add_argument("--keep-temp", action="store_true", default=common.env_flag("PHASE8_KEEP_TEMP"))
    parser.add_argument("--output", help="write final JSON result to this file")
    parser.add_argument("--json-indent", type=int, default=2)
    return parser.parse_args(argv)


def default_output_path() -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.gettempdir()) / f"agentfs-phase8-validation-{stamp}-{uuid.uuid4().hex[:8]}.json"


def load_json(path: Path) -> Optional[dict[str, Any]]:
    if not path.exists():
        return None
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None
    return value if isinstance(value, dict) else None


def git_commit(repo_root: Path) -> Optional[str]:
    return common.git_commit(repo_root)


def tool_path(name: str) -> Optional[str]:
    found = shutil.which(name)
    return found


def child_env(agentfs_bin: str, output_dir: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    env["AGENTFS_BIN"] = agentfs_bin
    child_tmp = output_dir / "tmp"
    child_tmp.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(child_tmp)
    env["TMP"] = str(child_tmp)
    env["TEMP"] = str(child_tmp)
    return env


def expected_json_missing(run: dict[str, Any], payload: Optional[dict[str, Any]]) -> bool:
    return payload is None


def run_json_gate(
    name: str,
    script: Path,
    args: list[str],
    repo_root: Path,
    env: dict[str, str],
    timeout: float,
    output_dir: Path,
    *,
    required: bool = True,
) -> dict[str, Any]:
    output_path = output_dir / f"{name}.json"
    argv = [sys.executable, str(script)] + args + ["--output", str(output_path)]
    if not script.is_file():
        return {
            "name": name,
            "status": "failed" if required else "skipped",
            "required": required,
            "reason": f"script not found: {script}",
            "json_path": str(output_path),
            "json_present": False,
        }
    run = common.run_subprocess(argv, repo_root, env, timeout)
    payload = load_json(output_path)
    missing_json = expected_json_missing(run, payload)
    summary_passed = payload.get("summary", {}).get("passed") if isinstance(payload, dict) else None
    passed = run["returncode"] == 0 and not missing_json and summary_passed is not False
    return {
        "name": name,
        "status": "passed" if passed else "failed",
        "required": required,
        "run": run,
        "json_path": str(output_path),
        "json_present": not missing_json,
        "expected_json_missing": missing_json,
        "result": payload,
        "summary": payload.get("summary") if isinstance(payload, dict) else None,
    }


def ratio_value(value: Any) -> Optional[float]:
    if isinstance(value, (int, float)):
        return float(value)
    return None


def phase_check(phase: str, ratio: Optional[float], enforced: bool) -> dict[str, Any]:
    target = PHASE8_TARGETS[phase]
    threshold = float(target["threshold"])
    stretch = float(target["stretch"])
    return {
        "phase": phase,
        "ratio": ratio,
        "threshold": threshold,
        "stretch": stretch,
        "passed": ratio is not None and ratio <= threshold,
        "stretch_passed": ratio is not None and ratio <= stretch,
        "enforced": enforced,
    }


def git_performance_checks(payload: Optional[dict[str, Any]], enforced: bool) -> list[dict[str, Any]]:
    ratios = payload.get("summary", {}).get("phase_ratios", {}) if isinstance(payload, dict) else {}
    checks = []
    for phase in ("clone", "checkout", "status", "read_search", "edit", "diff"):
        phase_payload = ratios.get(phase, {}) if isinstance(ratios, dict) else {}
        checks.append(phase_check(phase, ratio_value(phase_payload.get("ratio")), enforced))
    return checks


def base_read_performance_checks(payload: Optional[dict[str, Any]], enforced: bool) -> list[dict[str, Any]]:
    summary = payload.get("summary", {}) if isinstance(payload, dict) else {}
    return [phase_check("repeated-read", ratio_value(summary.get("repeated_open_read_workload_ratio")), enforced)]


def apply_performance_policy(
    gate: dict[str, Any],
    checks: list[dict[str, Any]],
    *,
    enforce: bool,
) -> None:
    gate["phase8_performance_checks"] = checks
    failures = [item for item in checks if item["passed"] is not True]
    gate["phase8_threshold_failures"] = failures
    if enforce and failures:
        gate["status"] = "failed"


def max_counter(payload: Optional[dict[str, Any]], key: str) -> Optional[int]:
    if not isinstance(payload, dict):
        return None
    candidates: list[Any] = []
    candidates.append(payload.get("summary", {}).get(key))
    candidates.append(payload.get("agentfs", {}).get("profile_counters", {}).get(key))
    candidates.append(payload.get("agentfs", {}).get("profile_counters", {}).get("max_counters", {}).get(key))
    for item in candidates:
        if isinstance(item, int):
            return item
    return None


def apply_parallel_policy(gate: dict[str, Any], *, enforce: bool) -> None:
    payload = gate.get("result")
    read_max = max_counter(payload, "fuse_read_lane_max_concurrent")
    dispatch_max = max_counter(payload, "fuse_dispatch_max_concurrent")
    checks = [
        {
            "field": "fuse_read_lane_max_concurrent",
            "value": read_max,
            "required_min_exclusive": 1,
            "passed": isinstance(read_max, int) and read_max > 1,
            "enforced": enforce,
        },
        {
            "field": "fuse_dispatch_max_concurrent",
            "value": dispatch_max,
            "required_min_exclusive": 1,
            "passed": isinstance(dispatch_max, int) and dispatch_max > 1,
            "enforced": enforce,
        },
    ]
    gate["phase8_parallel_checks"] = checks
    failures = [item for item in checks if item["passed"] is not True]
    gate["phase8_parallel_failures"] = failures
    if enforce and failures:
        gate["status"] = "failed"


def gate_passed(record: dict[str, Any]) -> bool:
    if record.get("status") == "passed":
        return True
    return False


def gate_summary(gates: dict[str, dict[str, Any]]) -> dict[str, Any]:
    summary: dict[str, Any] = {}
    for name, gate in gates.items():
        item: dict[str, Any] = {
            "status": gate.get("status"),
            "required": gate.get("required"),
            "json_present": gate.get("json_present"),
        }
        if "phase8_threshold_failures" in gate:
            item["phase8_threshold_failures"] = gate["phase8_threshold_failures"]
        if "phase8_parallel_failures" in gate:
            item["phase8_parallel_failures"] = gate["phase8_parallel_failures"]
        if "summary" in gate:
            item["summary"] = gate["summary"]
        summary[name] = item
    return summary


def print_readable_summary(result: dict[str, Any]) -> None:
    summary = result.get("summary", {})
    status = "PASS" if summary.get("passed") else "FAIL"
    print(f"Phase 8 validation {result.get('mode')} summary: {status}", file=sys.stderr)
    for name, gate in result.get("gates", {}).items():
        print(f"  - {name}: {gate.get('status')}", file=sys.stderr)
    failures = summary.get("threshold_failures") or []
    if failures:
        print("  Performance threshold failures:", file=sys.stderr)
        for item in failures:
            print(
                f"    {item.get('phase')}: ratio={item.get('ratio')} "
                f"threshold={item.get('threshold')} stretch={item.get('stretch')}",
                file=sys.stderr,
            )


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    mode = "smoke" if args.smoke else "full"
    enforce_phase8 = mode == "full"
    output_path = Path(args.output).expanduser() if args.output else default_output_path()

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        output_dir = Path(tempfile.mkdtemp(prefix="agentfs-phase8-validation-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(
            prefix="agentfs-phase8-validation-",
            ignore_cleanup_errors=True,
        )
        output_dir = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    try:
        agentfs_bin = common.resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = child_env(agentfs_bin, output_dir)
        scripts = repo_root / "scripts" / "validation"
        gates: dict[str, dict[str, Any]] = {}

        gates["phase7_validation_smoke"] = run_json_gate(
            "phase7-validation-smoke",
            scripts / "phase7-validation.py",
            ["--smoke", "--timeout", str(args.timeout), "--agentfs-bin", agentfs_bin],
            repo_root,
            env,
            args.timeout * 6 + 120,
            output_dir,
        )

        git_args = ["--timeout", str(args.timeout), "--agentfs-bin", agentfs_bin, "--profile"]
        if args.smoke:
            git_args.extend(
                [
                    "--fixture-files",
                    "12",
                    "--fixture-dirs",
                    "3",
                    "--fixture-file-size-bytes",
                    "512",
                    "--read-files",
                    "8",
                    "--read-bytes",
                    "512",
                    "--edit-files",
                    "2",
                    "--skip-fsck",
                ]
            )
        else:
            git_args.append("--require-performance")
        gates["git_workload_phase8_thresholds"] = run_json_gate(
            "git-workload-phase8-thresholds",
            scripts / "git-workload-benchmark.py",
            git_args,
            repo_root,
            env,
            args.timeout * 3 + 60,
            output_dir,
        )
        apply_performance_policy(
            gates["git_workload_phase8_thresholds"],
            git_performance_checks(gates["git_workload_phase8_thresholds"].get("result"), enforce_phase8),
            enforce=enforce_phase8,
        )

        fuse_args = ["--timeout", str(args.timeout), "--agentfs-bin", agentfs_bin, "--profile"]
        if args.smoke:
            fuse_args.extend(["--files", "4", "--file-size-bytes", "1024", "--threads", "2", "--iterations", "4", "--read-bytes", "512"])
        gates["fuse_serialization_parallelism"] = run_json_gate(
            "fuse-serialization-parallelism",
            scripts / "fuse-serialization-stress.py",
            fuse_args,
            repo_root,
            env,
            args.timeout * 2 + 30,
            output_dir,
        )
        apply_parallel_policy(gates["fuse_serialization_parallelism"], enforce=enforce_phase8)

        concurrent_args = ["--timeout", str(args.timeout), "--agentfs-bin", agentfs_bin]
        if args.smoke:
            concurrent_args.extend(["--fixture-files", "12", "--fixture-dirs", "3", "--fixture-file-size-bytes", "512", "--edit-files", "2", "--append-bytes", "32"])
        gates["phase8_concurrent_git_stress"] = run_json_gate(
            "phase8-concurrent-git-stress",
            scripts / "phase8-concurrent-git-stress.py",
            concurrent_args,
            repo_root,
            env,
            args.timeout * 2 + 30,
            output_dir,
        )

        durability_args = ["--timeout", str(args.timeout), "--agentfs-bin", agentfs_bin]
        no_fsync_args = ["--timeout", str(args.timeout), "--agentfs-bin", agentfs_bin]
        if args.smoke:
            durability_args.extend(["--write-bytes", "1024"])
            no_fsync_args.extend(["--write-bytes", "1024"])
        gates["phase8_writeback_durability"] = run_json_gate(
            "phase8-writeback-durability",
            scripts / "phase8-writeback-durability.py",
            durability_args,
            repo_root,
            env,
            args.timeout * 2 + 30,
            output_dir,
        )
        gates["phase8_writeback_no_fsync_crash"] = run_json_gate(
            "phase8-writeback-no-fsync-crash",
            scripts / "phase8-writeback-no-fsync-crash.py",
            no_fsync_args,
            repo_root,
            env,
            args.timeout * 2 + 30,
            output_dir,
        )

        base_read_args = ["--timeout", str(args.timeout), "--agentfs-bin", agentfs_bin, "--profile"]
        if args.smoke:
            base_read_args.extend(["--file-size-bytes", "65536", "--iterations", "4", "--read-bytes", "4096"])
        else:
            base_read_args.extend(["--file-size-bytes", "1048576", "--iterations", "64", "--read-bytes", "65536"])
        gates["base_read_repeated_read_threshold"] = run_json_gate(
            "base-read-repeated-read-threshold",
            scripts / "base-read-benchmark.py",
            base_read_args,
            repo_root,
            env,
            args.timeout * 2 + 60,
            output_dir,
        )
        apply_performance_policy(
            gates["base_read_repeated_read_threshold"],
            base_read_performance_checks(gates["base_read_repeated_read_threshold"].get("result"), enforce_phase8),
            enforce=enforce_phase8,
        )

        failed_gates = [name for name, gate in gates.items() if not gate_passed(gate)]
        threshold_failures = []
        parallel_failures = []
        for gate in gates.values():
            threshold_failures.extend(
                item
                for item in gate.get("phase8_threshold_failures", [])
                if item.get("enforced") and item.get("passed") is not True
            )
            parallel_failures.extend(
                item
                for item in gate.get("phase8_parallel_failures", [])
                if item.get("enforced") and item.get("passed") is not True
            )
        missing_json_gates = [name for name, gate in gates.items() if gate.get("expected_json_missing")]
        if failed_gates:
            exit_code = 1

        result = {
            "schema_version": 1,
            "benchmark": "phase8-validation-gates",
            "git_commit": git_commit(repo_root),
            "mode": mode,
            "parameters": {
                "timeout_seconds": args.timeout,
                "phase8_perf_parallel_enforced": enforce_phase8,
            },
            "summary": {
                "passed": exit_code == 0,
                "failed_gates": failed_gates,
                "missing_json_gates": missing_json_gates,
                "threshold_failures": threshold_failures,
                "parallel_failures": parallel_failures,
                "gates": gate_summary(gates),
            },
            "gates": gates,
            "env": {
                "python": sys.executable,
                "agentfs_bin": agentfs_bin,
                "git": tool_path("git"),
                "fusermount3": tool_path("fusermount3"),
                "fusermount": tool_path("fusermount"),
                "mountpoint": tool_path("mountpoint"),
                "phase8_targets": PHASE8_TARGETS,
                "override_env": {
                    key: os.environ.get(key)
                    for key in sorted(
                        set(
                            [
                                "PHASE8_TARGET_CLONE",
                                "PHASE8_STRETCH_CLONE",
                                "PHASE8_TARGET_CHECKOUT",
                                "PHASE8_TARGET_STATUS",
                                "PHASE8_TARGET_READ_SEARCH",
                                "PHASE8_TARGET_EDIT",
                                "PHASE8_TARGET_DIFF",
                                "PHASE8_TARGET_REPEATED_READ",
                            ]
                        )
                    )
                    if os.environ.get(key) is not None
                },
            },
            "output_dir": str(output_dir),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "phase8-validation-gates",
            "mode": mode,
            "summary": {"passed": False, "failed_gates": ["orchestrator_exception"]},
            "gates": {},
            "env": {
                "python": sys.executable,
                "git": tool_path("git"),
                "fusermount3": tool_path("fusermount3"),
                "fusermount": tool_path("fusermount"),
                "mountpoint": tool_path("mountpoint"),
                "phase8_targets": PHASE8_TARGETS,
            },
            "error": str(exc),
            "output_dir": str(output_dir),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }

    print_readable_summary(result)
    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(payload, encoding="utf-8")
    sys.stdout.write(payload)
    print(f"Wrote Phase 8 validation JSON to {output_path}", file=sys.stderr)

    if temp_manager is not None:
        temp_manager.cleanup()
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
