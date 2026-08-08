#!/usr/bin/env python3
"""Measure process spawn through the first successful Vfs filesystem probe."""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
import uuid
from pathlib import Path
from typing import Any, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib import common  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
PROBE = r"""
import os
import time

started = time.monotonic_ns()
stat = os.stat("probe.txt")
completed = time.monotonic_ns()
print(f"{started} {completed} {stat.st_size}", flush=True)
"""


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", type=common.positive_int, default=20)
    parser.add_argument(
        "--warmup",
        type=common.non_negative_int,
        default=1,
        help="leading native/transport samples to run and discard",
    )
    parser.add_argument(
        "--transport",
        choices=("both", "legacy", "uring"),
        default="both",
        help="Vfs FUSE transport to measure (default: both)",
    )
    parser.add_argument(
        "--vfs-bin",
        default=os.environ.get("VFS_BIN"),
        help="vfs executable path/name (default: repo target binary)",
    )
    parser.add_argument("--timeout", type=common.positive_float, default=30.0)
    parser.add_argument(
        "--profile",
        action="store_true",
        default=common.env_flag("VFS_PROFILE"),
        help="enable Vfs transport counters; use for diagnosis, not final timings",
    )
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=common.env_flag("MOUNT_STARTUP_KEEP_TEMP"),
    )
    parser.add_argument("--output", help="write the JSON payload to this path")
    parser.add_argument("--json-indent", type=common.non_negative_int, default=2)
    return parser.parse_args(argv)


def parse_probe(run: dict[str, Any]) -> Optional[dict[str, float | int]]:
    text = str(run.get("stdout", "")).strip()
    if not text:
        return None
    fields = text.splitlines()[-1].split()
    if len(fields) != 3:
        return None
    try:
        request_started_ns, request_completed_ns, size = map(int, fields)
    except ValueError:
        return None
    process_started_ns = run.get("started_perf_counter_ns")
    if (
        not isinstance(process_started_ns, int)
        or request_started_ns < process_started_ns
        or request_completed_ns < request_started_ns
    ):
        return None
    return {
        "process_to_request_start_seconds": (request_started_ns - process_started_ns)
        / 1_000_000_000,
        "process_to_first_request_seconds": (request_completed_ns - process_started_ns)
        / 1_000_000_000,
        "first_request_service_seconds": (request_completed_ns - request_started_ns)
        / 1_000_000_000,
        "observed_size": size,
    }


def compact_run(run: dict[str, Any]) -> dict[str, Any]:
    return {
        key: value
        for key, value in run.items()
        if key
        not in {
            "stdout",
            "started_perf_counter_ns",
            "finished_perf_counter_ns",
        }
    }


def render_summary(result: dict[str, Any]) -> str:
    lines = [f"mount startup: {'PASS' if result.get('passed') else 'FAIL'}"]
    for leg, stats in result.get("absolute_startup_seconds", {}).items():
        lines.append(
            f"  {leg:6s} median {stats.get('median', float('nan')):.4f}s "
            f"(p25 {stats.get('p25', float('nan')):.4f}, "
            f"p75 {stats.get('p75', float('nan')):.4f}, n={stats.get('n', 0)})"
        )
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if sys.platform != "linux":
        raise RuntimeError("mount startup transport benchmark requires Linux FUSE")

    if args.keep_temp:
        temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
        temp_root = Path(tempfile.mkdtemp(prefix="vfs-mount-startup-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(
            prefix="vfs-mount-startup-", ignore_cleanup_errors=True
        )
        temp_root = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    git_ai_before = common.git_ai_processes()
    try:
        vfs_bin = common.resolve_vfs_bin(args.vfs_bin, REPO_ROOT)
        base_env = os.environ.copy()
        base_env.setdefault("NO_COLOR", "1")
        base_env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
        if args.profile:
            base_env["VFS_PROFILE"] = "1"
        else:
            base_env.pop("VFS_PROFILE", None)

        transports = (
            ["legacy", "uring"] if args.transport == "both" else [args.transport]
        )
        samples: list[dict[str, Any]] = []
        values: dict[str, list[float]] = {
            "native": [],
            **{transport: [] for transport in transports},
        }
        service_values: dict[str, list[float]] = {leg: [] for leg in values}
        schedule: list[str] = []
        total_sample_count = args.warmup + args.samples
        for sample_index in range(total_sample_count):
            warmup = sample_index < args.warmup
            ordered_transports = (
                transports if sample_index % 2 == 0 else list(reversed(transports))
            )
            for leg in ["native", *ordered_transports]:
                if not warmup:
                    schedule.append(leg)
                context = temp_root / "runs" / f"{sample_index:03d}-{leg}"
                fixture = context / "fixture"
                fixture.mkdir(parents=True)
                (fixture / "probe.txt").write_bytes(b"vfs-startup-probe\n")
                env = common.isolate_benchmark_env(base_env, context)
                session = f"startup-{sample_index}-{leg}-{uuid.uuid4().hex}"
                command = [common.sandbox_python(), "-S", "-c", PROBE]
                if leg != "native":
                    env["VFS_FUSE_URING"] = "0" if leg == "legacy" else "1"
                    command = [
                        vfs_bin,
                        "run",
                        "--session",
                        session,
                        "--no-default-allows",
                        "--partial-origin",
                        "on",
                        "--",
                        *command,
                    ]
                run = common.run_subprocess(
                    command,
                    fixture,
                    env,
                    args.timeout,
                    keep_stdout=True,
                    include_timing_origin=True,
                )
                timing = parse_probe(run)
                cleanup = (
                    common.wait_for_benchmark_cleanup(session, context)
                    if leg != "native"
                    else {
                        "ok": True,
                        "clean_exit": True,
                        "processes": [],
                        "mounts": [],
                        "waited_seconds": 0.0,
                    }
                )
                passed = (
                    run["returncode"] == 0
                    and timing is not None
                    and timing["observed_size"] == len(b"vfs-startup-probe\n")
                    and cleanup["ok"] is True
                )
                if timing is not None and not warmup:
                    values[leg].append(
                        float(timing["process_to_first_request_seconds"])
                    )
                    service_values[leg].append(
                        float(timing["first_request_service_seconds"])
                    )
                samples.append(
                    {
                        "sample": sample_index,
                        "warmup": warmup,
                        "leg": leg,
                        "session": session if leg != "native" else None,
                        "timing": timing,
                        "outer_seconds": run["duration_seconds"],
                        "run": compact_run(run),
                        "cleanup": cleanup,
                        "profile": (
                            common.summarize_profile_counters(
                                run.get("profile_summaries", [])
                            )
                            if leg != "native" and args.profile
                            else None
                        ),
                        "passed": passed,
                    }
                )
                value = (
                    float(timing["process_to_first_request_seconds"])
                    if timing is not None
                    else float("nan")
                )
                print(
                    f"[{sample_index + 1}/{total_sample_count}] "
                    f"{'warmup ' if warmup else ''}{leg}: "
                    f"{value:.4f}s, {'PASS' if passed else 'FAIL'}",
                    file=sys.stderr,
                    flush=True,
                )

        leaked_git_ai = common.git_ai_leaks(git_ai_before, common.git_ai_processes())
        absolute = {
            leg: common.summarize_floats(leg_values)
            for leg, leg_values in values.items()
        }
        service = {
            leg: common.summarize_floats(leg_values)
            for leg, leg_values in service_values.items()
        }
        native_median = absolute["native"].get("median")
        derived = {
            f"{transport}_over_native_median": (
                float(absolute[transport]["median"]) / float(native_median)
                if isinstance(native_median, (int, float))
                and native_median > 0
                and isinstance(absolute[transport].get("median"), (int, float))
                else None
            )
            for transport in transports
        }
        transport_attestation = {
            transport: {
                "fuse_uring_requests_max": max(
                    (
                        int(
                            (sample.get("profile") or {})
                            .get("max_counters", {})
                            .get("fuse_uring_requests", 0)
                        )
                        for sample in samples
                        if sample["leg"] == transport
                    ),
                    default=0,
                ),
            }
            for transport in transports
        }
        for transport, attestation in transport_attestation.items():
            attestation["verified"] = (
                (
                    attestation["fuse_uring_requests_max"] == 0
                    if transport == "legacy"
                    else attestation["fuse_uring_requests_max"] > 0
                )
                if args.profile
                else None
            )
        passed = (
            all(sample["passed"] for sample in samples)
            and all(len(leg_values) == args.samples for leg_values in values.values())
            and (
                not args.profile
                or all(
                    attestation["verified"] is True
                    for attestation in transport_attestation.values()
                )
            )
            and not leaked_git_ai
        )
        kernel_uring = Path("/sys/module/fuse/parameters/enable_uring")
        result = {
            "schema_version": 1,
            "benchmark": "mount-startup",
            "git_commit": common.git_commit(REPO_ROOT),
            "parameters": {
                "samples": args.samples,
                "warmup_samples_discarded": args.warmup,
                "transports": transports,
                "timeout_seconds": args.timeout,
                "profile": args.profile,
            },
            "engine": {
                "bin": vfs_bin,
                "version": common.run_subprocess(
                    [vfs_bin, "version", "--json"],
                    temp_root,
                    base_env,
                    args.timeout,
                    keep_stdout=True,
                )
                .get("stdout", "")
                .strip(),
                "kernel_fuse_uring_enabled": (
                    kernel_uring.read_text(encoding="utf-8").strip()
                    if kernel_uring.is_file()
                    else None
                ),
            },
            "measurement": {
                "schedule": schedule,
                "primary_metric": (
                    "parent process spawn through completion of the child probe's "
                    "first successful stat of probe.txt"
                ),
                "native_role": (
                    "same Python probe without Vfs; exposes process and interpreter overhead"
                ),
                "transport_order": "alternated per sample when both are measured",
                "transport_labels": (
                    "requested transport; effective channel is verified only when "
                    "--profile records transport counters"
                ),
                "performance_threshold": None,
            },
            "absolute_startup_seconds": absolute,
            "absolute_first_request_service_seconds": service,
            "transport_attestation": {
                "profile_enabled": args.profile,
                "transports": transport_attestation,
            },
            "derived": {
                **derived,
                "warning": (
                    "derived ratios include the common native process baseline; "
                    "interpret beside absolute distributions"
                ),
            },
            "runs": samples,
            "git_ai_census": {
                "pre_existing": len(git_ai_before),
                "leaked": leaked_git_ai,
            },
            "passed": passed,
            "temp_dir": str(temp_root),
            "kept_temp": args.keep_temp,
        }
        if not passed:
            exit_code = 1
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "mount-startup",
            "error": str(exc),
            "passed": False,
            "temp_dir": str(temp_root),
            "kept_temp": args.keep_temp,
        }

    print(render_summary(result), file=sys.stderr)
    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    if args.output:
        output = Path(args.output).expanduser()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(payload, encoding="utf-8")
        print(f"Wrote mount startup JSON to {output}", file=sys.stderr)
    else:
        sys.stdout.write(payload)
    if temp_manager is not None:
        temp_manager.cleanup()
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
