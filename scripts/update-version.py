#!/usr/bin/env python3
"""
Update the Rust workspace version.

Rust crates inherit their version from the root [workspace.package] table, so
there is no per-crate Cargo.toml or per-crate Cargo.lock sync job anymore.
This script updates the single workspace version and refreshes the root lockfile.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?$")
WORKSPACE_VERSION_RE = re.compile(
    r'(^\[workspace\.package\]\s*(?:(?!^\[).)*?^version\s*=\s*)"[^"]*"',
    re.MULTILINE | re.DOTALL,
)


def parse_version(version: str) -> str:
    if not SEMVER_RE.match(version):
        raise ValueError(
            f"Invalid version format: {version}. Expected X.Y.Z or X.Y.Z-pre.N"
        )
    return version


def find_project_root() -> Path:
    current = Path(__file__).resolve().parent
    while current.parent != current:
        if (current / "Cargo.toml").is_file() and (current / "cli").is_dir():
            return current
        current = current.parent
    raise RuntimeError("Could not find project root")


def update_workspace_version(root: Path, version: str, dry_run: bool) -> bool:
    manifest = root / "Cargo.toml"
    content = manifest.read_text()
    new_content, count = WORKSPACE_VERSION_RE.subn(rf'\1"{version}"', content, count=1)
    if count != 1:
        raise RuntimeError("Could not find [workspace.package] version in root Cargo.toml")
    if new_content == content:
        print(f"Workspace version already {version}")
        return False
    if dry_run:
        print(f"Would update {manifest.relative_to(root)} to {version}")
    else:
        manifest.write_text(new_content)
        print(f"Updated {manifest.relative_to(root)} to {version}")
    return True


def refresh_lockfile(root: Path, dry_run: bool) -> None:
    if dry_run:
        print("Would run: cargo generate-lockfile")
        return
    result = subprocess.run(
        ["cargo", "generate-lockfile"],
        cwd=root,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError("cargo generate-lockfile failed")


def main() -> int:
    parser = argparse.ArgumentParser(description="Update the AgentFS workspace version")
    parser.add_argument("version", help="Version number, for example 0.6.5 or 0.7.0-pre.1")
    parser.add_argument("--dry-run", action="store_true", help="Print changes without writing files")
    args = parser.parse_args()

    try:
        version = parse_version(args.version)
        root = find_project_root()
        changed = update_workspace_version(root, version, args.dry_run)
        if changed:
            refresh_lockfile(root, args.dry_run)
    except Exception as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
