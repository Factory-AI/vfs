#!/usr/bin/env python3
"""Concurrent native-vs-Vfs filesystem workload benchmark."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import Any, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib import common  # noqa: E402

from lib.common import (  # noqa: E402
    env_flag,
    git_commit,
    parse_json_stdout,
    positive_float,
    positive_int,
    resolve_vfs_bin,
    run_subprocess,
    sandbox_python,
    summarize_floats,
    tail_text,
    tree_fingerprint,
)


CANONICAL_FIXTURE = (
    Path(__file__).resolve().parents[2]
    / ".agents"
    / "benchmarks"
    / "fixtures"
    / "codex"
)
OUTPUT_TAIL_CHARS = 30000
ACTOR_KINDS = ("git_churn", "editor", "reader", "builder", "churner", "fetcher")


CHAOS_WORKLOAD = r"""
import argparse
import concurrent.futures
import hashlib
import json
import os
import random
import statistics
import subprocess
import threading
import time
from pathlib import Path


ACTOR_KINDS = ("git_churn", "editor", "reader", "builder", "churner", "fetcher")
TAIL_CHARS = 2000
DEADLINE = None


def percentile(values, quantile):
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * quantile
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    fraction = position - lower
    return ordered[lower] * (1.0 - fraction) + ordered[upper] * fraction


def stats(values):
    if not values:
        return {"n": 0}
    return {
        "n": len(values),
        "median": statistics.median(values),
        "p25": percentile(values, 0.25),
        "p75": percentile(values, 0.75),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "min": min(values),
        "max": max(values),
        "stdev": statistics.stdev(values) if len(values) > 1 else 0.0,
    }


def tail(text):
    text = str(text or "")
    return text if len(text) <= TAIL_CHARS else text[-TAIL_CHARS:]


def git_env():
    env = os.environ.copy()
    env.setdefault("GIT_CONFIG_NOSYSTEM", "1")
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    env.setdefault("NO_COLOR", "1")
    env.setdefault("LC_ALL", "C")
    env["GIT_PAGER"] = "cat"
    env["GIT_AUTHOR_NAME"] = "Vfs Chaos Benchmark"
    env["GIT_AUTHOR_EMAIL"] = "vfs-chaos@example.invalid"
    env["GIT_COMMITTER_NAME"] = "Vfs Chaos Benchmark"
    env["GIT_COMMITTER_EMAIL"] = "vfs-chaos@example.invalid"
    return env


def run_git(argv, cwd, retries=4):
    git = os.environ.get("GIT", "git")
    started = time.perf_counter()
    attempts = []
    for attempt in range(retries + 1):
        proc = subprocess.run(
            [git] + argv,
            cwd=str(cwd),
            env=git_env(),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        attempts.append({
            "returncode": proc.returncode,
            "stdout_tail": tail(proc.stdout),
            "stderr_tail": tail(proc.stderr),
        })
        contention = (
            "index.lock" in (proc.stderr or "")
            or "cannot lock ref" in (proc.stderr or "")
            or "Unable to create" in (proc.stderr or "")
        )
        if proc.returncode == 0 or not contention or attempt == retries:
            return {
                "returncode": proc.returncode,
                "stdout": proc.stdout or "",
                "stderr": proc.stderr or "",
                "attempts": attempts,
                "duration_seconds": time.perf_counter() - started,
            }
        time.sleep(0.005 * (attempt + 1))


def require_git(argv, cwd, action):
    record = run_git(argv, cwd)
    if record["returncode"] != 0:
        raise RuntimeError(
            f"{action} failed with exit {record['returncode']}: {tail(record['stderr'])}"
        )
    return record


def tracked_files(root):
    record = require_git(["ls-files", "-z"], root, "git ls-files")
    paths = []
    for rel in record["stdout"].split("\0"):
        if not rel:
            continue
        path = root / rel
        try:
            if path.is_file() and not path.is_symlink():
                paths.append(rel)
        except OSError:
            continue
    if not paths:
        raise RuntimeError("fixture has no tracked regular files")
    return paths


def plan_for_actor(kind, actor_id, seed, operations, paths, large_paths):
    rng = random.Random(f"{seed}:{kind}:{actor_id}")
    plan = []
    for index in range(operations):
        if kind == "git_churn":
            action = ("status", "diff", "commit", "switch")[index % 4]
            plan.append({"action": action, "index": index, "slot": (index // 4) % 2})
        elif kind == "editor":
            action = "byte_edit" if index % 3 == 0 else "append"
            candidates = large_paths if action == "byte_edit" else paths
            rel = candidates[rng.randrange(len(candidates))]
            plan.append({
                "action": action,
                "index": index,
                "path": rel,
                "offset_seed": rng.randrange(2**31),
            })
        elif kind == "reader":
            action = "scan" if index % 4 == 0 else "small_read"
            plan.append({
                "action": action,
                "path": paths[rng.randrange(len(paths))],
                "offset_seed": rng.randrange(2**31),
            })
        elif kind == "builder":
            plan.append({
                "action": ("create", "rewrite", "delete")[index % 3],
                "index": index,
                "size": 1024 + rng.randrange(63 * 1024),
            })
        elif kind == "churner":
            plan.append({
                "action": ("rename", "unlink_open")[index % 2],
                "index": index,
                "size": 256 + rng.randrange(4096),
            })
        elif kind == "fetcher":
            plan.append({"action": "fetch", "index": index, "slot": index % 4})
    return plan


def plan_digest(plans):
    encoded = json.dumps(plans, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def warm_cache(root, paths, remote_url):
    started = time.perf_counter()
    digest = hashlib.sha256()
    selected = paths[: min(128, len(paths))]
    for rel in selected:
        path = root / rel
        try:
            stat = path.stat()
            digest.update(rel.encode("utf-8", errors="surrogateescape"))
            digest.update(str(stat.st_size).encode("ascii"))
            with path.open("rb") as handle:
                digest.update(handle.read(4096))
        except OSError:
            continue
    require_git(["status", "--short"], root, "cache warm git status")
    require_git(["ls-remote", remote_url, "HEAD"], root, "cache warm loopback remote")
    return {
        "strategy": "identical-read-warmup",
        "files_considered": len(selected),
        "digest": digest.hexdigest(),
        "duration_seconds": time.perf_counter() - started,
    }


def prepare_git_churn_repos(root, actor_specs):
    repos = {}
    for kind, actor_id, _plan in actor_specs:
        if kind != "git_churn":
            continue
        repo = root / f"chaos-git-repo-{actor_id}"
        require_git(
            ["clone", "--no-hardlinks", ".", str(repo)],
            root,
            "prepare git churn checkout",
        )
        for slot in range(2):
            branch = f"chaos-{actor_id}-{slot}"
            require_git(["branch", "-f", branch, "HEAD"], repo, "prepare churn branch")
        repos[actor_id] = repo
    return repos


def timed_operation(latencies, action, operation):
    started = time.perf_counter()
    detail = operation()
    elapsed = time.perf_counter() - started
    latencies.append(elapsed)
    return {"action": action, "duration_seconds": elapsed, "detail": detail}


def scheduled_items(plan):
    if not plan:
        return
    yielded = False
    while DEADLINE is None or time.perf_counter() < DEADLINE or not yielded:
        for item in plan:
            if DEADLINE is not None and time.perf_counter() >= DEADLINE and yielded:
                return
            yielded = True
            yield item
        if DEADLINE is None:
            return


def git_churn(root, actor_id, plan, barrier, repo_control_lock):
    latencies = []
    records = []
    commits = 0
    barrier.wait()
    for item in scheduled_items(plan):
        action = item["action"]
        if action == "status":
            def operation():
                record = require_git(["status", "--short", "--branch"], root, "git churn status")
                return {"stdout_bytes": len(record["stdout"].encode())}
        elif action == "diff":
            def operation():
                record = require_git(["diff", "--stat", "--"], root, "git churn diff")
                return {"stdout_bytes": len(record["stdout"].encode())}
        elif action == "commit":
            def operation(item=item):
                nonlocal commits
                with repo_control_lock:
                    before = require_git(
                        ["rev-parse", "HEAD"],
                        root,
                        "resolve pre-commit head",
                    )["stdout"].strip()
                    require_git(["add", "--refresh", "--", "."], root, "git churn add refresh")
                    commit = require_git(
                        [
                            "commit",
                            "--allow-empty",
                            "-m",
                            f"chaos actor {actor_id} op {item['index']}",
                        ],
                        root,
                        "git churn commit",
                    )
                    after = require_git(
                        ["rev-parse", "HEAD"],
                        root,
                        "resolve post-commit head",
                    )["stdout"].strip()
                    parent_tree = require_git(
                        ["rev-parse", "HEAD^1^{tree}"],
                        root,
                        "resolve parent tree",
                    )["stdout"].strip()
                    head_tree = require_git(
                        ["rev-parse", "HEAD^{tree}"],
                        root,
                        "resolve commit tree",
                    )["stdout"].strip()
                    if before == after or parent_tree != head_tree:
                        raise RuntimeError(
                            "git churn commit verification failed: "
                            f"before={before!r} after={after!r} "
                            f"parent_tree={parent_tree!r} head_tree={head_tree!r}"
                        )
                    commits += 1
                    return {
                        "before": before,
                        "after": after,
                        "tree": head_tree,
                        "commit_stdout_tail": tail(commit["stdout"]),
                    }
        else:
            def operation(item=item):
                with repo_control_lock:
                    branch = f"chaos-{actor_id}-{item['slot']}"
                    require_git(
                        ["checkout", branch],
                        root,
                        "git churn branch switch",
                    )
                    current = require_git(["branch", "--show-current"], root, "verify branch switch")
                    observed = current["stdout"].strip()
                    head_text = (root / ".git" / "HEAD").read_text(encoding="utf-8").strip()
                    expected_head = f"ref: refs/heads/{branch}"
                    if observed != branch or head_text != expected_head:
                        raise RuntimeError(
                            "branch switch verification failed: "
                            f"expected={branch!r} observed={observed!r} "
                            f"head={head_text!r}"
                        )
                    return {"branch": branch, "head": head_text}
        records.append(timed_operation(latencies, action, operation))
    return {"latencies": latencies, "records": records, "verified": True, "commits": commits}


def editor(root, actor_id, plan, barrier, locks):
    latencies = []
    records = []
    bytes_written = 0
    barrier.wait()
    for item in scheduled_items(plan):
        rel = item["path"]
        path = root / rel
        lock = locks.setdefault(rel, threading.Lock())

        def operation(item=item, path=path, lock=lock):
            nonlocal bytes_written
            with lock:
                if not path.exists() or not path.is_file():
                    return {"path": item["path"], "skipped": "not a regular file"}
                size = path.stat().st_size
                if item["action"] == "byte_edit" and size:
                    offset = item["offset_seed"] % size
                    with path.open("r+b", buffering=0) as handle:
                        handle.seek(offset)
                        before = handle.read(1)
                        after = bytes([(before[0] + 1) % 256])
                        handle.seek(offset)
                        handle.write(after)
                        if item["index"] % 2 == 0:
                            handle.flush()
                            os.fsync(handle.fileno())
                        handle.seek(offset)
                        observed = handle.read(1)
                    if observed != after:
                        raise RuntimeError(f"editor byte read-back failed for {item['path']}")
                    bytes_written += 1
                    return {"path": item["path"], "offset": offset, "bytes": 1, "fsync": item["index"] % 2 == 0}
                payload = f"\nchaos-editor actor={actor_id} op={item['index']}\n".encode()
                with path.open("ab", buffering=0) as handle:
                    handle.write(payload)
                    if item["index"] % 2 == 0:
                        handle.flush()
                        os.fsync(handle.fileno())
                with path.open("rb") as handle:
                    handle.seek(-len(payload), os.SEEK_END)
                    observed = handle.read()
                if observed != payload:
                    raise RuntimeError(f"editor append read-back failed for {item['path']}")
                bytes_written += len(payload)
                return {"path": item["path"], "bytes": len(payload), "fsync": item["index"] % 2 == 0}

        records.append(timed_operation(latencies, item["action"], operation))
    return {"latencies": latencies, "records": records, "verified": True, "bytes_written": bytes_written}


def reader(root, actor_id, plan, barrier):
    latencies = []
    records = []
    bytes_read = 0
    barrier.wait()
    for item in scheduled_items(plan):
        def operation(item=item):
            nonlocal bytes_read
            digest = hashlib.sha256()
            files = 0
            read = 0
            if item["action"] == "scan":
                for dirpath, dirnames, filenames in os.walk(root):
                    dirnames[:] = sorted(name for name in dirnames if name != ".git")
                    for name in sorted(filenames):
                        path = Path(dirpath) / name
                        try:
                            stat = path.stat()
                            with path.open("rb") as handle:
                                data = handle.read(4096)
                        except (FileNotFoundError, IsADirectoryError):
                            continue
                        digest.update(str(path.relative_to(root)).encode("utf-8", errors="surrogateescape"))
                        digest.update(str(stat.st_size).encode("ascii"))
                        digest.update(data)
                        files += 1
                        read += len(data)
            else:
                path = root / item["path"]
                try:
                    size = path.stat().st_size
                    offset = item["offset_seed"] % max(1, size)
                    with path.open("rb", buffering=0) as handle:
                        handle.seek(offset)
                        data = handle.read(4096)
                except FileNotFoundError:
                    data = b""
                    offset = 0
                digest.update(data)
                files = 1
                read = len(data)
            bytes_read += read
            return {"files": files, "bytes": read, "digest": digest.hexdigest()}

        records.append(timed_operation(latencies, item["action"], operation))
    return {"latencies": latencies, "records": records, "verified": True, "bytes_read": bytes_read}


def builder(root, actor_id, plan, barrier):
    build_root = root / "chaos-build" / f"actor-{actor_id}"
    build_root.mkdir(parents=True, exist_ok=True)
    latencies = []
    records = []
    live = {}
    barrier.wait()
    for item in scheduled_items(plan):
        path = build_root / f"artifact-{item['index'] % 5}.bin"

        def operation(item=item, path=path):
            payload = hashlib.sha256(f"builder:{actor_id}:{item['index']}".encode()).digest()
            payload = (payload * ((item["size"] // len(payload)) + 1))[: item["size"]]
            if item["action"] == "delete":
                path.unlink(missing_ok=True)
                if path.exists():
                    raise RuntimeError(f"builder delete verification failed for {path}")
                live.pop(path.name, None)
                return {"path": path.name, "deleted": True}
            mode = "wb" if item["action"] == "rewrite" else "xb"
            if mode == "xb" and path.exists():
                path.unlink()
            with path.open(mode, buffering=0) as handle:
                handle.write(payload)
                if item["index"] % 2 == 0:
                    handle.flush()
                    os.fsync(handle.fileno())
            observed = path.read_bytes()
            if observed != payload:
                raise RuntimeError(f"builder read-back failed for {path}")
            live[path.name] = hashlib.sha256(payload).hexdigest()
            return {"path": path.name, "bytes": len(payload), "sha256": live[path.name]}

        records.append(timed_operation(latencies, item["action"], operation))
    for name, expected in live.items():
        if hashlib.sha256((build_root / name).read_bytes()).hexdigest() != expected:
            raise RuntimeError(f"builder final verification failed for {name}")
    return {"latencies": latencies, "records": records, "verified": True, "live_artifacts": len(live)}


def churner(root, actor_id, plan, barrier):
    churn_root = root / "chaos-churn" / f"actor-{actor_id}"
    churn_root.mkdir(parents=True, exist_ok=True)
    latencies = []
    records = []
    barrier.wait()
    for item in scheduled_items(plan):
        payload = hashlib.sha256(f"churner:{actor_id}:{item['index']}".encode()).digest()
        payload = (payload * ((item["size"] // len(payload)) + 1))[: item["size"]]

        def operation(item=item, payload=payload):
            source = churn_root / f"source-{item['index']}.bin"
            destination = churn_root / f"destination-{item['index']}.bin"
            source.write_bytes(payload)
            if item["action"] == "rename":
                os.rename(source, destination)
                if source.exists() or destination.read_bytes() != payload:
                    raise RuntimeError("churner rename verification failed")
                destination.unlink()
                return {"bytes": len(payload), "renamed": True}
            with source.open("r+b", buffering=0) as handle:
                os.unlink(source)
                if source.exists():
                    raise RuntimeError("unlink-while-open path remained visible")
                handle.seek(0)
                observed = handle.read()
                handle.seek(0, os.SEEK_END)
                handle.write(b"still-open")
                handle.flush()
                os.fsync(handle.fileno())
                handle.seek(len(payload))
                appended = handle.read()
            if observed != payload or appended != b"still-open" or source.exists():
                raise RuntimeError("unlink-while-open verification failed")
            return {"bytes": len(payload), "unlinked_while_open": True}

        records.append(timed_operation(latencies, item["action"], operation))
    return {"latencies": latencies, "records": records, "verified": True}


def fetcher(root, actor_id, plan, barrier, remote_url, repo_control_lock):
    latencies = []
    records = []
    barrier.wait()
    for item in scheduled_items(plan):
        def operation(item=item):
            with repo_control_lock:
                ref = f"refs/remotes/chaos-fetch/{actor_id}-{item['slot']}"
                record = require_git(
                    ["fetch", "--no-tags", remote_url, f"HEAD:{ref}"],
                    root,
                    "loopback git fetch",
                )
                resolved = require_git(["rev-parse", "--verify", ref], root, "verify fetched ref")
                fetch_head = root / ".git" / "FETCH_HEAD"
                if not fetch_head.is_file() or fetch_head.stat().st_size == 0:
                    raise RuntimeError("fetch did not write FETCH_HEAD")
                return {
                    "ref": ref,
                    "commit": resolved["stdout"].strip(),
                    "attempts": len(record["attempts"]),
                }

        records.append(timed_operation(latencies, item["action"], operation))
    return {"latencies": latencies, "records": records, "verified": True}


def actor_operations(kind, base_operations, intensities):
    return base_operations * intensities[kind]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--actors", type=int, required=True)
    parser.add_argument("--operations", type=int, required=True)
    parser.add_argument("--duration", type=float)
    parser.add_argument("--remote-url", required=True)
    for kind in ACTOR_KINDS:
        parser.add_argument(f"--{kind.replace('_', '-')}-intensity", type=int, required=True)
    args = parser.parse_args()

    root = Path.cwd()
    paths = tracked_files(root)
    large_paths = sorted(paths, key=lambda rel: (root / rel).stat().st_size, reverse=True)[
        : max(1, min(16, len(paths)))
    ]
    intensities = {
        kind: getattr(args, f"{kind}_intensity")
        for kind in ACTOR_KINDS
    }
    actor_specs = []
    plans = {}
    for actor_id in range(args.actors):
        kind = ACTOR_KINDS[actor_id % len(ACTOR_KINDS)]
        operations = actor_operations(kind, args.operations, intensities)
        plan = plan_for_actor(kind, actor_id, args.seed, operations, paths, large_paths)
        actor_specs.append((kind, actor_id, plan))
        plans[f"{kind}-{actor_id}"] = plan

    git_churn_repos = prepare_git_churn_repos(root, actor_specs)
    warmup = warm_cache(root, paths, args.remote_url)
    barrier = threading.Barrier(args.actors)
    locks = {}
    started = time.perf_counter()
    global DEADLINE
    DEADLINE = started + args.duration if args.duration else None
    git_churn_locks = {
        actor_id: threading.Lock() for actor_id in git_churn_repos
    }
    fetch_lock = threading.Lock()

    def run_actor(spec):
        kind, actor_id, planned = spec
        if kind == "git_churn":
            result = git_churn(
                git_churn_repos[actor_id],
                actor_id,
                planned,
                barrier,
                git_churn_locks[actor_id],
            )
        elif kind == "editor":
            result = editor(root, actor_id, planned, barrier, locks)
        elif kind == "reader":
            result = reader(root, actor_id, planned, barrier)
        elif kind == "builder":
            result = builder(root, actor_id, planned, barrier)
        elif kind == "churner":
            result = churner(root, actor_id, planned, barrier)
        else:
            result = fetcher(
                root,
                actor_id,
                planned,
                barrier,
                args.remote_url,
                fetch_lock,
            )
        result["kind"] = kind
        result["actor_id"] = actor_id
        result["operation_count"] = len(result["latencies"])
        result["latency_seconds"] = stats(result["latencies"])
        del result["latencies"]
        return result

    actors = []
    errors = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.actors) as executor:
        futures = {executor.submit(run_actor, spec): spec for spec in actor_specs}
        for future, spec in futures.items():
            try:
                actors.append(future.result())
            except Exception as exc:
                errors.append({"kind": spec[0], "actor_id": spec[1], "error": str(exc)})
    actors.sort(key=lambda item: item["actor_id"])
    measured_seconds = time.perf_counter() - started

    fsck = run_git(["fsck", "--strict"], root)
    actor_verified = not errors and all(actor.get("verified") is True for actor in actors)
    total_ops = sum(actor.get("operation_count", 0) for actor in actors)
    result = {
        "seed": args.seed,
        "plan_digest": plan_digest(plans),
        "reproducibility": {
            "mode": "seeded-fixed-operations" if args.duration is None else "seeded-duration",
            "action_plan_reproducible": args.duration is None,
        },
        "cache_warmup": warmup,
        "git_churn_repositories": {
            str(actor_id): str(path.relative_to(root))
            for actor_id, path in sorted(git_churn_repos.items())
        },
        "measured_seconds": measured_seconds,
        "total_ops": total_ops,
        "actors": actors,
        "actor_errors": errors,
        "actor_verified": actor_verified,
        "git_fsck": {
            "returncode": fsck["returncode"],
            "ok": fsck["returncode"] == 0,
            "stderr_tail": tail(fsck["stderr"]),
        },
        "passed": actor_verified and fsck["returncode"] == 0,
    }
    print(json.dumps(result, sort_keys=True))


try:
    main()
except Exception as exc:
    print(json.dumps({"error": str(exc), "passed": False}, sort_keys=True))
    raise
"""


def at_least_six(value: str) -> int:
    parsed = int(value)
    if parsed < len(ACTOR_KINDS):
        raise argparse.ArgumentTypeError(f"must be >= {len(ACTOR_KINDS)}")
    return parsed


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return parsed


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Measure a seeded concurrent agent-like workload on native storage "
            "and through Vfs, with interleaved legs and absolute dispersion."
        )
    )
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--source", help="local Git repository or worktree")
    source.add_argument(
        "--remote", help="remote repository cloned into a local fixture mirror"
    )
    source.add_argument(
        "--synthetic",
        action="store_true",
        help="use a generated toy fixture; payload is marked non-comparable",
    )
    parser.add_argument("--samples", type=positive_int, default=5)
    parser.add_argument(
        "--warmup",
        type=non_negative_int,
        default=1,
        help="leading samples run but discarded, so a cold fixture cache does not enter the distribution",
    )
    parser.add_argument("--seed", type=int, default=20250808)
    parser.add_argument("--actors", type=at_least_six, default=6)
    parser.add_argument("--operations", type=positive_int, default=8)
    parser.add_argument(
        "--duration",
        type=positive_float,
        help="run actors by approximate duration instead of fixed operations; less reproducible",
    )
    for kind in ACTOR_KINDS:
        parser.add_argument(
            f"--{kind.replace('_', '-')}-intensity",
            type=non_negative_int,
            default=1,
            help=f"multiply the base operation count for {kind} actors (0 disables work)",
        )
    parser.add_argument("--synthetic-files", type=positive_int, default=128)
    parser.add_argument("--synthetic-large-file-mib", type=positive_int, default=8)
    parser.add_argument(
        "--fetch-url",
        help=(
            "explicit non-hermetic fetch target; default is a local loopback "
            "git daemon and any override marks the payload non-comparable"
        ),
    )
    parser.add_argument(
        "--vfs-bin",
        default=os.environ.get("VFS_BIN"),
        help="vfs executable path/name (default: repo target binary)",
    )
    parser.add_argument("--timeout", type=positive_float, default=300.0)
    parser.add_argument(
        "--profile", action="store_true", default=env_flag("VFS_PROFILE")
    )
    parser.add_argument(
        "--keep-temp", action="store_true", default=env_flag("CHAOS_KEEP_TEMP")
    )
    parser.add_argument("--output", help="write the JSON payload to this path")
    parser.add_argument("--json-indent", type=non_negative_int, default=2)
    parser.add_argument(
        "--self-test-base-mutation-guard",
        action="store_true",
        help="mutate the temporary Vfs base after its run; the benchmark must fail",
    )
    return parser.parse_args(argv)


def git_env() -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("GIT_CONFIG_NOSYSTEM", "1")
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    env.setdefault("NO_COLOR", "1")
    env.setdefault("LC_ALL", "C")
    env["GIT_AUTHOR_NAME"] = "Vfs Chaos Benchmark"
    env["GIT_AUTHOR_EMAIL"] = "vfs-chaos@example.invalid"
    env["GIT_COMMITTER_NAME"] = "Vfs Chaos Benchmark"
    env["GIT_COMMITTER_EMAIL"] = "vfs-chaos@example.invalid"
    return env


def run_git(
    argv: list[str],
    cwd: Path,
    env: dict[str, str],
    timeout: float,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [env.get("GIT", "git")] + argv,
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
            f"stdout:\n{tail_text(proc.stdout)}\n"
            f"stderr:\n{tail_text(proc.stderr)}"
        )


def create_synthetic_repo(
    root: Path, files: int, large_file_mib: int, env: dict[str, str]
) -> None:
    root.mkdir(parents=True, exist_ok=True)
    require_git_ok(
        run_git(["init", "-b", "main"], root, env, 60), "git init synthetic fixture"
    )
    for index in range(files):
        directory = (
            root / ("src" if index % 2 == 0 else "tests") / f"pkg-{index % 16:02d}"
        )
        directory.mkdir(parents=True, exist_ok=True)
        seed = hashlib.sha256(f"chaos-synthetic-{index}".encode()).hexdigest()
        content = "".join(f"{line:04d} {seed} CHAOS_TOKEN\n" for line in range(32))
        (directory / f"file-{index:04d}.txt").write_text(content, encoding="utf-8")
    large = root / "data" / "large-origin.bin"
    large.parent.mkdir(parents=True, exist_ok=True)
    block = hashlib.sha256(b"chaos-large-origin").digest() * 32768
    with large.open("wb") as handle:
        for _ in range(large_file_mib):
            handle.write(block)
    require_git_ok(run_git(["add", "."], root, env, 60), "git add synthetic fixture")
    commit_env = env.copy()
    commit_env["GIT_AUTHOR_DATE"] = "2024-01-01T00:00:00Z"
    commit_env["GIT_COMMITTER_DATE"] = "2024-01-01T00:00:00Z"
    require_git_ok(
        run_git(["commit", "-m", "deterministic chaos fixture"], root, commit_env, 60),
        "git commit synthetic fixture",
    )


def prepare_fixture(
    args: argparse.Namespace,
    temp_root: Path,
    env: dict[str, str],
) -> tuple[Path, Path, dict[str, Any]]:
    prepared = temp_root / "prepared"
    prepared.mkdir(parents=True, exist_ok=True)
    mirror = prepared / "mirror.git"
    if args.remote:
        require_git_ok(
            run_git(
                ["clone", "--mirror", args.remote, str(mirror)],
                prepared,
                env,
                args.timeout,
            ),
            "git clone --mirror remote fixture",
        )
        kind = "remote"
        source_path = args.remote
    elif args.source:
        source = Path(args.source).expanduser().resolve()
        if not source.exists():
            raise RuntimeError(f"--source does not exist: {source}")
        require_git_ok(
            run_git(
                ["clone", "--mirror", str(source), str(mirror)],
                prepared,
                env,
                args.timeout,
            ),
            "git clone --mirror source fixture",
        )
        kind = "source"
        source_path = str(source)
    elif not args.synthetic and CANONICAL_FIXTURE.is_dir():
        print(f"Using canonical fixture {CANONICAL_FIXTURE}", file=sys.stderr)
        require_git_ok(
            run_git(
                ["clone", "--mirror", str(CANONICAL_FIXTURE), str(mirror)],
                prepared,
                env,
                args.timeout,
            ),
            "git clone --mirror canonical fixture",
        )
        kind = "canonical-fixture"
        source_path = str(CANONICAL_FIXTURE)
    elif not args.synthetic:
        raise RuntimeError(
            f"canonical fixture not found at {CANONICAL_FIXTURE}; refusing to guess. "
            "Use --source, --remote, or explicitly accept --synthetic."
        )
    else:
        generated = prepared / "synthetic-source"
        create_synthetic_repo(
            generated,
            args.synthetic_files,
            args.synthetic_large_file_mib,
            env,
        )
        require_git_ok(
            run_git(
                ["clone", "--mirror", str(generated), str(mirror)],
                prepared,
                env,
                args.timeout,
            ),
            "git clone --mirror synthetic fixture",
        )
        kind = "synthetic"
        source_path = str(generated)

    head = run_git(
        ["--git-dir", str(mirror), "rev-parse", "HEAD"], prepared, env, args.timeout
    )
    require_git_ok(head, "resolve fixture HEAD")
    template = prepared / "template"
    require_git_ok(
        run_git(
            ["clone", "--no-hardlinks", str(mirror), str(template)],
            prepared,
            env,
            args.timeout,
        ),
        "prepare workload template",
    )
    exclude = template / ".git" / "info" / "exclude"
    with exclude.open("a", encoding="utf-8") as handle:
        handle.write("\n/chaos-build/\n/chaos-churn/\n")
    return (
        mirror,
        template,
        {
            "kind": kind,
            "path": source_path,
            "mirror_head": head.stdout.strip(),
            "comparable_to_scoreboard": kind == "canonical-fixture",
        },
    )


def free_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def start_git_daemon(
    mirror: Path,
    env: dict[str, str],
) -> tuple[subprocess.Popen[str], str]:
    port = free_loopback_port()
    root = mirror.parent
    proc = subprocess.Popen(
        [
            env.get("GIT", "git"),
            "daemon",
            "--reuseaddr",
            "--export-all",
            "--verbose",
            "--informative-errors",
            "--listen=127.0.0.1",
            f"--port={port}",
            f"--base-path={root}",
            str(root),
        ],
        cwd=str(root),
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    url = f"git://127.0.0.1:{port}/{mirror.name}"
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        probe = run_git(["ls-remote", url, "HEAD"], root, env, 2)
        if probe.returncode == 0:
            return proc, url
        if proc.poll() is not None:
            stdout, stderr = proc.communicate()
            raise RuntimeError(
                f"git daemon exited before becoming ready\nstdout:\n{stdout}\nstderr:\n{stderr}"
            )
        time.sleep(0.05)
    common.terminate_process_tree(proc)
    raise RuntimeError("timed out waiting for loopback git daemon")


def isolate_env(base_env: dict[str, str], leg_root: Path) -> dict[str, str]:
    """Give one measured leg its own HOME, XDG dirs and TMPDIR.

    A HOME shared across legs carries a session store, git config and caches
    from one measurement into the next, so a sample can be measuring state its
    predecessor left behind. Each leg gets a fresh context instead.
    """
    env = dict(base_env)
    home = leg_root / "home"
    for path in (home, home / ".config", home / ".cache", home / ".local" / "share"):
        path.mkdir(parents=True, exist_ok=True)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(home / ".config")
    env["XDG_CACHE_HOME"] = str(home / ".cache")
    env["XDG_DATA_HOME"] = str(home / ".local" / "share")
    common.pin_distro_git(env, home, home=home)
    tmp = leg_root / "tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(tmp)
    env["TMP"] = str(tmp)
    env["TEMP"] = str(tmp)
    return env


def mounts_under(root: Path) -> list[str]:
    """Live mountpoints below one leg context."""
    prefix = str(root.resolve()).rstrip(os.sep) + os.sep
    mounts: list[str] = []
    try:
        lines = Path("/proc/self/mountinfo").read_text(
            encoding="utf-8", errors="replace"
        ).splitlines()
    except OSError:
        return mounts
    for line in lines:
        fields = line.split(" - ", 1)[0].split()
        if len(fields) < 5:
            continue
        mountpoint = fields[4].replace("\\040", " ")
        if mountpoint == str(root) or mountpoint.startswith(prefix):
            mounts.append(mountpoint)
    return sorted(mounts)


def processes_for_leg(session: str, leg_root: Path) -> list[dict[str, Any]]:
    """Processes retaining a session id or an isolated leg path."""
    root_bytes = str(leg_root).encode()
    session_bytes = session.encode()
    matches: list[dict[str, Any]] = []
    try:
        entries = list(Path("/proc").iterdir())
    except OSError:
        return matches
    for entry in entries:
        if not entry.name.isdigit() or int(entry.name) == os.getpid():
            continue
        try:
            cmdline = (entry / "cmdline").read_bytes()
            environ = (entry / "environ").read_bytes()
        except OSError:
            continue
        if (
            session_bytes not in cmdline
            and session_bytes not in environ
            and root_bytes not in cmdline
            and root_bytes not in environ
        ):
            continue
        matches.append(
            {
                "pid": int(entry.name),
                "cmdline": " ".join(
                    chunk.decode("utf-8", "replace")
                    for chunk in cmdline.split(b"\0")
                    if chunk
                ),
            }
        )
    return matches


def wait_for_leg_cleanup(
    session: str, leg_root: Path, timeout_seconds: float = 2.0
) -> dict[str, Any]:
    """Prove a context is idle and recover it before another leg can start."""
    started = time.monotonic()
    processes: list[dict[str, Any]] = []
    mounts: list[str] = []
    while True:
        processes = processes_for_leg(session, leg_root)
        mounts = mounts_under(leg_root)
        if not processes and not mounts:
            break
        if time.monotonic() - started >= timeout_seconds:
            break
        time.sleep(0.05)
    clean_exit = not processes and not mounts
    initial_processes = processes
    initial_mounts = mounts
    if not clean_exit:
        for process in initial_processes:
            try:
                os.kill(int(process["pid"]), signal.SIGTERM)
            except (OSError, ValueError):
                pass
        time.sleep(0.2)
        for process in processes_for_leg(session, leg_root):
            try:
                os.kill(int(process["pid"]), signal.SIGKILL)
            except (OSError, ValueError):
                pass
        fusermount = shutil.which("fusermount3")
        if fusermount is not None:
            for mount in sorted(initial_mounts, reverse=True):
                subprocess.run(
                    [fusermount, "-uz", mount],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                )
        recovery_deadline = time.monotonic() + timeout_seconds
        while True:
            processes = processes_for_leg(session, leg_root)
            mounts = mounts_under(leg_root)
            if not processes and not mounts:
                break
            if time.monotonic() >= recovery_deadline:
                break
            time.sleep(0.05)
    return {
        "ok": clean_exit,
        "clean_exit": clean_exit,
        "recovery_attempted": not clean_exit,
        "final_clean": not processes and not mounts,
        "initial_processes": initial_processes,
        "initial_mounts": initial_mounts,
        "processes": processes,
        "mounts": mounts,
        "waited_seconds": time.monotonic() - started,
    }


def prepare_environment(temp_root: Path, profile: bool) -> dict[str, str]:
    env = git_env()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    env.setdefault("GIT_CONFIG_NOSYSTEM", "1")
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    if profile:
        env["VFS_PROFILE"] = "1"
    else:
        env.pop("VFS_PROFILE", None)
    return isolate_env(env, temp_root / "setup")


def workload_argv(args: argparse.Namespace, remote_url: str) -> list[str]:
    argv = [
        sandbox_python(),
        "-c",
        CHAOS_WORKLOAD,
        "--seed",
        str(args.seed),
        "--actors",
        str(args.actors),
        "--operations",
        str(args.operations),
        "--remote-url",
        remote_url,
    ]
    if args.duration is not None:
        argv.extend(["--duration", str(args.duration)])
    for kind in ACTOR_KINDS:
        argv.extend(
            [
                f"--{kind.replace('_', '-')}-intensity",
                str(getattr(args, f"{kind}_intensity")),
            ]
        )
    return argv


def compact_run(run: dict[str, Any]) -> dict[str, Any]:
    compact = {key: value for key, value in run.items() if key != "stdout"}
    argv = compact.get("argv")
    if isinstance(argv, list) and "-c" in argv:
        index = argv.index("-c")
        if index + 1 < len(argv):
            compact["argv"] = [
                *argv[: index + 1],
                "<embedded chaos workload>",
                *argv[index + 2 :],
            ]
    return compact


def profile_counters(summaries: list[dict[str, Any]]) -> dict[str, Any]:
    max_counters: dict[str, int] = {}
    by_source: dict[str, dict[str, int]] = {}
    for summary in summaries:
        counters = summary.get("counters")
        if not isinstance(counters, dict):
            continue
        numeric = {
            str(key): int(value)
            for key, value in counters.items()
            if isinstance(value, int)
        }
        by_source[str(summary.get("source", "unknown"))] = numeric
        for key, value in numeric.items():
            max_counters[key] = max(max_counters.get(key, 0), value)
    per_operation = {
        key: value
        for key, value in max_counters.items()
        if key.startswith(("fuse_", "op_", "fs_"))
        or key
        in {
            "lookup",
            "getattr",
            "open",
            "read",
            "write",
            "release",
            "readdir",
            "create",
            "rename",
            "unlink",
            "flush",
            "fsync",
        }
    }
    return {
        "summary_count": len(summaries),
        "max_counters": max_counters,
        "fuse_per_operation": per_operation,
        "last_by_source": by_source,
    }


def probe_engine(vfs_bin: str, cwd: Path, env: dict[str, str], timeout: float) -> dict[str, Any]:
    """Identify the binary under measurement and what it can be asked to do.

    The same workload is run against engines that do not share a feature set --
    a pre-fork agentfs build has neither partial-origin copy-up nor the
    integrity subcommand. Probing keeps one code path and makes the payload
    state which verifications were actually available, so a reader cannot
    mistake "not checked" for "checked and clean".
    """
    version = run_subprocess([vfs_bin, "--version"], cwd, env, timeout, keep_stdout=True)
    run_help = run_subprocess([vfs_bin, "run", "--help"], cwd, env, timeout, keep_stdout=True)
    integrity_help = run_subprocess(
        [vfs_bin, "integrity", "--help"], cwd, env, timeout, keep_stdout=True
    )
    run_help_text = str(run_help.get("stdout", "")) + str(run_help.get("stderr", ""))
    uring_setting = env.get("VFS_FUSE_URING")
    kernel_uring_path = Path("/sys/module/fuse/parameters/enable_uring")
    try:
        kernel_uring_enabled = kernel_uring_path.read_text(encoding="utf-8").strip()
    except OSError:
        kernel_uring_enabled = None
    return {
        "bin": vfs_bin,
        "version": str(version.get("stdout", "")).strip() or None,
        "requested_fuse_transport": (
            "uring"
            if uring_setting == "1"
            else "legacy"
            if uring_setting == "0"
            else "engine-default"
        ),
        "kernel_fuse_uring_enabled": kernel_uring_enabled,
        "capabilities": {
            "partial_origin": "--partial-origin" in run_help_text,
            "integrity": integrity_help["returncode"] == 0,
        },
    }


def run_integrity(
    vfs_bin: str,
    db_path: Path,
    cwd: Path,
    env: dict[str, str],
    timeout: float,
) -> dict[str, Any]:
    run = run_subprocess(
        [vfs_bin, "integrity", str(db_path), "--json", "--check-base", "--checkpoint"],
        cwd,
        env,
        timeout,
        keep_stdout=True,
    )
    payload = parse_json_stdout(run)
    return {
        "run": compact_run(run),
        "result": payload,
        "ok": run["returncode"] == 0
        and isinstance(payload, dict)
        and payload.get("ok") is True,
    }


def actor_stats(runs: list[dict[str, Any]]) -> dict[str, Any]:
    by_kind: dict[str, list[float]] = {}
    operations: dict[str, int] = {}
    for run in runs:
        workload = run.get("workload")
        if not isinstance(workload, dict):
            continue
        for actor in workload.get("actors", []):
            kind = str(actor.get("kind"))
            records = actor.get("records", [])
            for record in records:
                value = record.get("duration_seconds")
                if isinstance(value, (int, float)):
                    by_kind.setdefault(kind, []).append(float(value))
            operations[kind] = operations.get(kind, 0) + int(
                actor.get("operation_count", 0) or 0
            )
    return {
        kind: {
            "latency_seconds": {
                **summarize_floats(values),
                "p95": common.percentile(values, 0.95),
                "p99": common.percentile(values, 0.99),
            },
            "total_ops": operations.get(kind, 0),
        }
        for kind, values in sorted(by_kind.items())
    }


def render_summary(result: dict[str, Any]) -> str:
    stats = result.get("absolute_wall_seconds", {})
    native = stats.get("native", {})
    vfs = stats.get("vfs", {})
    derived = result.get("derived", {})
    ratio = derived.get("vfs_over_native_median")
    ratio_text = f"{ratio:.2f}x" if isinstance(ratio, (int, float)) else "n/a"
    return (
        f"chaos workload: {'PASS' if result.get('passed') else 'FAIL'}\n"
        f"  native median {native.get('median', float('nan')):.3f}s "
        f"(p25 {native.get('p25', float('nan')):.3f}, p75 {native.get('p75', float('nan')):.3f}, n={native.get('n', 0)})\n"
        f"  vfs    median {vfs.get('median', float('nan')):.3f}s "
        f"(p25 {vfs.get('p25', float('nan')):.3f}, p75 {vfs.get('p75', float('nan')):.3f}, n={vfs.get('n', 0)})\n"
        f"  derived median ratio {ratio_text}; "
        f"total measured ops {result.get('total_ops', 0)}; seed {result.get('seed')}"
    )


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    if args.keep_temp:
        temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
        temp_root = Path(tempfile.mkdtemp(prefix="vfs-chaos-workload-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(
            prefix="vfs-chaos-workload-",
            ignore_cleanup_errors=True,
        )
        temp_root = Path(temp_manager.name)

    daemon: Optional[subprocess.Popen[str]] = None
    exit_code = 0
    result: dict[str, Any]
    git_ai_before = common.git_ai_processes()
    try:
        base_env = prepare_environment(temp_root, args.profile)
        env = base_env
        vfs_bin = resolve_vfs_bin(args.vfs_bin, repo_root)
        engine = probe_engine(vfs_bin, temp_root, env, args.timeout)
        mirror, template, source = prepare_fixture(args, temp_root, env)
        if args.fetch_url:
            remote_url = args.fetch_url
            cache_control_comparable = False
            network = {
                "kind": "explicit-fetch-url",
                "url": args.fetch_url,
                "hermetic": False,
            }
        else:
            daemon, remote_url = start_git_daemon(mirror, env)
            cache_control_comparable = True
            network = {
                "kind": "loopback-git-daemon",
                "url": remote_url,
                "hermetic": True,
            }
        source["comparable_to_scoreboard"] = (
            source["comparable_to_scoreboard"] and cache_control_comparable
        )

        invocation = workload_argv(args, remote_url)
        samples: list[dict[str, Any]] = []
        native_seconds: list[float] = []
        vfs_seconds: list[float] = []
        schedule = []
        total_sample_count = args.warmup + args.samples
        for sample_index in range(total_sample_count):
            # Leading samples are discarded: the first touch of the fixture
            # pays a cold page cache that no later sample repeats, and letting
            # it into the distribution is what produced medians that moved with
            # whichever engine happened to run first.
            warmup_sample = sample_index < args.warmup
            for leg in ("native", "vfs"):
                if not warmup_sample:
                    schedule.append(leg)
                leg_root = temp_root / "runs" / f"{sample_index:02d}-{leg}"
                root = leg_root / "checkout"
                shutil.copytree(template, root, symlinks=True)
                session = f"chaos-{sample_index}-{uuid.uuid4().hex}"
                env = isolate_env(base_env, leg_root)
                db_path = Path(env["HOME"]) / ".vfs" / "run" / session / "delta.db"
                base_before = tree_fingerprint(root) if leg == "vfs" else None
                command = invocation
                if leg == "vfs":
                    command = [vfs_bin, "run", "--session", session, "--no-default-allows"]
                    if engine["capabilities"]["partial_origin"]:
                        command.extend(["--partial-origin", "on"])
                    command.extend(["--", *invocation])
                run = run_subprocess(
                    command,
                    root,
                    env,
                    args.timeout,
                    OUTPUT_TAIL_CHARS,
                    keep_stdout=True,
                )
                workload = parse_json_stdout(run)
                if (
                    leg == "vfs"
                    and args.self_test_base_mutation_guard
                    and sample_index == 0
                ):
                    sabotage = root / ".chaos-base-mutation-guard"
                    sabotage.write_text(
                        "the invariant guard must catch this\n", encoding="utf-8"
                    )
                base_after = tree_fingerprint(root) if leg == "vfs" else None
                base_unchanged = (
                    base_before["sha256"] == base_after["sha256"]
                    if base_before is not None and base_after is not None
                    else None
                )
                integrity = (
                    run_integrity(vfs_bin, db_path, root, env, args.timeout)
                    if leg == "vfs" and db_path.exists() and engine["capabilities"]["integrity"]
                    else None
                )
                cleanup = (
                    wait_for_leg_cleanup(session, leg_root)
                    if leg == "vfs"
                    else {
                        "ok": True,
                        "processes": [],
                        "mounts": [],
                        "waited_seconds": 0.0,
                    }
                )
                measured = (
                    float(workload["measured_seconds"])
                    if isinstance(workload, dict)
                    and isinstance(workload.get("measured_seconds"), (int, float))
                    else None
                )
                if measured is not None and not warmup_sample:
                    (native_seconds if leg == "native" else vfs_seconds).append(
                        measured
                    )
                sample_passed = (
                    run["returncode"] == 0
                    and isinstance(workload, dict)
                    and workload.get("passed") is True
                    and measured is not None
                    and (leg != "vfs" or base_unchanged is True)
                    and cleanup["ok"] is True
                    # Invariant 2 is enforced for every engine. The integrity
                    # subcommand is a fork addition, so it gates the sample
                    # only where the engine offers it.
                    and (
                        leg != "vfs"
                        or not engine["capabilities"]["integrity"]
                        or integrity is not None
                        and integrity.get("ok") is True
                    )
                )
                samples.append(
                    {
                        "sample": sample_index,
                        "warmup": warmup_sample,
                        "leg": leg,
                        "session": session if leg == "vfs" else None,
                        "db_path": str(db_path) if leg == "vfs" else None,
                        "measured_seconds": measured,
                        "outer_seconds": run["duration_seconds"],
                        "run": compact_run(run),
                        "workload": workload,
                        "base_tree": (
                            {
                                "before": base_before,
                                "after": base_after,
                                "unchanged": base_unchanged,
                            }
                            if leg == "vfs"
                            else None
                        ),
                        "integrity": integrity,
                        "cleanup": cleanup,
                        "profile": (
                            profile_counters(run.get("profile_summaries", []))
                            if leg == "vfs" and args.profile
                            else None
                        ),
                        "passed": sample_passed,
                    }
                )
                print(
                    f"[{sample_index + 1}/{total_sample_count}] "
                    f"{'warmup ' if warmup_sample else ''}{leg}: "
                    f"{measured:.3f}s, ops={workload.get('total_ops')}, "
                    f"{'PASS' if sample_passed else 'FAIL'}"
                    if measured is not None and isinstance(workload, dict)
                    else (
                        f"[{sample_index + 1}/{total_sample_count}] "
                        f"{'warmup ' if warmup_sample else ''}{leg}: FAIL"
                    ),
                    file=sys.stderr,
                    flush=True,
                )

        measured_runs = [sample for sample in samples if not sample["warmup"]]
        native_runs = [
            sample for sample in measured_runs if sample["leg"] == "native"
        ]
        vfs_runs = [sample for sample in measured_runs if sample["leg"] == "vfs"]
        native_stats = summarize_floats(native_seconds)
        vfs_stats = summarize_floats(vfs_seconds)
        derived_ratio = (
            float(vfs_stats["median"]) / float(native_stats["median"])
            if native_stats.get("n")
            and vfs_stats.get("n")
            and float(native_stats["median"]) > 0
            else None
        )
        plan_digests = {
            str(sample.get("workload", {}).get("plan_digest"))
            for sample in samples
            if isinstance(sample.get("workload"), dict)
        }
        reproducible_plans = len(plan_digests) == 1 and args.duration is None
        total_ops = sum(
            int(sample.get("workload", {}).get("total_ops", 0) or 0)
            for sample in measured_runs
            if isinstance(sample.get("workload"), dict)
        )
        leaked_git_ai = common.git_ai_leaks(git_ai_before, common.git_ai_processes())
        passed = (
            all(sample["passed"] for sample in samples)
            and len(native_seconds) == args.samples
            and len(vfs_seconds) == args.samples
            and not leaked_git_ai
        )
        result = {
            "schema_version": 1,
            "benchmark": "chaos-workload",
            "git_commit": git_commit(repo_root),
            "seed": args.seed,
            "source": source,
            "network": network,
            "parameters": {
                "samples": args.samples,
                "warmup_samples_discarded": args.warmup,
                "actors": args.actors,
                "operations": args.operations,
                "duration_seconds": args.duration,
                "intensities": {
                    kind: getattr(args, f"{kind}_intensity") for kind in ACTOR_KINDS
                },
                "partial_origin": (
                    "on" if engine["capabilities"]["partial_origin"] else "unsupported-by-engine"
                ),
                "timeout_seconds": args.timeout,
            },
            "engine": engine,
            "measurement": {
                "schedule": schedule,
                "interleaved": schedule
                == [leg for _ in range(args.samples) for leg in ("native", "vfs")],
                "primary_metric": "absolute measured wall-clock seconds per leg",
                "ratio_role": "derived from leg medians after aggregation",
                "performance_threshold": None,
            },
            "cache_control": {
                "strategy": "identical in-workload warmup before each measured leg",
                "drop_caches_attempted": False,
                "drop_caches_reason": "unprivileged benchmark; sudo and global cache mutation are forbidden",
                "warmup_included_in_measured_seconds": False,
            },
            "absolute_wall_seconds": {
                "native": native_stats,
                "vfs": vfs_stats,
            },
            "derived": {
                "vfs_over_native_median": derived_ratio,
                "warning": "derived ratio; interpret only beside both absolute distributions",
            },
            "per_actor": {
                "native": actor_stats(native_runs),
                "vfs": actor_stats(vfs_runs),
            },
            "total_ops": total_ops,
            "reproducibility": {
                "mode": "seeded-fixed-operations"
                if args.duration is None
                else "seeded-duration",
                "plan_digests": sorted(plan_digests),
                "same_plan_all_legs": reproducible_plans,
            },
            "vfs": {
                "bin": vfs_bin,
                "profile_enabled": args.profile,
                "profile_samples": [
                    sample["profile"]
                    for sample in vfs_runs
                    if sample["profile"] is not None
                ],
            },
            "runs": samples,
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
            "schema_version": 1,
            "benchmark": "chaos-workload",
            "error": str(exc),
            "passed": False,
            "temp_dir": str(temp_root),
            "kept_temp": args.keep_temp,
        }
    finally:
        if daemon is not None:
            common.terminate_process_tree(daemon)

    print(render_summary(result), file=sys.stderr)
    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    if args.output:
        output = Path(args.output).expanduser()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(payload, encoding="utf-8")
        print(f"Wrote chaos workload JSON to {output}", file=sys.stderr)
    else:
        sys.stdout.write(payload)
    if temp_manager is not None:
        temp_manager.cleanup()
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
