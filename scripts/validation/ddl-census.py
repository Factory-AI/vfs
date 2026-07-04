#!/usr/bin/env python3
"""Test-aware census for VAL-CORE-014 schema DDL centralization."""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path


DDL_RE = re.compile(
    r"CREATE\s+(?:TEMP(?:ORARY)?\s+)?TABLE"
    r"|ALTER\s+TABLE"
    r"|PRAGMA\s+user_version"
    r"|[\"']schema_version[\"']",
    re.IGNORECASE,
)

SCHEMA_AUTHORITY_FILES = {
    Path("sdk/rust/src/schema.rs"),
}
SCHEMA_AUTHORITY_DIRS = (
    Path("sdk/rust/src/schema"),
    Path("crates/agentfs-core/src/schema"),
)

EXCLUDED_DIR_NAMES = {".git", "target"}
EXCLUDED_PREFIXES = (
    Path(".agents/benchmarks/fixtures"),
    Path(".agents/kernel"),
)


@dataclass(frozen=True)
class Match:
    path: Path
    line_no: int
    text: str

    def render(self) -> str:
        return f"{self.path}:{self.line_no}:{self.text.strip()}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=Path.cwd(),
        help="Repository root to scan (default: current working directory)",
    )
    return parser.parse_args()


def rel_to(root: Path, path: Path) -> Path:
    return path.relative_to(root).as_posix()


def is_under(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def is_excluded(root: Path, path: Path) -> bool:
    rel = path.relative_to(root)
    if any(part in EXCLUDED_DIR_NAMES for part in rel.parts):
        return True
    if rel.parts and rel.parts[0] == ".agents" and len(rel.parts) > 1:
        if re.fullmatch(r"\d{2}_\d{2}_\d{4}", rel.parts[1]):
            return True
    return any(is_under(rel, prefix) for prefix in EXCLUDED_PREFIXES)


def rust_files(root: Path) -> list[Path]:
    return sorted(
        path
        for path in root.rglob("*.rs")
        if path.is_file() and not is_excluded(root, path)
    )


def has_test_cfg(attrs: list[str]) -> bool:
    return any("#[cfg" in attr and re.search(r"\btest\b", attr) for attr in attrs)


def path_attr(attrs: list[str]) -> str | None:
    for attr in attrs:
        match = re.search(r'#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]', attr)
        if match:
            return match.group(1)
    return None


def line_offsets(text: str) -> list[int]:
    offsets = []
    offset = 0
    for line in text.splitlines(keepends=True):
        offsets.append(offset)
        offset += len(line)
    return offsets


def line_for_offset(offsets: list[int], offset: int) -> int:
    lo = 0
    hi = len(offsets)
    while lo + 1 < hi:
        mid = (lo + hi) // 2
        if offsets[mid] <= offset:
            lo = mid
        else:
            hi = mid
    return lo


def matching_brace(text: str, open_index: int) -> int | None:
    depth = 0
    in_string: str | None = None
    escaped = False
    in_line_comment = False
    in_block_comment = False
    i = open_index
    while i < len(text):
        ch = text[i]
        nxt = text[i + 1] if i + 1 < len(text) else ""

        if in_line_comment:
            if ch == "\n":
                in_line_comment = False
            i += 1
            continue
        if in_block_comment:
            if ch == "*" and nxt == "/":
                in_block_comment = False
                i += 2
            else:
                i += 1
            continue
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == in_string:
                in_string = None
            i += 1
            continue

        if ch == "/" and nxt == "/":
            in_line_comment = True
            i += 2
            continue
        if ch == "/" and nxt == "*":
            in_block_comment = True
            i += 2
            continue
        if ch in ("'", '"'):
            in_string = ch
            i += 1
            continue
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return None


def item_span_from_attrs(text: str, offsets: list[int], item_line: int) -> tuple[int, int] | None:
    start = offsets[item_line]
    next_line = offsets[item_line + 1] if item_line + 1 < len(offsets) else len(text)
    semicolon = text.find(";", start, next_line)
    open_brace = text.find("{", start)
    if open_brace == -1:
        return None
    if semicolon != -1 and semicolon < open_brace:
        return None
    close = matching_brace(text, open_brace)
    if close is None:
        return None
    return (item_line + 1, line_for_offset(offsets, close) + 1)


def test_ranges_and_path_includes(root: Path, path: Path, text: str) -> tuple[set[int], set[Path]]:
    offsets = line_offsets(text)
    lines = text.splitlines()
    test_lines: set[int] = set()
    path_includes: set[Path] = set()
    attrs: list[str] = []

    for idx, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("#["):
            attrs.append(stripped)
            continue

        if attrs and (not stripped or stripped.startswith("//")):
            continue

        if not attrs:
            continue

        if has_test_cfg(attrs):
            include = path_attr(attrs)
            if include and re.search(r"\bmod\b", stripped) and stripped.endswith(";"):
                path_includes.add((path.parent / include).resolve())
            span = item_span_from_attrs(text, offsets, idx)
            if span:
                start, end = span
                test_lines.update(range(start, end + 1))
        attrs = []

    return test_lines, path_includes


def schema_authority(root: Path, path: Path) -> bool:
    rel = path.relative_to(root)
    if rel in SCHEMA_AUTHORITY_FILES:
        return True
    return any(is_under(rel, authority_dir) for authority_dir in SCHEMA_AUTHORITY_DIRS)


def test_file(root: Path, path: Path, path_included_tests: set[Path]) -> bool:
    rel = path.relative_to(root)
    if path.resolve() in path_included_tests:
        return True
    if path.name == "tests.rs":
        return True
    return "tests" in rel.parts


def find_matches(root: Path, path: Path, text: str, skip_lines: set[int]) -> list[Match]:
    rel = Path(rel_to(root, path))
    matches: list[Match] = []
    for idx, line in enumerate(text.splitlines(), start=1):
        if idx in skip_lines:
            continue
        if DDL_RE.search(line):
            matches.append(Match(rel, idx, line))
    return matches


def main() -> int:
    args = parse_args()
    root = args.root.resolve()
    files = rust_files(root)

    texts: dict[Path, str] = {}
    test_ranges: dict[Path, set[int]] = {}
    path_included_tests: set[Path] = set()
    for path in files:
        text = path.read_text(encoding="utf-8")
        texts[path] = text
        ranges, includes = test_ranges_and_path_includes(root, path, text)
        test_ranges[path] = ranges
        path_included_tests.update(includes)

    inside: list[Match] = []
    outside: list[Match] = []
    for path, text in texts.items():
        if schema_authority(root, path):
            inside.extend(find_matches(root, path, text, set()))
            continue
        if test_file(root, path, path_included_tests):
            continue
        outside.extend(find_matches(root, path, text, test_ranges[path]))

    print(f"inside_schema_matches={len(inside)}")
    print(f"outside_schema_matches={len(outside)}")
    print(f"path_included_test_files={len(path_included_tests)}")

    if inside:
        print("schema_authority_positive_sample:")
        for match in inside[:10]:
            print(f"  {match.render()}")

    if outside:
        print("outside_schema_match_details:")
        for match in outside:
            print(f"  {match.render()}")
        return 1

    if not inside:
        print("error: no schema authority DDL matches found", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
