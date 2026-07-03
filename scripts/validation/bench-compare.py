#!/usr/bin/env python3
"""Compare two median-of-5 git workload benchmark JSON files."""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


RELATIVE_RED_THRESHOLD = 0.05
ABSOLUTE_RED_THRESHOLD_SECONDS = 0.010
EXPECTED_ITERATIONS = 5


class BenchmarkFormatError(ValueError):
    """Raised when a benchmark JSON file does not match the perf contract."""


@dataclass(frozen=True)
class PhaseMedian:
    phase: str
    agentfs_seconds: float
    count: int


@dataclass(frozen=True)
class PhaseComparison:
    phase: str
    baseline_seconds: float
    candidate_seconds: float
    delta_seconds: float
    relative_delta: float

    @property
    def is_red(self) -> bool:
        return (
            self.relative_delta > RELATIVE_RED_THRESHOLD
            and self.delta_seconds > ABSOLUTE_RED_THRESHOLD_SECONDS
        )


def load_json(path: Path) -> dict[str, Any]:
    try:
        payload = json.loads(path.read_text())
    except FileNotFoundError as exc:
        raise BenchmarkFormatError(f"{path}: file not found") from exc
    except json.JSONDecodeError as exc:
        raise BenchmarkFormatError(f"{path}: invalid JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise BenchmarkFormatError(f"{path}: top-level JSON must be an object")
    return payload


def require_number(value: Any, path: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise BenchmarkFormatError(f"{path}: expected number, got {type(value).__name__}")
    return float(value)


def require_int(value: Any, path: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise BenchmarkFormatError(f"{path}: expected integer, got {type(value).__name__}")
    return value


def parse_phase_medians(payload: dict[str, Any], source: Path) -> dict[str, PhaseMedian]:
    iterations = require_int(payload.get("iterations"), f"{source}: iterations")
    if iterations != EXPECTED_ITERATIONS:
        raise BenchmarkFormatError(
            f"{source}: expected iterations={EXPECTED_ITERATIONS}, got {iterations}"
        )

    returncodes = payload.get("iteration_returncodes")
    if not isinstance(returncodes, list):
        raise BenchmarkFormatError(f"{source}: iteration_returncodes must be a list")
    if len(returncodes) != EXPECTED_ITERATIONS:
        raise BenchmarkFormatError(
            f"{source}: expected {EXPECTED_ITERATIONS} iteration return codes, got {len(returncodes)}"
        )
    bad_returncodes = [code for code in returncodes if code != 0]
    if bad_returncodes:
        raise BenchmarkFormatError(
            f"{source}: benchmark has non-zero iteration return codes: {bad_returncodes}"
        )

    phases = payload.get("phases")
    if not isinstance(phases, dict) or not phases:
        raise BenchmarkFormatError(f"{source}: phases must be a non-empty object")

    parsed: dict[str, PhaseMedian] = {}
    for phase, phase_payload in phases.items():
        if not isinstance(phase, str):
            raise BenchmarkFormatError(f"{source}: phase name must be a string")
        if not isinstance(phase_payload, dict):
            raise BenchmarkFormatError(f"{source}: phases.{phase} must be an object")
        agentfs = phase_payload.get("agentfs_seconds")
        if not isinstance(agentfs, dict):
            raise BenchmarkFormatError(
                f"{source}: phases.{phase}.agentfs_seconds must be an object"
            )
        count = require_int(agentfs.get("count"), f"{source}: phases.{phase}.agentfs_seconds.count")
        if count != EXPECTED_ITERATIONS:
            raise BenchmarkFormatError(
                f"{source}: phases.{phase}.agentfs_seconds.count expected "
                f"{EXPECTED_ITERATIONS}, got {count}"
            )
        median = require_number(
            agentfs.get("median"),
            f"{source}: phases.{phase}.agentfs_seconds.median",
        )
        parsed[phase] = PhaseMedian(phase=phase, agentfs_seconds=median, count=count)

    return parsed


def compare_phase_medians(
    baseline: dict[str, PhaseMedian],
    candidate: dict[str, PhaseMedian],
) -> list[PhaseComparison]:
    missing_from_candidate = sorted(set(baseline) - set(candidate))
    missing_from_baseline = sorted(set(candidate) - set(baseline))
    if missing_from_candidate or missing_from_baseline:
        details: list[str] = []
        if missing_from_candidate:
            details.append(f"missing from candidate: {', '.join(missing_from_candidate)}")
        if missing_from_baseline:
            details.append(f"missing from baseline: {', '.join(missing_from_baseline)}")
        raise BenchmarkFormatError("phase sets differ: " + "; ".join(details))

    comparisons: list[PhaseComparison] = []
    for phase in sorted(baseline):
        base = baseline[phase].agentfs_seconds
        cand = candidate[phase].agentfs_seconds
        if base <= 0:
            raise BenchmarkFormatError(
                f"baseline phase {phase} has non-positive median {base}"
            )
        delta = cand - base
        comparisons.append(
            PhaseComparison(
                phase=phase,
                baseline_seconds=base,
                candidate_seconds=cand,
                delta_seconds=delta,
                relative_delta=delta / base,
            )
        )
    return comparisons


def format_ms(seconds: float) -> str:
    return f"{seconds * 1000.0:.1f}"


def format_percent(value: float) -> str:
    return f"{value * 100.0:+.1f}%"


def render_human(comparisons: list[PhaseComparison]) -> str:
    lines = [
        "Perf contract: red iff candidate median is >5% slower AND >10ms slower.",
        "",
        "| Phase | Baseline ms | Candidate ms | Delta ms | Relative | Result |",
        "| --- | ---: | ---: | ---: | ---: | --- |",
    ]
    for item in comparisons:
        result = "RED" if item.is_red else "OK"
        lines.append(
            f"| {item.phase} | {format_ms(item.baseline_seconds)} | "
            f"{format_ms(item.candidate_seconds)} | {format_ms(item.delta_seconds)} | "
            f"{format_percent(item.relative_delta)} | {result} |"
        )
    red_count = sum(1 for item in comparisons if item.is_red)
    lines.extend(
        [
            "",
            f"Summary: {red_count} red phase(s), {len(comparisons) - red_count} within band.",
        ]
    )
    return "\n".join(lines)


def render_json(comparisons: list[PhaseComparison]) -> str:
    payload = {
        "rule": {
            "relative_slower_than": RELATIVE_RED_THRESHOLD,
            "absolute_slower_than_seconds": ABSOLUTE_RED_THRESHOLD_SECONDS,
            "iterations": EXPECTED_ITERATIONS,
        },
        "red_phases": [item.phase for item in comparisons if item.is_red],
        "phases": [
            {
                "phase": item.phase,
                "baseline_seconds": item.baseline_seconds,
                "candidate_seconds": item.candidate_seconds,
                "delta_seconds": item.delta_seconds,
                "relative_delta": item.relative_delta,
                "red": item.is_red,
            }
            for item in comparisons
        ],
    }
    return json.dumps(payload, indent=2, sort_keys=True)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Compare two git-workload-benchmark-multi.py aggregate JSON files "
            "using the VFS perf contract."
        )
    )
    parser.add_argument("baseline_json", type=Path, help="M1 baseline aggregate JSON")
    parser.add_argument("candidate_json", type=Path, help="candidate aggregate JSON")
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit machine-readable JSON instead of a Markdown table",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    try:
        baseline = parse_phase_medians(load_json(args.baseline_json), args.baseline_json)
        candidate = parse_phase_medians(load_json(args.candidate_json), args.candidate_json)
        comparisons = compare_phase_medians(baseline, candidate)
    except BenchmarkFormatError as exc:
        print(f"bench-compare: {exc}", file=sys.stderr)
        return 2

    if args.json:
        print(render_json(comparisons))
    else:
        print(render_human(comparisons))

    return 1 if any(item.is_red for item in comparisons) else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
