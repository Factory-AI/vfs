#!/usr/bin/env python3
"""Validate that overlay base-file drift never serves stale mounted bytes."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


OUTPUT_TAIL_CHARS = 8000
STALE_BYTES = b"before-base-content\n"
FRESH_BYTES = b"after-base-content-with-new-size\n"


def tail_text(text: str) -> str:
    return text if len(text) <= OUTPUT_TAIL_CHARS else text[-OUTPUT_TAIL_CHARS:]


def resolve_agentfs_bin(agentfs_bin: str | None, repo_root: Path) -> str:
    if agentfs_bin:
        candidate = Path(agentfs_bin).expanduser()
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate.resolve())
        if os.sep not in agentfs_bin:
            found = shutil.which(agentfs_bin)
            if found:
                return found
        raise RuntimeError(f"agentfs binary not found or not executable: {agentfs_bin}")

    candidate = repo_root / "target" / "release" / "agentfs"
    if candidate.is_file() and os.access(candidate, os.X_OK):
        return str(candidate)
    raise RuntimeError("no release agentfs binary found, pass --agentfs-bin or build release")


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def parse_profile_summary(output: str) -> dict[str, Any]:
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
        if isinstance(counters, dict):
            return counters
    return {}


def run_checked(argv: list[str], cwd: Path, env: dict[str, str], timeout: float) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        cwd=str(cwd),
        env=env,
        text=True,
        capture_output=True,
        timeout=timeout,
    )


def wait_for_mount(mountpoint: Path, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        probe = subprocess.run(
            ["mountpoint", "-q", str(mountpoint)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if probe.returncode == 0:
            return True
        time.sleep(0.1)
    return False


def unmount(mountpoint: Path) -> None:
    for command in (["fusermount3", "-u", str(mountpoint)], ["fusermount", "-u", str(mountpoint)]):
        try:
            proc = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
        except (FileNotFoundError, subprocess.TimeoutExpired):
            continue
        if proc.returncode == 0:
            return


def run_leg(
    label: str,
    extra_env: dict[str, str],
    expect_noopen: bool,
    agentfs_bin: str,
    timeout: float,
) -> dict[str, Any]:
    started = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix=f"agentfs-base-drift-{label}-") as tmp:
        root = Path(tmp)
        work = root / "work"
        base = root / "base"
        mountpoint = root / "mnt"
        work.mkdir()
        base.mkdir()
        mountpoint.mkdir()
        base_file = base / "base.txt"
        base_file.write_bytes(STALE_BYTES)

        env = os.environ.copy()
        env.update(extra_env)
        env["AGENTFS_PROFILE"] = "1"

        init = run_checked([agentfs_bin, "init", "drift", "--base", str(base)], work, env, timeout)
        if init.returncode != 0:
            return {
                "label": label,
                "passed": False,
                "phase": "init",
                "returncode": init.returncode,
                "stderr_tail": tail_text(init.stderr),
            }

        mount_log = root / "mount.log"
        mount_out = mount_log.open("w", encoding="utf-8")
        proc = subprocess.Popen(
            [
                agentfs_bin,
                "mount",
                str(work / ".agentfs" / "drift.db"),
                str(mountpoint),
                "--foreground",
            ],
            cwd=str(work),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )

        mount_output = ""
        try:
            if not wait_for_mount(mountpoint, min(timeout, 20.0)):
                proc.terminate()
                try:
                    mount_output, _ = proc.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    mount_output, _ = proc.communicate(timeout=10)
                mount_out.write(mount_output)
                return {
                    "label": label,
                    "passed": False,
                    "phase": "mount",
                    "returncode": proc.returncode,
                    "mount_output_tail": tail_text(mount_output),
                }

            first = subprocess.run(
                ["cat", str(mountpoint / "base.txt")],
                text=False,
                capture_output=True,
                timeout=timeout,
            )
            if first.returncode != 0 or first.stdout != STALE_BYTES:
                return {
                    "label": label,
                    "passed": False,
                    "phase": "prime_read",
                    "returncode": first.returncode,
                    "stdout": first.stdout.decode("utf-8", "replace"),
                    "stderr_tail": tail_text(first.stderr.decode("utf-8", "replace")),
                }

            before_stat = base_file.stat()
            before_hash = sha256(base_file.read_bytes())

            # Keep mtime distinguishable on filesystems with coarse timestamps.
            time.sleep(1.1)
            base_file.write_bytes(FRESH_BYTES)
            after_stat = base_file.stat()
            after_hash = sha256(base_file.read_bytes())

            stat_after = subprocess.run(
                ["stat", "-c", "%s %Y", str(mountpoint / "base.txt")],
                text=True,
                capture_output=True,
                timeout=timeout,
            )
            if stat_after.returncode == 0:
                time.sleep(0.2)
                read_after = subprocess.run(
                    ["cat", str(mountpoint / "base.txt")],
                    text=False,
                    capture_output=True,
                    timeout=timeout,
                )
            else:
                read_after = subprocess.CompletedProcess(
                    args=["cat", str(mountpoint / "base.txt")],
                    returncode=stat_after.returncode,
                    stdout=b"",
                    stderr=stat_after.stderr.encode("utf-8", "replace"),
                )
        finally:
            unmount(mountpoint)
            try:
                mount_output, _ = proc.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                proc.terminate()
                try:
                    mount_output, _ = proc.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    mount_output, _ = proc.communicate(timeout=10)
            mount_out.write(mount_output)
            mount_out.close()

        counters = parse_profile_summary(mount_output)
        stale = read_after.returncode == 0 and read_after.stdout == STALE_BYTES
        fresh = read_after.returncode == 0 and read_after.stdout == FRESH_BYTES
        drift_error = read_after.returncode != 0 or stat_after.returncode != 0
        stat_size = stat_after.stdout.strip().split(" ")[0] if stat_after.returncode == 0 else None
        stat_fresh = stat_size == str(len(FRESH_BYTES))
        noopen_ok = True
        if expect_noopen:
            noopen_ok = counters.get("fuse_noopen_enosys_replies") == 1 and counters.get("fuse_op_open_count") == 1
        else:
            noopen_ok = (counters.get("fuse_op_open_count") or 0) >= 1 and (
                counters.get("fuse_noopen_enosys_replies") or 0
            ) == 0
        invalidations = counters.get("base_fast_inode_invalidations") or 0
        stale_rejections = counters.get("base_fast_stale_rejections") or 0

        return {
            "label": label,
            "passed": (fresh and stat_fresh and not stale and noopen_ok)
            or (drift_error and not stale and noopen_ok),
            "duration_seconds": time.perf_counter() - started,
            "before_hash": before_hash,
            "after_hash": after_hash,
            "before_size": before_stat.st_size,
            "after_size": after_stat.st_size,
            "before_mtime_ns": before_stat.st_mtime_ns,
            "after_mtime_ns": after_stat.st_mtime_ns,
            "mounted_read_returncode": read_after.returncode,
            "mounted_read": read_after.stdout.decode("utf-8", "replace"),
            "mounted_read_stderr_tail": tail_text(read_after.stderr.decode("utf-8", "replace")),
            "mounted_stat_returncode": stat_after.returncode,
            "mounted_stat": stat_after.stdout.strip(),
            "stale": stale,
            "fresh": fresh,
            "drift_error": drift_error,
            "stat_fresh": stat_fresh,
            "noopen_ok": noopen_ok,
            "fuse_op_open_count": counters.get("fuse_op_open_count"),
            "fuse_noopen_enosys_replies": counters.get("fuse_noopen_enosys_replies"),
            "base_fast_inode_invalidations": invalidations,
            "base_fast_stale_rejections": stale_rejections,
            "profile_counters_present": bool(counters),
            "mount_output_tail": tail_text(mount_output),
        }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agentfs-bin", default=os.environ.get("AGENTFS_BIN"))
    parser.add_argument("--timeout", type=float, default=60.0)
    parser.add_argument("--output", default=None)
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[2]
    agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
    legs = [
        ("default_noopen", {}, True),
        ("noopen_off", {"AGENTFS_FUSE_NOOPEN": "0"}, False),
    ]
    runs = [run_leg(label, env, noopen, agentfs_bin, args.timeout) for label, env, noopen in legs]
    report = {
        "schema_version": 1,
        "agentfs_bin": agentfs_bin,
        "passed": all(run.get("passed") for run in runs),
        "runs": runs,
    }
    output = args.output or os.path.join(
        tempfile.gettempdir(),
        f"agentfs-external-base-mutation-{time.strftime('%Y%m%d-%H%M%S')}.json",
    )
    Path(output).write_text(json.dumps(report, indent=2))

    for run in runs:
        status = "PASS" if run.get("passed") else "FAIL"
        print(
            f"{status} {run['label']:15s} stale={run.get('stale')} "
            f"fresh={run.get('fresh')} stat_fresh={run.get('stat_fresh')} "
            f"read_rc={run.get('mounted_read_returncode')} opens={run.get('fuse_op_open_count')} "
            f"enosys={run.get('fuse_noopen_enosys_replies')} "
            f"base_invalidations={run.get('base_fast_inode_invalidations')} "
            f"stale_rejections={run.get('base_fast_stale_rejections')}"
        )
        if not run.get("passed"):
            print(f"  mounted_read={run.get('mounted_read')!r}")
            print(f"  mounted_stat={run.get('mounted_stat')!r}")
    print(f"report: {output}")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
