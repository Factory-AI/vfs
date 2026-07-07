#!/usr/bin/env python3
"""Multi-iteration wrapper around git-workload-benchmark.py.

Single-shot benchmark runs are noisy (page cache, scheduler, disk activity).
This wrapper runs the underlying benchmark N times and reports median +
percentile statistics per phase, so we can make confident before/after
comparisons when tuning AgentFS.

The wrapper is intentionally non-invasive: it shells out to the existing
benchmark with --output, parses each JSON, and aggregates. Pass-through of
unknown args means it stays in sync as the benchmark grows new flags.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib import common  # noqa: E402


def percentile(values: list[float], q: float) -> float:
    if not values:
        return float("nan")
    sorted_values = sorted(values)
    if len(sorted_values) == 1:
        return sorted_values[0]
    pos = (len(sorted_values) - 1) * q
    lo = int(pos)
    hi = min(lo + 1, len(sorted_values) - 1)
    frac = pos - lo
    return sorted_values[lo] * (1 - frac) + sorted_values[hi] * frac


def summarize_floats(values: list[float]) -> dict[str, float | int]:
    cleaned = [v for v in values if isinstance(v, (int, float))]
    if not cleaned:
        return {"count": 0}
    return {
        "count": len(cleaned),
        "min": min(cleaned),
        "max": max(cleaned),
        "median": statistics.median(cleaned),
        "p25": percentile(cleaned, 0.25),
        "p75": percentile(cleaned, 0.75),
        "mean": statistics.mean(cleaned),
        "stdev": statistics.stdev(cleaned) if len(cleaned) > 1 else 0.0,
    }


def aggregate(runs: list[dict[str, Any]]) -> dict[str, Any]:
    overall_ratios: list[float] = []
    native_totals: list[float] = []
    agentfs_totals: list[float] = []
    phase_natives: dict[str, list[float]] = {}
    phase_agentfs: dict[str, list[float]] = {}
    phase_ratios: dict[str, list[float]] = {}

    for run in runs:
        summary = run.get("summary") or {}
        ratio = summary.get("ratio")
        if isinstance(ratio, (int, float)):
            overall_ratios.append(float(ratio))
        n = summary.get("native_seconds")
        a = summary.get("agentfs_seconds")
        if isinstance(n, (int, float)):
            native_totals.append(float(n))
        if isinstance(a, (int, float)):
            agentfs_totals.append(float(a))

        for phase, payload in (summary.get("phase_ratios") or {}).items():
            if not isinstance(payload, dict):
                continue
            nv = payload.get("native_seconds")
            av = payload.get("agentfs_seconds")
            rv = payload.get("ratio")
            if isinstance(nv, (int, float)):
                phase_natives.setdefault(phase, []).append(float(nv))
            if isinstance(av, (int, float)):
                phase_agentfs.setdefault(phase, []).append(float(av))
            if isinstance(rv, (int, float)):
                phase_ratios.setdefault(phase, []).append(float(rv))

    phase_stats: dict[str, Any] = {}
    for phase in sorted(set(phase_natives) | set(phase_agentfs) | set(phase_ratios)):
        phase_stats[phase] = {
            "native_seconds": summarize_floats(phase_natives.get(phase, [])),
            "agentfs_seconds": summarize_floats(phase_agentfs.get(phase, [])),
            "ratio": summarize_floats(phase_ratios.get(phase, [])),
        }

    return {
        "iterations": len(runs),
        "overall": {
            "native_seconds": summarize_floats(native_totals),
            "agentfs_seconds": summarize_floats(agentfs_totals),
            "ratio": summarize_floats(overall_ratios),
        },
        "phases": phase_stats,
    }


def run_one(forward_argv: list[str], output_path: Path, agentfs_bin: str | None) -> dict[str, Any]:
    benchmark = Path(__file__).resolve().with_name("git-workload-benchmark.py")
    argv = [sys.executable, str(benchmark), "--output", str(output_path)] + forward_argv
    env = os.environ.copy()
    if agentfs_bin is not None:
        env["AGENTFS_BIN"] = agentfs_bin
    started = time.perf_counter()
    proc = subprocess.run(argv, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    duration = time.perf_counter() - started
    payload: dict[str, Any] = {
        "argv": argv,
        "wall_seconds": duration,
        "returncode": proc.returncode,
    }
    if output_path.exists():
        try:
            payload["result"] = json.loads(output_path.read_text())
        except Exception as exc:
            payload["result_error"] = str(exc)
    if proc.returncode != 0:
        payload["stderr_tail"] = (proc.stderr or "").splitlines()[-20:]
    return payload


def format_seconds(value: float) -> str:
    return f"{value:.3f}s" if value < 1 else f"{value:.2f}s"


def render_human(label: str, agg: dict[str, Any]) -> str:
    out: list[str] = []
    overall = agg["overall"]
    n = overall["native_seconds"]
    a = overall["agentfs_seconds"]
    r = overall["ratio"]
    head = (
        f"=== {label} (iterations={agg['iterations']}) ===\n"
        f"  native  median={format_seconds(n.get('median', float('nan')))}"
        f" [p25={format_seconds(n.get('p25', float('nan')))}, p75={format_seconds(n.get('p75', float('nan')))}]\n"
        f"  agentfs median={format_seconds(a.get('median', float('nan')))}"
        f" [p25={format_seconds(a.get('p25', float('nan')))}, p75={format_seconds(a.get('p75', float('nan')))}]\n"
        f"  ratio   median={r.get('median', float('nan')):.2f}x"
        f" [p25={r.get('p25', float('nan')):.2f}x, p75={r.get('p75', float('nan')):.2f}x]"
        f" stdev={r.get('stdev', float('nan')):.2f}x"
    )
    out.append(head)
    out.append("  phase ratios (median):")
    for phase, stats in agg["phases"].items():
        r = stats["ratio"]
        nv = stats["native_seconds"]
        av = stats["agentfs_seconds"]
        if r.get("count", 0) == 0:
            continue
        out.append(
            f"    {phase:<14s} native={format_seconds(nv['median'])}"
            f" agentfs={format_seconds(av['median'])}"
            f" ratio={r['median']:.2f}x"
            f" (p25={r['p25']:.2f}x p75={r['p75']:.2f}x)"
        )
    return "\n".join(out)


def parse_args(argv: list[str]) -> tuple[argparse.Namespace, list[str]]:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--label",
        default="benchmark",
        help="human-readable label used in summary output",
    )
    parser.add_argument(
        "--iterations",
        type=int,
        default=5,
        help="number of measurement iterations (default: 5)",
    )
    parser.add_argument(
        "--warmup",
        type=int,
        default=1,
        help="number of warmup iterations whose results are discarded (default: 1)",
    )
    parser.add_argument(
        "--agentfs-bin",
        default=os.environ.get("AGENTFS_BIN"),
        help="override AGENTFS_BIN for the underlying benchmark",
    )
    parser.add_argument(
        "--output",
        help="write aggregated JSON to this path (default: stdout)",
    )
    parser.add_argument(
        "--keep-iterations",
        action="store_true",
        help="keep per-iteration JSON files alongside --output",
    )
    args, forward = parser.parse_known_args(argv)
    forward = [token for token in forward if token != "--"]
    return args, forward


def main(argv: list[str]) -> int:
    args, forward = parse_args(argv)
    if args.iterations < 1:
        print("--iterations must be >= 1", file=sys.stderr)
        return 2
    if args.warmup < 0:
        print("--warmup must be >= 0", file=sys.stderr)
        return 2

    output_path = Path(args.output).expanduser().resolve() if args.output else None
    persist_dir: Path | None = None
    if output_path is not None and args.keep_iterations:
        persist_dir = output_path.with_suffix(output_path.suffix + ".iterations")
        persist_dir.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="gw-bench-multi-") as tmpdir:
        tmp_root = Path(tmpdir)

        git_ai_before = common.git_ai_processes()
        common.pin_distro_git(os.environ, tmp_root)

        warmup_runs: list[dict[str, Any]] = []
        for i in range(args.warmup):
            out_path = (persist_dir / f"warmup-{i:02d}.json") if persist_dir else (tmp_root / f"warmup-{i:02d}.json")
            print(f"[warmup {i+1}/{args.warmup}] running...", file=sys.stderr, flush=True)
            warmup_runs.append(run_one(forward, out_path, args.agentfs_bin))

        runs: list[dict[str, Any]] = []
        for i in range(args.iterations):
            out_path = (persist_dir / f"iter-{i:02d}.json") if persist_dir else (tmp_root / f"iter-{i:02d}.json")
            print(f"[iter {i+1}/{args.iterations}] running...", file=sys.stderr, flush=True)
            payload = run_one(forward, out_path, args.agentfs_bin)
            runs.append(payload)
            result = payload.get("result") or {}
            summary = result.get("summary") or {}
            ratio = summary.get("ratio")
            ratio_text = f"{ratio:.2f}x" if isinstance(ratio, (int, float)) else "N/A"
            print(
                f"  rc={payload['returncode']} wall={payload['wall_seconds']:.2f}s ratio={ratio_text}",
                file=sys.stderr,
                flush=True,
            )

        leaked_git_ai = common.git_ai_leaks(git_ai_before, common.git_ai_processes())

        runs_for_aggregation = [r.get("result") or {} for r in runs]
        aggregation = aggregate(runs_for_aggregation)
        aggregation["label"] = args.label
        aggregation["forwarded_argv"] = forward
        aggregation["warmup_iterations"] = args.warmup
        aggregation["agentfs_bin"] = args.agentfs_bin
        aggregation["iteration_returncodes"] = [r["returncode"] for r in runs]
        aggregation["iteration_wall_seconds"] = [r["wall_seconds"] for r in runs]
        aggregation["git_ai_census"] = {
            "pre_existing": len(git_ai_before),
            "leaked": leaked_git_ai,
        }

        human = render_human(args.label, aggregation)
        print(human, file=sys.stderr, flush=True)

        payload = json.dumps(aggregation, indent=2, sort_keys=True) + "\n"
        if output_path is not None:
            output_path.write_text(payload)
        else:
            sys.stdout.write(payload)

        if leaked_git_ai:
            print(
                f"ERROR: benchmark run leaked {len(leaked_git_ai)} git-ai process(es); "
                "the pinned-git shim must cover every git invocation",
                file=sys.stderr,
            )
            return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
