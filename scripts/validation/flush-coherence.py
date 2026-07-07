#!/usr/bin/env python3
"""Attr coherence around close() with and without the FLUSH round trip.

With AGENTFS_FUSE_NOFLUSH=1 the adapter answers the first FLUSH with ENOSYS,
so the kernel stops sending FLUSH and a closed handle's buffered write tail
reaches the SDK only at the async RELEASE (or via the adapter's pending-tail
drains on attr-bearing paths). This script hammers exactly that window:

  1. coherence loop: write (varied sizes) -> close -> immediately stat the
     path, `scandir` + stat the directory (READDIRPLUS), hardlink + stat the
     link (LINK reply attrs), and re-read the content. Every observed size
     must match what was written, on every iteration, no matter who wins the
     race against RELEASE.
  2. open-tail check: push dirty pages to the adapter with sync_file_range
     while the writer fd stays open, then stat/read through other handles.

Each scenario runs under {flush, noflush} x {default TTLs, entry TTL 0}; the
entry-TTL-0 configs force a LOOKUP per stat so the adapter's pending-tail
guard is actually on the hot path. Gates:

  - zero size/content mismatches in every config;
  - noflush configs reply ENOSYS at least once (the latch engaged);
  - the pending-tail drain counter fired in at least one noflush config
    (proof the close->RELEASE window was really exercised).
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib.common import resolve_agentfs_bin, tail_text  # noqa: E402

WORKLOAD = r'''
import ctypes
import json
import os
import sys

root = os.getcwd()
mismatches = []
iterations = int(sys.argv[1])

libc = ctypes.CDLL("libc.so.6", use_errno=True)
SYNC_FILE_RANGE_WRITE = 2


def check(label, observed, expected):
    if observed != expected:
        mismatches.append(
            {"label": label, "observed": observed, "expected": expected}
        )


def write_close(path, size):
    payload = bytes((i * 31 + size) % 251 for i in range(size))
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.write(fd, payload)
    os.close(fd)
    return payload


# 1. coherence loop: race stat/readdirplus/link/read against async RELEASE.
sizes = [1, 137, 4096, 65536, 200_000]
linkdir = os.path.join(root, "links")
os.mkdir(linkdir)
for i in range(iterations):
    size = sizes[i % len(sizes)]
    name = os.path.join(root, f"race_{i}.bin")
    payload = write_close(name, size)

    check(f"stat[{i}]", os.stat(name).st_size, size)
    listed = {
        entry.name: entry.stat().st_size
        for entry in os.scandir(root)
        if entry.is_file()
    }
    check(f"scandir[{i}]", listed.get(f"race_{i}.bin"), size)
    link = os.path.join(linkdir, f"link_{i}.bin")
    os.link(name, link)
    check(f"linkstat[{i}]", os.stat(link).st_size, size)
    with open(name, "rb") as handle:
        check(f"read[{i}]", handle.read() == payload, True)
    if i % len(sizes) == 0:
        os.unlink(name)
        os.unlink(link)

# 2. open tail: dirty pages pushed to the adapter while the fd stays open.
tail = os.path.join(root, "tail.bin")
fd = os.open(tail, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, b"x" * 10_000)
rc = libc.sync_file_range(fd, 0, 0, SYNC_FILE_RANGE_WRITE)
check("sync_file_range", rc, 0)
check("open_tail_stat", os.stat(tail).st_size, 10_000)
with open(tail, "rb") as handle:
    check("open_tail_read", len(handle.read()), 10_000)
os.write(fd, b"y" * 5_000)
libc.sync_file_range(fd, 0, 0, SYNC_FILE_RANGE_WRITE)
check("open_tail_appended_stat", os.stat(tail).st_size, 15_000)
os.close(fd)
check("closed_tail_stat", os.stat(tail).st_size, 15_000)

print(json.dumps({"mismatches": mismatches, "iterations": iterations}))
'''


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
        if '"agentfs_profile_summary"' not in line:
            continue
        start = line.find("{")
        if start < 0:
            continue
        try:
            value = json.loads(line[start:])
        except json.JSONDecodeError:
            continue
        counters = value.get("counters")
        if isinstance(counters, dict) and any(key.startswith("fuse_") for key in counters):
            return counters
    return None


def run_config(
    agentfs_bin: str,
    temp_root: Path,
    label: str,
    iterations: int,
    timeout: float,
    noflush: bool,
    entry_ttl_ms: Optional[int],
) -> dict[str, Any]:
    db = temp_root / f"{label}.db"
    db.touch()
    env = os.environ.copy()
    env["AGENTFS_PROFILE"] = "1"
    env["AGENTFS_FUSE_NOFLUSH"] = "1" if noflush else "0"
    if entry_ttl_ms is None:
        env.pop("AGENTFS_FUSE_ENTRY_TTL_MS", None)
    else:
        env["AGENTFS_FUSE_ENTRY_TTL_MS"] = str(entry_ttl_ms)

    argv = [
        agentfs_bin,
        "exec",
        str(db),
        sys.executable,
        "--",
        "-c",
        WORKLOAD,
        str(iterations),
    ]
    started = time.perf_counter()
    proc = subprocess.run(
        argv,
        cwd=str(temp_root),
        env=env,
        text=True,
        capture_output=True,
        timeout=timeout,
    )
    combined = proc.stdout + "\n" + proc.stderr
    workload = parse_workload_json(proc.stdout)
    counters = parse_fuse_counters(combined)

    mismatches = workload.get("mismatches") if isinstance(workload, dict) else None
    result: dict[str, Any] = {
        "label": label,
        "noflush": noflush,
        "entry_ttl_ms": entry_ttl_ms,
        "returncode": proc.returncode,
        "duration_seconds": time.perf_counter() - started,
        "workload_json_present": workload is not None,
        "counters_present": counters is not None,
        "mismatch_count": len(mismatches) if isinstance(mismatches, list) else None,
        "mismatches": (mismatches or [])[:20],
        "stderr_tail": tail_text(proc.stderr) if proc.returncode != 0 else "",
    }
    if counters:
        result["fuse_op_flush_count"] = counters.get("fuse_op_flush_count")
        result["fuse_noflush_enosys_replies"] = counters.get(
            "fuse_noflush_enosys_replies"
        )
        result["fuse_pending_tail_drains"] = counters.get("fuse_pending_tail_drains")
        result["fuse_release_count"] = counters.get("fuse_release_count")

    passed = (
        proc.returncode == 0
        and workload is not None
        and counters is not None
        and mismatches == []
    )
    if noflush:
        passed = passed and (result.get("fuse_noflush_enosys_replies") or 0) >= 1
    result["passed"] = passed
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agentfs-bin", default=os.environ.get("AGENTFS_BIN"))
    parser.add_argument("--iterations", type=int, default=120)
    parser.add_argument("--timeout", type=float, default=600.0)
    parser.add_argument("--output", default=None)
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[2]
    agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)

    configs = [
        ("flush_default_ttl", False, None),
        ("flush_entry_ttl0", False, 0),
        ("noflush_default_ttl", True, None),
        ("noflush_entry_ttl0", True, 0),
    ]

    runs = []
    with tempfile.TemporaryDirectory(prefix="agentfs-flush-coherence-") as tmp:
        temp_root = Path(tmp)
        for label, noflush, ttl in configs:
            runs.append(
                run_config(
                    agentfs_bin,
                    temp_root,
                    label,
                    args.iterations,
                    args.timeout,
                    noflush,
                    ttl,
                )
            )

    tail_drains = sum(
        run.get("fuse_pending_tail_drains") or 0 for run in runs if run["noflush"]
    )
    window_exercised = tail_drains >= 1
    all_passed = all(run["passed"] for run in runs) and window_exercised

    report = {
        "schema_version": 1,
        "agentfs_bin": agentfs_bin,
        "iterations": args.iterations,
        "noflush_pending_tail_drains_total": tail_drains,
        "window_exercised": window_exercised,
        "passed": all_passed,
        "runs": runs,
    }
    output = args.output or os.path.join(
        tempfile.gettempdir(),
        f"agentfs-flush-coherence-{time.strftime('%Y%m%d-%H%M%S')}.json",
    )
    Path(output).write_text(json.dumps(report, indent=2))

    for run in runs:
        status = "PASS" if run["passed"] else "FAIL"
        print(
            f"{status} {run['label']:22s} mismatches={run['mismatch_count']} "
            f"enosys={run.get('fuse_noflush_enosys_replies')} "
            f"tail_drains={run.get('fuse_pending_tail_drains')} "
            f"flush_ops={run.get('fuse_op_flush_count')}"
        )
    print(f"window_exercised={window_exercised} (tail_drains={tail_drains})")
    print(f"report: {output}")
    return 0 if all_passed else 1


if __name__ == "__main__":
    sys.exit(main())
