#!/usr/bin/env python3
"""Focused native git clone versus Vfs bulk-ingest benchmark."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib import common  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
CANONICAL_FIXTURE = REPO_ROOT / ".agents" / "benchmarks" / "fixtures" / "codex"
CONTENT_HASH_CMD = "git ls-files -z | sort -z | xargs -0 sha256sum | sha256sum"


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--source", help="local Git repository or worktree")
    source.add_argument("--remote", help="remote repository cloned into a local mirror")
    source.add_argument(
        "--synthetic",
        action="store_true",
        help="use a generated toy fixture; payload is marked non-comparable",
    )
    parser.add_argument("--samples", type=common.positive_int, default=10)
    parser.add_argument(
        "--warmup",
        type=common.non_negative_int,
        default=1,
        help="leading native/Vfs pairs to run and discard",
    )
    parser.add_argument("--synthetic-files", type=common.positive_int, default=128)
    parser.add_argument(
        "--synthetic-large-file-mib", type=common.positive_int, default=8
    )
    parser.add_argument(
        "--vfs-bin",
        default=os.environ.get("VFS_BIN"),
        help="vfs executable path/name (default: repo target binary)",
    )
    parser.add_argument("--timeout", type=common.positive_float, default=300.0)
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=common.env_flag("VFS_CLONE_KEEP_TEMP"),
    )
    parser.add_argument("--output", help="write the JSON payload to this path")
    parser.add_argument("--json-indent", type=common.non_negative_int, default=2)
    return parser.parse_args(argv)


def git_env() -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("GIT_CONFIG_NOSYSTEM", "1")
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    env.setdefault("NO_COLOR", "1")
    env.setdefault("LC_ALL", "C")
    return env


def run_git(
    argv: list[str], cwd: Path, env: dict[str, str], timeout: float
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [env.get("GIT", "git"), *argv],
        cwd=str(cwd),
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
    )


def require_git_ok(proc: subprocess.CompletedProcess[str], action: str) -> None:
    if proc.returncode != 0:
        raise RuntimeError(
            f"{action} failed with exit {proc.returncode}\n"
            f"stdout:\n{common.tail_text(proc.stdout)}\n"
            f"stderr:\n{common.tail_text(proc.stderr)}"
        )


def create_synthetic_repo(
    root: Path, files: int, large_file_mib: int, env: dict[str, str]
) -> None:
    root.mkdir(parents=True)
    require_git_ok(
        run_git(["init", "-b", "main"], root, env, 60), "git init synthetic fixture"
    )
    for index in range(files):
        directory = (
            root / ("src" if index % 2 == 0 else "tests") / f"pkg-{index % 16:02d}"
        )
        directory.mkdir(parents=True, exist_ok=True)
        seed = hashlib.sha256(f"clone-synthetic-{index}".encode()).hexdigest()
        content = "".join(f"{line:04d} {seed} CLONE_TOKEN\n" for line in range(32))
        (directory / f"file-{index:04d}.txt").write_text(content, encoding="utf-8")
    large = root / "data" / "large-origin.bin"
    large.parent.mkdir(parents=True, exist_ok=True)
    block = hashlib.sha256(b"clone-large-origin").digest() * 32768
    with large.open("wb") as handle:
        for _ in range(large_file_mib):
            handle.write(block)
    require_git_ok(run_git(["add", "."], root, env, 60), "git add synthetic fixture")
    commit_env = env.copy()
    commit_env["GIT_AUTHOR_NAME"] = "Vfs Clone Benchmark"
    commit_env["GIT_AUTHOR_EMAIL"] = "vfs-clone@example.invalid"
    commit_env["GIT_COMMITTER_NAME"] = "Vfs Clone Benchmark"
    commit_env["GIT_COMMITTER_EMAIL"] = "vfs-clone@example.invalid"
    commit_env["GIT_AUTHOR_DATE"] = "2024-01-01T00:00:00Z"
    commit_env["GIT_COMMITTER_DATE"] = "2024-01-01T00:00:00Z"
    require_git_ok(
        run_git(["commit", "-m", "deterministic clone fixture"], root, commit_env, 60),
        "git commit synthetic fixture",
    )


def prepare_mirror(
    args: argparse.Namespace, temp_root: Path, env: dict[str, str]
) -> tuple[Path, dict[str, Any]]:
    prepared = temp_root / "prepared"
    prepared.mkdir(parents=True)
    mirror = prepared / "mirror.git"
    if args.remote:
        source = args.remote
        kind = "remote"
    elif args.source:
        source_path = Path(args.source).expanduser().resolve()
        if not source_path.exists():
            raise RuntimeError(f"--source does not exist: {source_path}")
        source = str(source_path)
        kind = "source"
    elif not args.synthetic and CANONICAL_FIXTURE.is_dir():
        source = str(CANONICAL_FIXTURE)
        kind = "canonical-fixture"
        print(f"Using canonical fixture {CANONICAL_FIXTURE}", file=sys.stderr)
    elif not args.synthetic:
        raise RuntimeError(
            f"canonical fixture not found at {CANONICAL_FIXTURE}; refusing to guess. "
            "Use --source, --remote, or explicitly accept --synthetic."
        )
    else:
        generated = prepared / "synthetic-source"
        create_synthetic_repo(
            generated, args.synthetic_files, args.synthetic_large_file_mib, env
        )
        source = str(generated)
        kind = "synthetic"

    require_git_ok(
        run_git(
            ["clone", "--mirror", source, str(mirror)], prepared, env, args.timeout
        ),
        "prepare fixture mirror",
    )
    head = run_git(
        ["--git-dir", str(mirror), "rev-parse", "HEAD"], prepared, env, args.timeout
    )
    require_git_ok(head, "resolve fixture HEAD")
    return mirror, {
        "kind": kind,
        "path": source,
        "mirror_head": head.stdout.strip(),
        "comparable_to_scoreboard": kind == "canonical-fixture",
    }


def content_hash(workdir: Path, env: dict[str, str], timeout: float) -> str:
    proc = subprocess.run(
        ["sh", "-c", CONTENT_HASH_CMD],
        cwd=str(workdir),
        env=env,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"content hash failed: {common.tail_text(proc.stderr)}")
    return proc.stdout.split()[0]


def verify_native(
    workdir: Path, expected_hash: str, env: dict[str, str], timeout: float
) -> dict[str, Any]:
    status = run_git(["status", "--porcelain"], workdir, env, timeout)
    fsck = run_git(["fsck", "--strict"], workdir, env, timeout)
    observed_hash = content_hash(workdir, env, timeout)
    return {
        "status_clean": status.returncode == 0 and status.stdout == "",
        "fsck_ok": fsck.returncode == 0,
        "content_hash": observed_hash,
        "content_matches": observed_hash == expected_hash,
        "passed": status.returncode == 0
        and status.stdout == ""
        and fsck.returncode == 0
        and observed_hash == expected_hash,
    }


def verify_vfs(
    vfs_bin: str,
    db: Path,
    expected_hash: str,
    cwd: Path,
    env: dict[str, str],
    timeout: float,
) -> dict[str, Any]:
    script = (
        "cd repo || exit 9; "
        'test -z "$(git status --porcelain)" || exit 10; '
        "git fsck --strict >/dev/null 2>&1 || exit 11; " + CONTENT_HASH_CMD
    )
    run = common.run_subprocess(
        [vfs_bin, "exec", str(db), "sh", "--", "-c", script],
        cwd,
        env,
        timeout,
        keep_stdout=True,
    )
    lines = [line for line in str(run.get("stdout", "")).splitlines() if line.strip()]
    observed_hash = lines[-1].split()[0] if lines else None
    return {
        "run": {key: value for key, value in run.items() if key != "stdout"},
        "content_hash": observed_hash,
        "content_matches": observed_hash == expected_hash,
        "passed": run["returncode"] == 0 and observed_hash == expected_hash,
    }


def render_summary(result: dict[str, Any]) -> str:
    stats = result.get("absolute_command_seconds", {})
    native = stats.get("native", {})
    vfs = stats.get("vfs", {})
    return (
        f"clone benchmark: {'PASS' if result.get('passed') else 'FAIL'}\n"
        f"  native median {native.get('median', float('nan')):.3f}s "
        f"(p25 {native.get('p25', float('nan')):.3f}, p75 {native.get('p75', float('nan')):.3f}, n={native.get('n', 0)})\n"
        f"  vfs    median {vfs.get('median', float('nan')):.3f}s "
        f"(p25 {vfs.get('p25', float('nan')):.3f}, p75 {vfs.get('p75', float('nan')):.3f}, n={vfs.get('n', 0)})"
    )


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.keep_temp:
        temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
        temp_root = Path(tempfile.mkdtemp(prefix="vfs-clone-benchmark-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(
            prefix="vfs-clone-benchmark-", ignore_cleanup_errors=True
        )
        temp_root = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    git_ai_before = common.git_ai_processes()
    try:
        base_env = git_env()
        base_env = common.isolate_benchmark_env(base_env, temp_root / "setup")
        vfs_bin = common.resolve_vfs_bin(args.vfs_bin, REPO_ROOT)
        mirror, source = prepare_mirror(args, temp_root, base_env)

        baseline = temp_root / "prepared" / "baseline"
        require_git_ok(
            run_git(
                ["clone", "--no-hardlinks", str(mirror), str(baseline)],
                temp_root,
                base_env,
                args.timeout,
            ),
            "prepare correctness baseline",
        )
        expected_hash = content_hash(baseline, base_env, args.timeout)

        runs: list[dict[str, Any]] = []
        native_seconds: list[float] = []
        vfs_seconds: list[float] = []
        schedule: list[str] = []
        total_sample_count = args.warmup + args.samples
        for sample_index in range(total_sample_count):
            warmup = sample_index < args.warmup
            for leg in ("native", "vfs"):
                if not warmup:
                    schedule.append(leg)
                context = temp_root / "runs" / f"{sample_index:03d}-{leg}"
                context.mkdir(parents=True)
                env = common.isolate_benchmark_env(base_env, context)
                if leg == "native":
                    destination = context / "repo"
                    command = [
                        env["GIT"],
                        "clone",
                        "--quiet",
                        "--no-hardlinks",
                        str(mirror),
                        str(destination),
                    ]
                else:
                    db = context / "clone.db"
                    command = [
                        vfs_bin,
                        "clone",
                        str(db),
                        str(mirror),
                        "repo",
                    ]
                    cleanup_marker = str(db)
                run = common.run_subprocess(
                    command, context, env, args.timeout, keep_stdout=True
                )
                verification = (
                    verify_native(destination, expected_hash, env, args.timeout)
                    if leg == "native" and run["returncode"] == 0
                    else verify_vfs(
                        vfs_bin, db, expected_hash, context, env, args.timeout
                    )
                    if leg == "vfs" and run["returncode"] == 0
                    else {"passed": False}
                )
                cleanup = (
                    common.wait_for_benchmark_cleanup(cleanup_marker, context)
                    if leg == "vfs"
                    else {
                        "ok": True,
                        "clean_exit": True,
                        "processes": [],
                        "mounts": [],
                        "waited_seconds": 0.0,
                    }
                )
                passed = (
                    run["returncode"] == 0
                    and verification["passed"] is True
                    and cleanup["ok"] is True
                )
                if not warmup:
                    (native_seconds if leg == "native" else vfs_seconds).append(
                        float(run["duration_seconds"])
                    )
                runs.append(
                    {
                        "sample": sample_index,
                        "warmup": warmup,
                        "leg": leg,
                        "command_seconds": run["duration_seconds"],
                        "run": {
                            key: value for key, value in run.items() if key != "stdout"
                        },
                        "verification": verification,
                        "cleanup": cleanup,
                        "passed": passed,
                    }
                )
                print(
                    f"[{sample_index + 1}/{total_sample_count}] "
                    f"{'warmup ' if warmup else ''}{leg}: "
                    f"{run['duration_seconds']:.3f}s, {'PASS' if passed else 'FAIL'}",
                    file=sys.stderr,
                    flush=True,
                )

        native_stats = common.summarize_floats(native_seconds)
        vfs_stats = common.summarize_floats(vfs_seconds)
        leaked_git_ai = common.git_ai_leaks(git_ai_before, common.git_ai_processes())
        passed = (
            all(run["passed"] for run in runs)
            and len(native_seconds) == args.samples
            and len(vfs_seconds) == args.samples
            and not leaked_git_ai
        )
        result = {
            "schema_version": 2,
            "benchmark": "vfs-clone",
            "git_commit": common.git_commit(REPO_ROOT),
            "source": source,
            "parameters": {
                "samples": args.samples,
                "warmup_samples_discarded": args.warmup,
                "timeout_seconds": args.timeout,
            },
            "engine": {
                "bin": vfs_bin,
                "version": common.run_subprocess(
                    [vfs_bin, "version", "--json"],
                    temp_root,
                    base_env,
                    args.timeout,
                    keep_stdout=True,
                )
                .get("stdout", "")
                .strip(),
            },
            "measurement": {
                "schedule": schedule,
                "interleaved": schedule
                == [leg for _ in range(args.samples) for leg in ("native", "vfs")],
                "native_control": "git clone --no-hardlinks from the prepared local mirror",
                "vfs_operation": "vfs clone from the same prepared local mirror",
                "primary_metric": "absolute user-visible command wall-clock seconds",
                "verification_included_in_metric": False,
                "performance_threshold": None,
            },
            "absolute_command_seconds": {"native": native_stats, "vfs": vfs_stats},
            "derived": {
                "vfs_over_native_median": (
                    float(vfs_stats["median"]) / float(native_stats["median"])
                    if native_stats.get("n")
                    and vfs_stats.get("n")
                    and float(native_stats["median"]) > 0
                    else None
                ),
                "warning": "derived ratio; interpret only beside both absolute distributions",
            },
            "correctness": {
                "expected_content_sha256": expected_hash,
                "git_status_clean": True,
                "git_fsck_strict": True,
                "content_hash_matches": True,
            },
            "runs": runs,
            "git_ai_census": {
                "pre_existing": len(git_ai_before),
                "leaked": leaked_git_ai,
            },
            "passed": passed,
            "temp_dir": str(temp_root),
            "kept_temp": args.keep_temp,
        }
        if not passed:
            exit_code = 1
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 2,
            "benchmark": "vfs-clone",
            "error": str(exc),
            "passed": False,
            "temp_dir": str(temp_root),
            "kept_temp": args.keep_temp,
        }

    print(render_summary(result), file=sys.stderr)
    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    if args.output:
        output = Path(args.output).expanduser()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(payload, encoding="utf-8")
        print(f"Wrote clone benchmark JSON to {output}", file=sys.stderr)
    else:
        sys.stdout.write(payload)
    if temp_manager is not None:
        temp_manager.cleanup()
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
