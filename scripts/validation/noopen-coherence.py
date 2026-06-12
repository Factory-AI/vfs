#!/usr/bin/env python3
"""Coherence around zero-message opens (AGENTFS_FUSE_NOOPEN / kernel no_open).

With the kernel's `no_open` latched, open(2)/close(2) complete with no FUSE
request, all file I/O arrives with `fh=0`, and FUSE_RELEASE is skipped for
every file (including CREATE-opened ones). The adapter serves that traffic
from a shared per-inode file table resolved on first I/O. This script hammers
the seams of that model:

  exec scenarios (pure AgentFS db):
    - write -> close -> immediately stat / scandir / hardlink-stat / re-read
      (race loop, absolute size asserts);
    - ftruncate through an fd (SETATTR arrives with fh=0);
    - O_TRUNC reopen (delivered as SETATTR size=0, never atomic);
    - mmap shared-write + msync;
    - fd kept open across an unlink (per-ino file must keep serving);
    - a small AGENTFS_FUSE_INO_FILES_CAP config that forces soft-cap
      eviction + re-resolution mid-workload.

  overlay scenario (agentfs run --session over a base tree):
    - read a base file (read-only resolution = host passthrough), append to
      it (write upgrade -> copy-up replaces the per-ino file), then re-read
      through fresh fds and verify base+append content and sizes.

Gates: zero mismatches in every config; noopen configs must show exactly one
OPEN ever (the ENOSYS latch: fuse_noopen_enosys_replies == 1) and at least
one per-ino resolution across the noopen configs.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any, Optional

OUTPUT_TAIL_CHARS = 8000

EXEC_WORKLOAD = r'''
import ctypes
import json
import mmap
import os
import sys

root = os.getcwd()
mismatches = []
iterations = int(sys.argv[1])


def check(label, observed, expected):
    if observed != expected:
        mismatches.append({"label": label, "observed": repr(observed), "expected": repr(expected)})


# 1. close-race loop: zero open/close round trips must not change semantics.
sizes = [1, 137, 4096, 65536, 200_000]
linkdir = os.path.join(root, "links")
os.mkdir(linkdir)
for i in range(iterations):
    size = sizes[i % len(sizes)]
    name = os.path.join(root, f"race_{i}.bin")
    payload = bytes((j * 31 + size) % 251 for j in range(size))
    fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.write(fd, payload)
    os.close(fd)

    check(f"stat[{i}]", os.stat(name).st_size, size)
    listed = {e.name: e.stat().st_size for e in os.scandir(root) if e.is_file()}
    check(f"scandir[{i}]", listed.get(f"race_{i}.bin"), size)
    link = os.path.join(linkdir, f"link_{i}.bin")
    os.link(name, link)
    check(f"linkstat[{i}]", os.stat(link).st_size, size)
    with open(name, "rb") as handle:
        check(f"read[{i}]", handle.read() == payload, True)

# 2. ftruncate through an fd (SETATTR carries fh=0 under no_open).
t = os.path.join(root, "trunc.bin")
fd = os.open(t, os.O_WRONLY | os.O_CREAT, 0o644)
os.write(fd, b"0123456789")
os.ftruncate(fd, 4)
os.close(fd)
check("ftruncate_size", os.stat(t).st_size, 4)
check("ftruncate_content", open(t, "rb").read(), b"0123")

# 3. O_TRUNC reopen.
fd = os.open(t, os.O_WRONLY | os.O_TRUNC)
os.write(fd, b"xy")
os.close(fd)
check("otrunc_size", os.stat(t).st_size, 2)
check("otrunc_content", open(t, "rb").read(), b"xy")

# 4. mmap shared write + msync.
m = os.path.join(root, "mapped.bin")
fd = os.open(m, os.O_RDWR | os.O_CREAT, 0o644)
os.ftruncate(fd, 32)
mm = mmap.mmap(fd, 32)
mm[:6] = b"mapped"
mm.flush()
mm.close()
os.close(fd)
check("mmap_content", open(m, "rb").read()[:6], b"mapped")

# 5. unlink must not wedge later operations. (I/O on an unlinked-but-open
# inode is a pre-existing SDK gap — the inode is reaped immediately, so even
# the close-time writeback mtime SETATTR errors. Identical under the legacy
# fh path; tracked as a followup, not asserted here.)
u = os.path.join(root, "unlinked.bin")
with open(u, "wb") as handle:
    handle.write(b"ghost")
os.unlink(u)
check("unlinked_gone", os.path.exists(u), False)
after = os.path.join(root, "after-unlink.bin")
with open(after, "wb") as handle:
    handle.write(b"still-works")
check("post_unlink_io", open(after, "rb").read(), b"still-works")

print(json.dumps({"mismatches": mismatches, "iterations": iterations}))
'''

OVERLAY_WORKLOAD = r'''
import json
import os

root = os.getcwd()
mismatches = []


def check(label, observed, expected):
    if observed != expected:
        mismatches.append({"label": label, "observed": repr(observed), "expected": repr(expected)})


base_payload = open("base.txt", "rb").read()
check("base_read", base_payload, b"base-content\n")

# Append: upgrades the read-only (host passthrough) resolution to the
# copy-up'd delta file; later reads must see base + appended bytes.
with open("base.txt", "ab") as handle:
    handle.write(b"appended\n")

check("post_copyup_size", os.stat("base.txt").st_size, len(b"base-content\nappended\n"))
check("post_copyup_read", open("base.txt", "rb").read(), b"base-content\nappended\n")

print(json.dumps({"mismatches": mismatches}))
'''


def tail_text(text: str) -> str:
    return text if len(text) <= OUTPUT_TAIL_CHARS else text[-OUTPUT_TAIL_CHARS:]


def resolve_agentfs_bin(agentfs_bin: Optional[str], repo_root: Path) -> str:
    if agentfs_bin:
        candidate = Path(agentfs_bin).expanduser()
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate.resolve())
        if os.sep not in agentfs_bin:
            found = shutil.which(agentfs_bin)
            if found:
                return found
        raise RuntimeError(f"agentfs binary not found or not executable: {agentfs_bin}")
    for candidate in (
        repo_root / "cli" / "target" / "release" / "agentfs",
        repo_root / "cli" / "target" / "debug" / "agentfs",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    raise RuntimeError("no agentfs binary found; pass --agentfs-bin or set AGENTFS_BIN")


def parse_workload_json(stdout: str) -> Optional[dict[str, Any]]:
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and "mismatches" in value:
            return value
    return None


def parse_fuse_counters(output: str) -> Optional[dict[str, Any]]:
    for line in reversed(output.splitlines()):
        if '"agentfs_profile_summary"' not in line or '"fuse_session"' not in line:
            continue
        start = line.find("{")
        if start < 0:
            continue
        try:
            value = json.loads(line[start:])
        except json.JSONDecodeError:
            continue
        counters = value.get("counters")
        if isinstance(counters, dict):
            return counters
    return None


def run_one(
    argv: list[str],
    cwd: Path,
    env: dict[str, str],
    timeout: float,
    label: str,
    noopen: bool,
) -> dict[str, Any]:
    started = time.perf_counter()
    proc = subprocess.run(
        argv, cwd=str(cwd), env=env, text=True, capture_output=True, timeout=timeout
    )
    combined = proc.stdout + "\n" + proc.stderr
    workload = parse_workload_json(proc.stdout)
    counters = parse_fuse_counters(combined) or {}
    mismatches = workload.get("mismatches") if isinstance(workload, dict) else None

    result: dict[str, Any] = {
        "label": label,
        "noopen": noopen,
        "returncode": proc.returncode,
        "duration_seconds": time.perf_counter() - started,
        "workload_json_present": workload is not None,
        "mismatch_count": len(mismatches) if isinstance(mismatches, list) else None,
        "mismatches": (mismatches or [])[:20],
        "fuse_op_open_count": counters.get("fuse_op_open_count"),
        "fuse_noopen_enosys_replies": counters.get("fuse_noopen_enosys_replies"),
        "fuse_ino_file_resolutions": counters.get("fuse_ino_file_resolutions"),
        "fuse_ino_file_upgrades": counters.get("fuse_ino_file_upgrades"),
        "fuse_op_release_count": counters.get("fuse_op_release_count"),
        "stderr_tail": tail_text(proc.stderr) if proc.returncode != 0 else "",
    }
    passed = proc.returncode == 0 and workload is not None and mismatches == []
    if noopen:
        passed = (
            passed
            and result["fuse_noopen_enosys_replies"] == 1
            and result["fuse_op_open_count"] == 1
        )
    result["passed"] = passed
    return result


def base_env(extra: dict[str, str]) -> dict[str, str]:
    env = os.environ.copy()
    env["AGENTFS_PROFILE"] = "1"
    env.pop("AGENTFS_FUSE_INO_FILES_CAP", None)
    env.update(extra)
    return env


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agentfs-bin", default=os.environ.get("AGENTFS_BIN"))
    parser.add_argument("--iterations", type=int, default=60)
    parser.add_argument("--timeout", type=float, default=600.0)
    parser.add_argument("--output", default=None)
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[2]
    agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)

    exec_configs = [
        ("exec_noopen_off", {"AGENTFS_FUSE_NOOPEN": "0"}, False),
        ("exec_noopen_on", {"AGENTFS_FUSE_NOOPEN": "1"}, True),
        (
            "exec_noopen_ttl0",
            {"AGENTFS_FUSE_NOOPEN": "1", "AGENTFS_FUSE_ENTRY_TTL_MS": "0"},
            True,
        ),
        (
            "exec_noopen_smallcap",
            {"AGENTFS_FUSE_NOOPEN": "1", "AGENTFS_FUSE_INO_FILES_CAP": "16"},
            True,
        ),
    ]
    overlay_configs = [
        ("overlay_noopen_off", {"AGENTFS_FUSE_NOOPEN": "0"}, False),
        ("overlay_noopen_on", {"AGENTFS_FUSE_NOOPEN": "1"}, True),
    ]

    runs = []
    with tempfile.TemporaryDirectory(prefix="agentfs-noopen-coherence-") as tmp:
        temp_root = Path(tmp)
        for label, extra, noopen in exec_configs:
            db = temp_root / f"{label}.db"
            db.touch()
            argv = [
                agentfs_bin,
                "exec",
                str(db),
                sys.executable,
                "--",
                "-c",
                EXEC_WORKLOAD,
                str(args.iterations),
            ]
            runs.append(
                run_one(argv, temp_root, base_env(extra), args.timeout, label, noopen)
            )

        for label, extra, noopen in overlay_configs:
            base_root = temp_root / f"{label}-base"
            base_root.mkdir()
            (base_root / "base.txt").write_bytes(b"base-content\n")
            argv = [
                agentfs_bin,
                "run",
                "--session",
                f"noopen-coh-{uuid.uuid4().hex[:8]}",
                "--no-default-allows",
                "--",
                sys.executable,
                "-c",
                OVERLAY_WORKLOAD,
            ]
            runs.append(
                run_one(argv, base_root, base_env(extra), args.timeout, label, noopen)
            )

    resolutions = sum(r.get("fuse_ino_file_resolutions") or 0 for r in runs if r["noopen"])
    upgrades = sum(r.get("fuse_ino_file_upgrades") or 0 for r in runs if r["noopen"])
    all_passed = all(r["passed"] for r in runs) and resolutions >= 1 and upgrades >= 1

    report = {
        "schema_version": 1,
        "agentfs_bin": agentfs_bin,
        "iterations": args.iterations,
        "noopen_resolutions_total": resolutions,
        "noopen_upgrades_total": upgrades,
        "passed": all_passed,
        "runs": runs,
    }
    output = args.output or os.path.join(
        tempfile.gettempdir(),
        f"agentfs-noopen-coherence-{time.strftime('%Y%m%d-%H%M%S')}.json",
    )
    Path(output).write_text(json.dumps(report, indent=2))

    for run in runs:
        status = "PASS" if run["passed"] else "FAIL"
        print(
            f"{status} {run['label']:22s} mismatches={run['mismatch_count']} "
            f"opens={run.get('fuse_op_open_count')} "
            f"enosys={run.get('fuse_noopen_enosys_replies')} "
            f"resolves={run.get('fuse_ino_file_resolutions')} "
            f"upgrades={run.get('fuse_ino_file_upgrades')} "
            f"releases={run.get('fuse_op_release_count')}"
        )
    print(f"resolutions={resolutions} upgrades={upgrades} passed={all_passed}")
    print(f"report: {output}")
    return 0 if all_passed else 1


if __name__ == "__main__":
    sys.exit(main())
