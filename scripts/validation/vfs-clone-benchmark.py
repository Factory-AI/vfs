#!/usr/bin/env python3
"""Benchmark `agentfs clone` (bulk ingest) against native `git clone`.

For each iteration the timed unit is the whole user-visible command:
  native : git clone <mirror> <dst>
  agentfs: agentfs clone <fresh.db> <mirror> repo

Correctness per agentfs iteration (through a fresh `agentfs exec` mount):
  - `git status --porcelain` is empty (fabricated index matches served stats)
  - `git fsck --strict` passes
  - sha256 over the sorted worktree equals the native clone's

Usage:
  agentfs-clone-benchmark.py --source <repo> [--iterations 5]
"""

from __future__ import annotations

import argparse
import json
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]

CONTENT_HASH_CMD = "git ls-files -z | sort -z | xargs -0 sha256sum | sha256sum"


def run(cmd: list[str], cwd: Path | None = None, timeout: int = 300) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def require(proc: subprocess.CompletedProcess, what: str) -> None:
    if proc.returncode != 0:
        raise RuntimeError(f"{what} failed (rc={proc.returncode}): {proc.stderr.strip()[:500]}")


def resolve_agentfs_bin(arg: str | None) -> str:
    if arg:
        return arg
    for candidate in (
        REPO_ROOT / "cli" / "target" / "release" / "agentfs",
        REPO_ROOT / "cli" / "target" / "debug" / "agentfs",
    ):
        if candidate.is_file():
            return str(candidate)
    raise RuntimeError("agentfs binary not found; build cli or pass --agentfs-bin")


def content_hash_native(workdir: Path) -> str:
    proc = subprocess.run(
        ["sh", "-c", CONTENT_HASH_CMD], cwd=workdir, capture_output=True, text=True
    )
    require(proc, "native content hash")
    return proc.stdout.split()[0]


def verify_agentfs(agentfs_bin: str, db: Path, native_hash: str) -> None:
    script = (
        "cd repo || exit 9; "
        "test -z \"$(git status --porcelain)\" || { echo STATUS_DIRTY; exit 10; }; "
        "git fsck --strict >/dev/null 2>&1 || { echo FSCK_FAIL; exit 11; }; "
        + CONTENT_HASH_CMD
    )
    proc = run([agentfs_bin, "exec", str(db), "sh", "--", "-c", script])
    require(proc, "agentfs verification")
    # The mount emits tracing lines on stdout; the hash is the last line.
    lines = [line for line in proc.stdout.strip().splitlines() if line.strip()]
    got = lines[-1].split()[0] if lines else ""
    if got != native_hash:
        raise RuntimeError(f"content hash mismatch: agentfs {got} != native {native_hash}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, help="git repository used to build the mirror")
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--agentfs-bin")
    parser.add_argument("--output")
    args = parser.parse_args()

    agentfs_bin = resolve_agentfs_bin(args.agentfs_bin)
    temp_root = Path(tempfile.mkdtemp(prefix="agentfs-clone-bench-"))
    results: dict = {"iterations": [], "source": args.source}
    try:
        mirror = temp_root / "mirror.git"
        require(
            run(["git", "clone", "--bare", "--quiet", args.source, str(mirror)]),
            "mirror preparation",
        )

        baseline = temp_root / "baseline"
        require(run(["git", "clone", "--quiet", str(mirror), str(baseline)]), "baseline clone")
        native_hash = content_hash_native(baseline)

        for i in range(args.iterations):
            native_dst = temp_root / f"native-{i}"
            started = time.perf_counter()
            require(
                run(["git", "clone", "--quiet", str(mirror), str(native_dst)]),
                "native clone",
            )
            native_s = time.perf_counter() - started
            shutil.rmtree(native_dst)

            db = temp_root / f"agentfs-{i}.db"
            started = time.perf_counter()
            require(
                run([agentfs_bin, "clone", str(db), str(mirror), "repo"]),
                "agentfs clone",
            )
            agentfs_s = time.perf_counter() - started

            verify_agentfs(agentfs_bin, db, native_hash)
            db.unlink(missing_ok=True)

            results["iterations"].append(
                {"native_seconds": native_s, "agentfs_seconds": agentfs_s,
                 "ratio": agentfs_s / native_s}
            )
            print(
                f"iter {i}: native={native_s:.3f}s agentfs={agentfs_s:.3f}s "
                f"ratio={agentfs_s / native_s:.2f}x (verified)",
                flush=True,
            )

        natives = [r["native_seconds"] for r in results["iterations"]]
        ours = [r["agentfs_seconds"] for r in results["iterations"]]
        results["summary"] = {
            "native_median": statistics.median(natives),
            "agentfs_median": statistics.median(ours),
            "ratio_median": statistics.median(ours) / statistics.median(natives),
            "ratio_paired_median": statistics.median(
                r["ratio"] for r in results["iterations"]
            ),
            "all_verified": True,
        }
        s = results["summary"]
        print(
            f"\nmedian: native={s['native_median']:.3f}s agentfs={s['agentfs_median']:.3f}s "
            f"ratio={s['ratio_median']:.2f}x paired={s['ratio_paired_median']:.2f}x"
        )
        if args.output:
            Path(args.output).write_text(json.dumps(results, indent=1))
        return 0
    finally:
        shutil.rmtree(temp_root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
