#!/usr/bin/env python3
"""Summarize pjdfstest prove logs and map failures to known POSIX gaps."""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from tempfile import TemporaryDirectory


TEST_START_RE = re.compile(r"^(?P<path>.*?/tests/(?P<rel>\S+?\.t))\s+\.+")
FAILED_RE = re.compile(r"^Failed\s+(?P<failed>\d+)/(?P<total>\d+)\s+subtests")
SUMMARY_FAIL_RE = re.compile(r"^(?P<path>.*?/tests/(?P<rel>\S+?\.t))\s+\(.*Failed:\s+(?P<failed>\d+)\)")
PLAN_RE = re.compile(r"^1\.\.(?P<total>\d+)")


@dataclass
class TestResult:
    relpath: str
    suite: str
    status: str = "unknown"
    failed: int = 0
    total: int = 0


@dataclass
class KnownGap:
    target: str
    category: str
    reason: str


def infer_category(reason: str) -> str:
    lowered = reason.lower()
    if "mix" in lowered or "split" in lowered:
        return "mixed-test-file"
    if "root" in lowered or "platform" in lowered or "environment" in lowered or "ci" in lowered:
        return "environment-sensitive"
    if (
        "unsupported" in lowered
        or "contract" in lowered
        or "privileged" in lowered
        or "mknod" in lowered
        or "chown" in lowered
        or "uid/gid" in lowered
        or "alternate uid" in lowered
    ):
        return "unsupported-contract"
    return "core-correctness-bug"


def parse_known_gaps(path: Path) -> list[KnownGap]:
    gaps: list[KnownGap] = []
    if not path.exists():
        return gaps

    for line_number, raw_line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue

        columns = raw_line.split("\t")
        if len(columns) == 2:
            target, reason = columns
            category = infer_category(reason)
        elif len(columns) >= 3:
            target, category, reason = columns[0], columns[1], "\t".join(columns[2:])
        else:
            raise ValueError(f"{path}:{line_number}: expected target<TAB>reason")

        gaps.append(KnownGap(target=target.strip(), category=category.strip(), reason=reason.strip()))

    return gaps


def parse_log(path: Path) -> dict[str, TestResult]:
    results: dict[str, TestResult] = {}
    current: TestResult | None = None

    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if match := TEST_START_RE.match(line):
            relpath = match.group("rel")
            current = results.setdefault(
                relpath,
                TestResult(relpath=relpath, suite=relpath.split("/", 1)[0]),
            )
            continue

        if match := SUMMARY_FAIL_RE.match(line):
            relpath = match.group("rel")
            result = results.setdefault(
                relpath,
                TestResult(relpath=relpath, suite=relpath.split("/", 1)[0]),
            )
            result.status = "failed"
            result.failed = max(result.failed, int(match.group("failed")))
            continue

        if current is None:
            continue

        if match := PLAN_RE.match(line):
            current.total = max(current.total, int(match.group("total")))
        elif match := FAILED_RE.match(line):
            current.status = "failed"
            current.failed = int(match.group("failed"))
            current.total = max(current.total, int(match.group("total")))
        elif line == "ok":
            if current.status != "failed":
                current.status = "passed"
            current = None

    return results


def known_gap_for(relpath: str, gaps: list[KnownGap]) -> KnownGap | None:
    exact_matches = [gap for gap in gaps if not gap.target.endswith("/") and gap.target == relpath]
    if exact_matches:
        return exact_matches[0]

    prefix_matches = [gap for gap in gaps if gap.target.endswith("/") and relpath.startswith(gap.target)]
    if prefix_matches:
        return max(prefix_matches, key=lambda gap: len(gap.target))

    return None


def format_summary(results: dict[str, TestResult], gaps: list[KnownGap]) -> str:
    ordered = sorted(results.values(), key=lambda result: result.relpath)
    passed = [result for result in ordered if result.status == "passed"]
    failed = [result for result in ordered if result.status == "failed"]
    unknown = [result for result in ordered if result.status == "unknown"]

    lines = [
        "pjdfstest summary",
        f"files: {len(ordered)} passed: {len(passed)} failed: {len(failed)} unknown: {len(unknown)}",
        "",
        "by suite:",
        "suite\tpassed\tfailed\tunknown",
    ]

    suites = sorted({result.suite for result in ordered})
    for suite in suites:
        suite_results = [result for result in ordered if result.suite == suite]
        lines.append(
            "\t".join(
                [
                    suite,
                    str(sum(result.status == "passed" for result in suite_results)),
                    str(sum(result.status == "failed" for result in suite_results)),
                    str(sum(result.status == "unknown" for result in suite_results)),
                ]
            )
        )

    category_counts: dict[str, int] = {}
    uncategorized: list[TestResult] = []
    categorized: list[tuple[TestResult, KnownGap]] = []
    for result in failed:
        gap = known_gap_for(result.relpath, gaps)
        if gap is None:
            uncategorized.append(result)
            continue
        categorized.append((result, gap))
        category_counts[gap.category] = category_counts.get(gap.category, 0) + 1

    lines.extend(
        [
            "",
            "known-gap coverage for failed files:",
            f"categorized: {len(categorized)} uncategorized: {len(uncategorized)}",
        ]
    )
    for category in sorted(category_counts):
        lines.append(f"{category}: {category_counts[category]}")

    if failed:
        lines.extend(["", "failed files:"])
        for result in failed:
            gap = known_gap_for(result.relpath, gaps)
            if gap is None:
                lines.append(f"{result.relpath}\tuncategorized\t")
            else:
                lines.append(f"{result.relpath}\t{gap.category}\t{gap.reason}")

    return "\n".join(lines)


def self_test() -> None:
    sample_log = """\
/tmp/pjdfstest/tests/chmod/02.t .....
1..2
ok 1
ok 2
ok
/tmp/pjdfstest/tests/rename/11.t ....
1..3
ok 1
not ok 2 - tried 'rename a b', expected 0, got EIO
ok 3
Failed 1/3 subtests

Test Summary Report
-------------------
/tmp/pjdfstest/tests/rename/11.t (Wstat: 0 Tests: 3 Failed: 1)
  Failed test:  2
Files=2, Tests=5
Result: FAIL
"""
    sample_gaps = "# target\treason\nrename/11.t\tCore rename gap in full run; needs Phase 5 triage.\n"

    with TemporaryDirectory() as temp_dir:
        log_path = Path(temp_dir) / "pjdfstest.log"
        gaps_path = Path(temp_dir) / "known-gaps.tsv"
        log_path.write_text(sample_log, encoding="utf-8")
        gaps_path.write_text(sample_gaps, encoding="utf-8")
        summary = format_summary(parse_log(log_path), parse_known_gaps(gaps_path))

    assert "files: 2 passed: 1 failed: 1 unknown: 0" in summary, summary
    assert "rename\t0\t1\t0" in summary, summary
    assert "categorized: 1 uncategorized: 0" in summary, summary
    assert "core-correctness-bug: 1" in summary, summary


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", nargs="?", type=Path, help="pjdfstest TAP/prove log to summarize")
    parser.add_argument(
        "--known-gaps",
        type=Path,
        default=Path(__file__).resolve().parent / "pjdfstest" / "known-gaps.tsv",
        help="known-gaps TSV file; supports target<TAB>reason or target<TAB>category<TAB>reason",
    )
    parser.add_argument("--self-test", action="store_true", help="run built-in parser self-test and exit")
    args = parser.parse_args(argv)

    if args.self_test:
        self_test()
        print("self-test ok")
        return 0

    if args.log is None:
        parser.error("log is required unless --self-test is used")

    results = parse_log(args.log)
    gaps = parse_known_gaps(args.known_gaps)
    print(format_summary(results, gaps))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
