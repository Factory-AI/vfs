#!/usr/bin/env python3
"""Emit a Phase 5 backend-risk decision input record."""

from __future__ import annotations

import argparse
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
            "decision inputs without changing dependencies."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  scripts/validation/backend-risk-spike.py
  scripts/validation/backend-risk-spike.py --candidate-turso-version 0.5.x --output backend-risk.json
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
        "--json-indent",
        type=int,
        default=2,
        help="JSON indentation level (default: 2)",
    )
    return parser.parse_args(argv)


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

    record = {
        "schema_version": 1,
        "spike": "phase5-backend-risk",
        "git_commit": git_commit(repo_root),
        "dependency_state": {
            "cargo_manifests": cargo_deps(repo_root),
            "dependency_changed_by_helper": False,
        },
        "turso_upgrade": {
            "candidate_version": args.candidate_turso_version,
            "decision_inputs": {
                "api_breakage": None,
                "behavior_changes": None,
                "single_file_checkpoint_snapshot_preserved": None,
                "sdk_tests": None,
                "cli_tests": None,
                "migration_tests": None,
                "replay_smoke": None,
                "corruption_torture": None,
                "phase45_ci": None,
                "blockers": [],
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
                "db_backend_trait_practicality": None,
                "estimated_invasiveness": None,
                "risk_reduction_vs_complexity": None,
                "blockers": [],
            },
        },
        "recommended_validation_commands": [
            "cargo test --manifest-path sdk/rust/Cargo.toml",
            "cargo test --manifest-path cli/Cargo.toml",
            "cli/tests/all.sh",
            "scripts/validation/phase0.sh",
            "scripts/validation/replay/replay-smoke.sh",
            "scripts/validation/posix/run-pjdfstest.sh --profile phase45-ci --agentfs-bin \"$PWD/cli/target/debug/agentfs\" --pjdfstest-dir /path/to/pjdfstest",
        ],
        "decision": {
            "status": "unmade",
            "selected_path": None,
            "rationale": None,
            "required_followups": [],
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
