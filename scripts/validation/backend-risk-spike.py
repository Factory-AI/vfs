#!/usr/bin/env python3
"""Emit a Phase 5 backend-risk decision input/result record."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Optional


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Record machine-readable Turso upgrade and rusqlite fallback "
            "decision inputs/results without changing dependencies."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  scripts/validation/backend-risk-spike.py
  scripts/validation/backend-risk-spike.py --candidate-turso-version 0.5.x --output backend-risk.json
  scripts/validation/backend-risk-spike.py --candidate-turso-version 0.5.x \\
    --resolved-turso-version 0.5.3 \\
    --upgrade-built true \\
    --validation-result sdk_tests=passed \\
    --validation-command 'sdk_tests=cargo test --manifest-path sdk/rust/Cargo.toml' \\
    --decision-status upgraded \\
    --selected-path turso-upgrade-now \\
    --rationale 'Candidate built and validation passed.'
""",
    )
    parser.add_argument(
        "--candidate-turso-version",
        default="0.5.x",
        help="Turso version/range to evaluate (default: 0.5.x)",
    )
    parser.add_argument(
        "--fallback-crate",
        default="rusqlite",
        help="SQLite fallback crate to evaluate (default: rusqlite)",
    )
    parser.add_argument(
        "--output",
        help="write JSON record to this file instead of stdout",
    )
    parser.add_argument(
        "--resolved-turso-version",
        help="exact Turso version resolved by Cargo, when known",
    )
    parser.add_argument(
        "--upgrade-built",
        choices=["true", "false", "blocked"],
        help="whether the candidate Turso upgrade built in this spike",
    )
    parser.add_argument(
        "--turso-api-breakage",
        action="append",
        default=[],
        help="actual API breakage observed while attempting the candidate upgrade; repeatable",
    )
    parser.add_argument(
        "--turso-behavior-change",
        action="append",
        default=[],
        help="actual behavior change observed while attempting the candidate upgrade; repeatable",
    )
    parser.add_argument(
        "--turso-blocker",
        action="append",
        default=[],
        help="compiler/API/runtime blocker observed for the Turso candidate; repeatable",
    )
    parser.add_argument(
        "--validation-result",
        action="append",
        default=[],
        metavar="KEY=STATUS",
        help=(
            "record a measured validation status, e.g. sdk_tests=passed, "
            "cli_tests=blocked, phase45_ci=not_run; repeatable"
        ),
    )
    parser.add_argument(
        "--validation-command",
        action="append",
        default=[],
        metavar="KEY=COMMAND",
        help="record the command used for a validation key; repeatable",
    )
    parser.add_argument(
        "--validation-exit-code",
        action="append",
        default=[],
        metavar="KEY=CODE",
        help="record the process exit code for a validation key; repeatable",
    )
    parser.add_argument(
        "--validation-duration",
        action="append",
        default=[],
        metavar="KEY=SECONDS",
        help="record elapsed wall time in seconds for a validation key; repeatable",
    )
    parser.add_argument(
        "--validation-summary",
        action="append",
        default=[],
        metavar="KEY=TEXT",
        help="record concise measured output/findings for a validation key; repeatable",
    )
    parser.add_argument(
        "--fallback-trait-practicality",
        help="assessment of whether a DB-backend trait is practical",
    )
    parser.add_argument(
        "--fallback-invasiveness",
        help="estimated invasiveness of a rusqlite fallback",
    )
    parser.add_argument(
        "--fallback-risk-reduction",
        help="assessment of risk reduction versus complexity for a fallback",
    )
    parser.add_argument(
        "--fallback-blocker",
        action="append",
        default=[],
        help="fallback feasibility blocker or caveat; repeatable",
    )
    parser.add_argument(
        "--decision-status",
        choices=["unmade", "upgraded", "blocked", "fallback-required", "deferred"],
        default="unmade",
        help="overall backend decision status (default: unmade)",
    )
    parser.add_argument(
        "--selected-path",
        help="chosen backend path, e.g. turso-upgrade-now, defer-upgrade, rusqlite-fallback-spike",
    )
    parser.add_argument(
        "--rationale",
        help="decision rationale based on measured results",
    )
    parser.add_argument(
        "--required-followup",
        action="append",
        default=[],
        help="required follow-up action; repeatable",
    )
    parser.add_argument(
        "--json-indent",
        type=int,
        default=2,
        help="JSON indentation level (default: 2)",
    )
    return parser.parse_args(argv)


def parse_key_value_entries(entries: list[str], option_name: str) -> dict[str, str]:
    parsed: dict[str, str] = {}
    for entry in entries:
        if "=" not in entry:
            raise SystemExit(f"{option_name} must use KEY=VALUE syntax: {entry!r}")
        key, value = entry.split("=", 1)
        key = key.strip()
        if not key:
            raise SystemExit(f"{option_name} key must not be empty: {entry!r}")
        parsed[key] = value.strip()
    return parsed


def parse_exit_codes(entries: list[str]) -> dict[str, int]:
    parsed = parse_key_value_entries(entries, "--validation-exit-code")
    result: dict[str, int] = {}
    for key, value in parsed.items():
        try:
            result[key] = int(value)
        except ValueError as exc:
            raise SystemExit(
                f"--validation-exit-code value for {key!r} must be an integer: {value!r}"
            ) from exc
    return result


def parse_durations(entries: list[str]) -> dict[str, float]:
    parsed = parse_key_value_entries(entries, "--validation-duration")
    result: dict[str, float] = {}
    for key, value in parsed.items():
        try:
            result[key] = float(value)
        except ValueError as exc:
            raise SystemExit(
                f"--validation-duration value for {key!r} must be numeric seconds: {value!r}"
            ) from exc
    return result


def validation_results(args: argparse.Namespace) -> dict[str, dict[str, Any]]:
    statuses = parse_key_value_entries(args.validation_result, "--validation-result")
    commands = parse_key_value_entries(args.validation_command, "--validation-command")
    summaries = parse_key_value_entries(args.validation_summary, "--validation-summary")
    exit_codes = parse_exit_codes(args.validation_exit_code)
    durations = parse_durations(args.validation_duration)

    keys = set(statuses) | set(commands) | set(summaries) | set(exit_codes) | set(durations)
    results = {}
    for key in sorted(keys):
        item: dict[str, Any] = {
            "status": statuses.get(key, "unknown"),
        }
        if key in commands:
            item["command"] = commands[key]
        if key in exit_codes:
            item["exit_code"] = exit_codes[key]
        if key in durations:
            item["duration_seconds"] = durations[key]
        if key in summaries:
            item["summary"] = summaries[key]
        results[key] = item
    return results


def upgrade_built_value(value: Optional[str]) -> Optional[bool | str]:
    if value is None:
        return None
    if value == "true":
        return True
    if value == "false":
        return False
    return value


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


def cargo_dependency_versions(cargo_toml: Path, dependency: str) -> list[str]:
    if not cargo_toml.exists():
        return []
    text = cargo_toml.read_text(encoding="utf-8")
    pattern = re.compile(
        r"^\s*"
        + re.escape(dependency)
        + r'\s*=\s*(?:"([^"]+)"|\{[^}\n]*version\s*=\s*"([^"]+)"[^}\n]*\})',
        re.MULTILINE,
    )
    versions = []
    for match in pattern.finditer(text):
        versions.append(next(group for group in match.groups() if group is not None))
    return versions


def cargo_deps(repo_root: Path) -> dict[str, Any]:
    manifests = [
        repo_root / "cli" / "Cargo.toml",
        repo_root / "sdk" / "rust" / "Cargo.toml",
        repo_root / "sandbox" / "Cargo.toml",
    ]
    return {
        str(path.relative_to(repo_root)): {
            "turso": cargo_dependency_versions(path, "turso"),
            "rusqlite": cargo_dependency_versions(path, "rusqlite"),
        }
        for path in manifests
    }


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    measured_validation_results = validation_results(args)
    validation_statuses = {
        key: value["status"] for key, value in measured_validation_results.items()
    }
    api_breakage = args.turso_api_breakage or None
    behavior_changes = args.turso_behavior_change or None
    upgrade_built = upgrade_built_value(args.upgrade_built)

    record = {
        "schema_version": 2,
        "spike": "phase5-backend-risk",
        "git_commit": git_commit(repo_root),
        "recorded_at": datetime.now(timezone.utc).isoformat(),
        "dependency_state": {
            "cargo_manifests": cargo_deps(repo_root),
            "dependency_changed_by_helper": False,
        },
        "turso_upgrade": {
            "candidate_version": args.candidate_turso_version,
            "resolved_version": args.resolved_turso_version,
            "built": upgrade_built,
            "decision_inputs": {
                "api_breakage": api_breakage,
                "behavior_changes": behavior_changes,
                "single_file_checkpoint_snapshot_preserved": None,
                "sdk_tests": validation_statuses.get("sdk_tests"),
                "cli_tests": validation_statuses.get("cli_tests"),
                "migration_tests": validation_statuses.get("migration_tests"),
                "replay_smoke": validation_statuses.get("replay_smoke"),
                "corruption_torture": validation_statuses.get("corruption_torture"),
                "phase45_ci": validation_statuses.get("phase45_ci"),
                "blockers": args.turso_blocker,
            },
            "candidate_run": {
                "validation_results": measured_validation_results,
                "api_breakage": args.turso_api_breakage,
                "behavior_changes": args.turso_behavior_change,
                "blockers": args.turso_blocker,
            },
        },
        "fallback": {
            "crate": args.fallback_crate,
            "decision_inputs": {
                "minimum_storage_api_surface": [
                    "open local file-backed database",
                    "execute statements and query rows asynchronously or behind an async boundary",
                    "transactions for filesystem metadata/data updates",
                    "BLOB reads/writes for fs_data and inline inode payloads",
                    "PRAGMA WAL, synchronous=NORMAL, checkpoint, and busy-timeout behavior",
                    "single-file snapshot/checkpoint semantics",
                    "optional local encryption/cloud sync compatibility decision",
                ],
                "db_backend_trait_practicality": args.fallback_trait_practicality,
                "estimated_invasiveness": args.fallback_invasiveness,
                "risk_reduction_vs_complexity": args.fallback_risk_reduction,
                "blockers": args.fallback_blocker,
            },
        },
        "recommended_validation_commands": [
            "cargo test --manifest-path sdk/rust/Cargo.toml",
            "cargo test --manifest-path cli/Cargo.toml",
            "cli/tests/all.sh",
            "scripts/validation/phase0.sh",
            "scripts/validation/replay/replay_workload.py --agentfs-bin cli/target/debug/agentfs /path/to/replay.jsonl",
            "scripts/validation/posix/run-pjdfstest.sh --profile phase45-ci --agentfs-bin \"$PWD/cli/target/debug/agentfs\" --pjdfstest-dir /path/to/pjdfstest",
        ],
        "decision": {
            "status": args.decision_status,
            "selected_path": args.selected_path,
            "rationale": args.rationale,
            "required_followups": args.required_followup,
        },
    }

    payload = json.dumps(record, indent=args.json_indent, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(payload, encoding="utf-8")
        print(f"Wrote backend-risk spike JSON to {args.output}", file=sys.stderr)
    else:
        sys.stdout.write(payload)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
