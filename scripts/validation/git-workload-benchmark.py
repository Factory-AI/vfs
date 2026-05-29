#!/usr/bin/env python3
"""Phase 7 native-vs-AgentFS Git workload benchmark and principle gate."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from statistics import mean
from typing import Any, Optional


OUTPUT_TAIL_CHARS = 20000
HASH_BLOCK_BYTES = 1024 * 1024


GIT_WORKLOAD = r'''
import argparse
import hashlib
import json
import os
import signal
import sys
import subprocess
import time
from pathlib import Path


OUTPUT_TAIL_CHARS = 4000

# Ordered phase labels emitted via profiling checkpoints (see profile_checkpoint).
PROFILE_CHECKPOINTS = []


def profile_checkpoint(label):
    """Request an AgentFS profiling checkpoint at a phase boundary.

    Only meaningful when running inside an AgentFS sandbox with profiling
    enabled. We signal the parent `agentfs run` process (SIGUSR1), which emits a
    cumulative, sequence-tagged profile summary to its stderr; the analyzer
    subtracts consecutive checkpoints to obtain per-phase counter deltas. A small
    sleep lets the parent flush before the next phase begins. Guarded on AGENTFS
    so native runs never signal the benchmark harness.
    """
    PROFILE_CHECKPOINTS.append(label)
    if os.environ.get("AGENTFS") != "1":
        return
    if os.environ.get("AGENTFS_PROFILE", "") not in {"1", "true", "TRUE", "yes", "on"}:
        return
    try:
        os.kill(os.getppid(), signal.SIGUSR1)
    except OSError:
        return
    time.sleep(0.1)


def tail_text(value):
    if value is None:
        return ""
    if isinstance(value, bytes):
        text = value.decode("utf-8", errors="replace")
    else:
        text = str(value)
    if len(text) <= OUTPUT_TAIL_CHARS:
        return text
    return text[-OUTPUT_TAIL_CHARS:]


def git_env():
    env = os.environ.copy()
    env.setdefault("GIT_CONFIG_NOSYSTEM", "1")
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    env.setdefault("NO_COLOR", "1")
    env.setdefault("LC_ALL", "C")
    return env


def run_git(argv, cwd):
    started = time.perf_counter()
    proc = subprocess.run(
        ["git"] + argv,
        cwd=str(cwd),
        env=git_env(),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return {
        "argv": ["git"] + argv,
        "cwd": str(cwd),
        "duration_seconds": time.perf_counter() - started,
        "returncode": proc.returncode,
        "stdout_tail": tail_text(proc.stdout),
        "stderr_tail": tail_text(proc.stderr),
        "stdout_bytes": len((proc.stdout or "").encode("utf-8", errors="replace")),
        "stderr_bytes": len((proc.stderr or "").encode("utf-8", errors="replace")),
        "stdout": proc.stdout,
    }


def require_ok(record, phase):
    if record["returncode"] != 0:
        raise RuntimeError(
            f"{phase} failed with exit {record['returncode']}: {record['stderr_tail']}"
        )


def bounded_read_search(workdir, max_files, read_bytes, token):
    started = time.perf_counter()
    ls_files = run_git(["ls-files", "-z"], workdir)
    require_ok(ls_files, "ls-files")
    paths = [item for item in ls_files["stdout"].split("\0") if item]
    digest = hashlib.sha256()
    scanned = 0
    bytes_read = 0
    matches = 0
    selected = []
    for rel in paths:
        if scanned >= max_files:
            break
        path = workdir / rel
        if not path.is_file():
            continue
        data = path.read_bytes()[:read_bytes]
        digest.update(rel.encode("utf-8"))
        digest.update(b"\0")
        digest.update(str(path.stat().st_size).encode("ascii"))
        digest.update(b"\0")
        digest.update(data)
        matches += data.count(token.encode("utf-8"))
        bytes_read += len(data)
        scanned += 1
        selected.append(rel)
    return {
        "duration_seconds": time.perf_counter() - started,
        "ls_files_run": {key: value for key, value in ls_files.items() if key != "stdout"},
        "digest": digest.hexdigest(),
        "files_total": len(paths),
        "files_scanned": scanned,
        "bytes_read": bytes_read,
        "token": token,
        "matches": matches,
        "selected_files": selected,
        "all_files": paths,
    }


def representative_edit_paths(paths, limit):
    preferred_prefixes = ("src/", "tests/", "docs/")
    selected = []
    for prefix in preferred_prefixes:
        for rel in paths:
            if rel.startswith(prefix) and rel not in selected:
                selected.append(rel)
                if len(selected) >= limit:
                    return selected
    for rel in paths:
        if rel not in selected:
            selected.append(rel)
            if len(selected) >= limit:
                return selected
    return selected


def edit_files(workdir, paths, limit):
    started = time.perf_counter()
    selected = representative_edit_paths(paths, limit)
    edits = []
    for index, rel in enumerate(selected):
        path = workdir / rel
        before_size = path.stat().st_size
        payload = f"\nAgentFS Git benchmark edit {index:02d} for {rel}\n".encode("utf-8")
        with path.open("ab", buffering=0) as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        edits.append(
            {
                "path": rel,
                "size_before": before_size,
                "size_after": path.stat().st_size,
                "appended_bytes": len(payload),
            }
        )
    return {"duration_seconds": time.perf_counter() - started, "changed_files": selected, "edits": edits}


def diff_summary(workdir):
    started = time.perf_counter()
    name_only = run_git(["diff", "--name-only", "--"], workdir)
    require_ok(name_only, "diff --name-only")
    stat = run_git(["diff", "--stat", "--"], workdir)
    require_ok(stat, "diff --stat")
    patch = run_git(["diff", "--", "."], workdir)
    require_ok(patch, "diff")
    changed = [line for line in name_only["stdout"].splitlines() if line]
    patch_bytes = patch["stdout"].encode("utf-8", errors="replace")
    return {
        "duration_seconds": time.perf_counter() - started,
        "changed_files": changed,
        "changed_file_count": len(changed),
        "stat_stdout": stat["stdout_tail"],
        "patch_sha256": hashlib.sha256(patch_bytes).hexdigest(),
        "patch_bytes": len(patch_bytes),
        "runs": {
            "name_only": {key: value for key, value in name_only.items() if key != "stdout"},
            "stat": {key: value for key, value in stat.items() if key != "stdout"},
            "patch": {key: value for key, value in patch.items() if key != "stdout"},
        },
    }


def main(argv):
    parser = argparse.ArgumentParser()
    parser.add_argument("--mirror", default="mirror.git")
    parser.add_argument("--work-dir", default="work")
    parser.add_argument("--read-files", type=int, required=True)
    parser.add_argument("--read-bytes", type=int, required=True)
    parser.add_argument("--edit-files", type=int, required=True)
    parser.add_argument("--search-token", default="AGENTFS_TOKEN")
    parser.add_argument("--skip-fsck", action="store_true")
    args = parser.parse_args(argv)

    root = Path.cwd()
    mirror = root / args.mirror
    workdir = root / args.work_dir
    phase_seconds = {}
    phase_runs = {}
    started_total = time.perf_counter()

    started = time.perf_counter()
    clone = run_git(["clone", "--local", "--no-hardlinks", str(mirror), str(workdir)], root)
    require_ok(clone, "clone")
    phase_seconds["clone"] = time.perf_counter() - started
    phase_runs["clone"] = {key: value for key, value in clone.items() if key != "stdout"}
    profile_checkpoint("clone")

    started = time.perf_counter()
    checkout = run_git(["checkout", "-B", "agentfs-benchmark"], workdir)
    require_ok(checkout, "checkout")
    head = run_git(["rev-parse", "HEAD"], workdir)
    require_ok(head, "rev-parse")
    phase_seconds["checkout"] = time.perf_counter() - started
    phase_runs["checkout"] = {key: value for key, value in checkout.items() if key != "stdout"}
    profile_checkpoint("checkout")

    started = time.perf_counter()
    status_initial = run_git(["status", "--short"], workdir)
    require_ok(status_initial, "status")
    branch_status = run_git(["status", "--short", "--branch"], workdir)
    require_ok(branch_status, "status --branch")
    phase_seconds["status"] = time.perf_counter() - started
    phase_runs["status"] = {
        "short": {key: value for key, value in status_initial.items() if key != "stdout"},
        "branch": {key: value for key, value in branch_status.items() if key != "stdout"},
    }

    profile_checkpoint("status")

    read_search = bounded_read_search(workdir, args.read_files, args.read_bytes, args.search_token)
    phase_seconds["read_search"] = read_search["duration_seconds"]
    profile_checkpoint("read_search")

    edits = edit_files(workdir, read_search["all_files"], args.edit_files)
    phase_seconds["edit"] = edits["duration_seconds"]
    profile_checkpoint("edit")

    diff = diff_summary(workdir)
    phase_seconds["diff"] = diff["duration_seconds"]
    profile_checkpoint("diff")

    fsck = {"ran": False, "ok": None, "run": None}
    if not args.skip_fsck:
        started = time.perf_counter()
        fsck_run = run_git(["fsck", "--strict"], workdir)
        phase_seconds["fsck"] = time.perf_counter() - started
        fsck = {
            "ran": True,
            "ok": fsck_run["returncode"] == 0,
            "run": {key: value for key, value in fsck_run.items() if key != "stdout"},
        }
        require_ok(fsck_run, "fsck")
        profile_checkpoint("fsck")
    else:
        phase_seconds["fsck"] = 0.0

    total_seconds = time.perf_counter() - started_total
    print(
        json.dumps(
            {
                "head_commit": head["stdout"].strip(),
                "phase_seconds": phase_seconds,
                "total_seconds": total_seconds,
                "phase_runs": phase_runs,
                "profile_checkpoints": PROFILE_CHECKPOINTS,
                "initial_status": status_initial["stdout"],
                "branch_status": branch_status["stdout"],
                "read_search": {
                    key: value
                    for key, value in read_search.items()
                    if key not in {"duration_seconds", "all_files"}
                },
                "edits": edits,
                "diff": diff,
                "fsck": fsck,
            },
            sort_keys=True,
        )
    )


try:
    main(sys.argv[1:])
except Exception as exc:
    print(json.dumps({"error": str(exc)}, sort_keys=True))
    raise
'''


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be >= 1")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be > 0")
    return parsed


def env_flag(name: str) -> bool:
    value = os.environ.get(name, "")
    return value.lower() in {"1", "true", "yes", "on"}


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Compare a deterministic Git-like mixed workflow on native storage "
            "against the same workflow through an AgentFS overlay."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples:
  # Fast deterministic smoke, no network
  scripts/validation/git-workload-benchmark.py --fixture-files 12 --edit-files 3 --timeout 60

  # Use a local source checkout/repository by first preparing a local bare mirror
  scripts/validation/git-workload-benchmark.py --source /path/to/repo --read-files 128

Environment:
  AGENTFS_BIN                  path/name of agentfs executable
  AGENTFS_PROFILE              set to 0 only when --no-profile is supplied
  GIT_WORKLOAD_BENCHMARK_KEEP_TEMP=1
                               keep temporary source/native/AgentFS trees
""",
    )
    source_group = parser.add_mutually_exclusive_group()
    source_group.add_argument(
        "--source",
        help="local Git repository or worktree used to prepare the bare mirror",
    )
    source_group.add_argument(
        "--remote",
        help="optional remote URL used to prepare the bare mirror (networked, not used by default)",
    )
    parser.add_argument("--fixture-files", type=positive_int, default=96)
    parser.add_argument("--fixture-dirs", type=positive_int, default=8)
    parser.add_argument("--fixture-file-size-bytes", type=positive_int, default=1024)
    parser.add_argument("--read-files", type=positive_int, default=64)
    parser.add_argument("--read-bytes", type=positive_int, default=2048)
    parser.add_argument("--edit-files", type=positive_int, default=8)
    parser.add_argument("--search-token", default="AGENTFS_TOKEN")
    parser.add_argument(
        "--agentfs-bin",
        default=os.environ.get("AGENTFS_BIN"),
        help="agentfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=positive_float(os.environ.get("GIT_WORKLOAD_BENCHMARK_TIMEOUT", "180")),
        help="per-command timeout in seconds (default: 180)",
    )
    parser.add_argument("--session", default=None, help="AgentFS session id (default: generated)")
    profile_group = parser.add_mutually_exclusive_group()
    profile_group.add_argument(
        "--profile",
        dest="profile",
        action="store_true",
        help="enable AGENTFS_PROFILE=1 for AgentFS invocation (default)",
    )
    profile_group.add_argument(
        "--no-profile",
        dest="profile",
        action="store_false",
        help="disable AgentFS profile summaries",
    )
    parser.set_defaults(profile=True)
    parser.add_argument("--skip-fsck", action="store_true", help="skip git fsck --strict phase")
    parser.add_argument(
        "--require-performance",
        action="store_true",
        default=env_flag("GIT_WORKLOAD_REQUIRE_PERFORMANCE"),
        help="fail the benchmark when configured performance thresholds are missed",
    )
    parser.add_argument(
        "--keep-temp",
        action="store_true",
        default=env_flag("GIT_WORKLOAD_BENCHMARK_KEEP_TEMP"),
        help="keep temporary trees and isolated HOME after the run",
    )
    parser.add_argument("--output", help="write JSON result to this file instead of stdout")
    parser.add_argument("--json-indent", type=int, default=2)
    return parser.parse_args(argv)


def tail_text(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        text = value.decode("utf-8", errors="replace")
    else:
        text = str(value)
    if len(text) <= OUTPUT_TAIL_CHARS:
        return text
    return text[-OUTPUT_TAIL_CHARS:]


def extract_profile_summaries(stderr: Any) -> list[dict[str, Any]]:
    if stderr is None:
        return []
    if isinstance(stderr, bytes):
        text = stderr.decode("utf-8", errors="replace")
    else:
        text = str(stderr)

    summaries: list[dict[str, Any]] = []
    for line in text.splitlines():
        line = line.strip()
        if not line or "agentfs_profile_summary" not in line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and value.get("event") == "agentfs_profile_summary":
            summaries.append(value)
    return summaries


def profile_counter_summary(summaries: list[dict[str, Any]]) -> dict[str, Any]:
    by_source: dict[str, dict[str, Any]] = {}
    max_counters: dict[str, int] = {}
    for summary in summaries:
        counters = summary.get("counters")
        if not isinstance(counters, dict):
            continue
        source = str(summary.get("source", "unknown"))
        by_source[source] = counters
        for key, value in counters.items():
            if isinstance(value, int):
                max_counters[key] = max(max_counters.get(key, 0), value)
    return {"summary_count": len(summaries), "last_by_source": by_source, "max_counters": max_counters}


def per_phase_profile_counters(
    summaries: list[dict[str, Any]], phase_labels: list[str]
) -> dict[str, Any]:
    """Attribute counter deltas to workload phases from ordered checkpoints.

    Each `phase-checkpoint-<seq>` summary is cumulative; subtracting consecutive
    checkpoints (and the implicit all-zero start) yields the counters consumed by
    each phase. Checkpoints are ordered by their monotonic sequence number and
    zipped with the ordered phase labels emitted by the workload.
    """
    checkpoints: list[tuple[int, dict[str, Any]]] = []
    for summary in summaries:
        source = str(summary.get("source", ""))
        if not source.startswith("phase-checkpoint-"):
            continue
        try:
            seq = int(source.rsplit("-", 1)[1])
        except (ValueError, IndexError):
            continue
        counters = summary.get("counters")
        if isinstance(counters, dict):
            checkpoints.append((seq, counters))
    checkpoints.sort(key=lambda item: item[0])

    phases: list[dict[str, Any]] = []
    prev: dict[str, Any] = {}
    for index, (seq, counters) in enumerate(checkpoints):
        label = phase_labels[index] if index < len(phase_labels) else f"checkpoint-{seq}"
        delta = {
            key: value - int(prev.get(key, 0))
            for key, value in counters.items()
            if isinstance(value, int)
        }
        phases.append({"phase": label, "seq": seq, "counters": delta})
        prev = counters

    aligned = len(checkpoints) == len(phase_labels)
    return {
        "checkpoint_count": len(checkpoints),
        "label_count": len(phase_labels),
        "labels_aligned": aligned,
        "phases": phases,
    }


def terminate_process_tree(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is not None:
        return
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    except Exception:
        proc.terminate()

    try:
        proc.wait(timeout=5)
        return
    except subprocess.TimeoutExpired:
        pass

    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        return
    except Exception:
        proc.kill()


def run_subprocess(argv: list[str], cwd: Path, env: dict[str, str], timeout: float) -> dict[str, Any]:
    started = time.perf_counter()
    proc = subprocess.Popen(
        argv,
        cwd=str(cwd),
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        stdout, stderr = proc.communicate(timeout=timeout)
        timed_out = False
    except subprocess.TimeoutExpired:
        terminate_process_tree(proc)
        try:
            stdout, stderr = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            if proc.stdout is not None:
                proc.stdout.close()
            if proc.stderr is not None:
                proc.stderr.close()
            stdout, stderr = "", "process timed out; output pipes were closed after termination"
        timed_out = True

    return {
        "argv": argv,
        "cwd": str(cwd),
        "duration_seconds": time.perf_counter() - started,
        "returncode": proc.returncode,
        "timed_out": timed_out,
        "stdout_tail": tail_text(stdout),
        "stderr_tail": tail_text(stderr),
        "stdout_bytes": len((stdout or "").encode("utf-8", errors="replace")),
        "stderr_bytes": len((stderr or "").encode("utf-8", errors="replace")),
        "profile_summaries": extract_profile_summaries(stderr),
    }


def parse_json_stdout(run: dict[str, Any]) -> Optional[dict[str, Any]]:
    text = str(run.get("stdout_tail", "")).strip()
    if text:
        try:
            value = json.loads(text)
            if isinstance(value, dict):
                return value
        except json.JSONDecodeError:
            pass
    for line in reversed(text.splitlines()):
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            return value
    return None


def resolve_agentfs_bin(agentfs_bin: Optional[str], repo_root: Path) -> str:
    if agentfs_bin:
        candidate_path = Path(agentfs_bin).expanduser()
        if candidate_path.is_file() and os.access(candidate_path, os.X_OK):
            return str(candidate_path.resolve())
        if os.sep not in agentfs_bin:
            found = shutil.which(agentfs_bin)
            if found:
                return found
        raise RuntimeError(f"configured agentfs executable not found or not executable: {agentfs_bin}")

    # Prefer release over debug: release binaries are what benchmarks should be
    # measuring (debug is unoptimized and can be 10x slower), AND release tends
    # to be rebuilt more often than debug during active development, so we are
    # more likely to pick up recent source changes. Debug-first ordering bit us
    # in Tier One (see RCA in the notes file): a stale debug binary missing the
    # `fuse-modern` feature kept returning ENOSYS while the just-built release
    # binary worked fine.
    for candidate_path in (
        repo_root / "cli" / "target" / "release" / "agentfs",
        repo_root / "cli" / "target" / "debug" / "agentfs",
    ):
        if candidate_path.is_file() and os.access(candidate_path, os.X_OK):
            return str(candidate_path)

    build = subprocess.run(
        ["cargo", "build", "--manifest-path", str(repo_root / "cli" / "Cargo.toml")],
        cwd=str(repo_root / "cli"),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if build.returncode != 0:
        raise RuntimeError(
            "failed to build repo-local agentfs binary; set AGENTFS_BIN to an explicit binary\n"
            f"stdout:\n{tail_text(build.stdout)}\n"
            f"stderr:\n{tail_text(build.stderr)}"
        )

    built = repo_root / "cli" / "target" / "debug" / "agentfs"
    if built.is_file() and os.access(built, os.X_OK):
        return str(built)
    raise RuntimeError(f"repo-local build completed but binary was not found: {built}")


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


def git_env() -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("GIT_CONFIG_NOSYSTEM", "1")
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    env.setdefault("NO_COLOR", "1")
    env.setdefault("LC_ALL", "C")
    env["GIT_AUTHOR_NAME"] = "AgentFS Benchmark"
    env["GIT_AUTHOR_EMAIL"] = "agentfs-benchmark@example.invalid"
    env["GIT_COMMITTER_NAME"] = "AgentFS Benchmark"
    env["GIT_COMMITTER_EMAIL"] = "agentfs-benchmark@example.invalid"
    return env


def run_git(argv: list[str], cwd: Path, *, env: Optional[dict[str, str]] = None, timeout: float = 60) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git"] + argv,
        cwd=str(cwd),
        env=env or git_env(),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
    )


def require_git() -> None:
    if shutil.which("git") is None:
        raise RuntimeError("git executable is required")


def require_git_ok(proc: subprocess.CompletedProcess[str], action: str) -> None:
    if proc.returncode != 0:
        raise RuntimeError(
            f"{action} failed with exit {proc.returncode}\n"
            f"stdout:\n{tail_text(proc.stdout)}\n"
            f"stderr:\n{tail_text(proc.stderr)}"
        )


def create_generated_repo(root: Path, file_count: int, dir_count: int, file_size: int) -> None:
    root.mkdir(parents=True, exist_ok=True)
    env = git_env()
    env["GIT_AUTHOR_DATE"] = "2024-01-01T00:00:00Z"
    env["GIT_COMMITTER_DATE"] = "2024-01-01T00:00:00Z"
    init = run_git(["init"], root, env=env)
    require_git_ok(init, "git init generated repo")
    require_git_ok(run_git(["checkout", "-B", "main"], root, env=env), "git checkout main")
    require_git_ok(run_git(["config", "user.name", "AgentFS Benchmark"], root, env=env), "git config user.name")
    require_git_ok(
        run_git(["config", "user.email", "agentfs-benchmark@example.invalid"], root, env=env),
        "git config user.email",
    )

    categories = ("src", "tests", "docs", "data")
    for index in range(file_count):
        category = categories[index % len(categories)]
        directory = root / category / f"pkg{index % dir_count:03d}"
        directory.mkdir(parents=True, exist_ok=True)
        if category == "src":
            filename = f"module_{index:05d}.py"
            header = f"# Generated source {index}\nTOKEN = 'AGENTFS_TOKEN_{index % 11}'\n"
        elif category == "tests":
            filename = f"test_{index:05d}.py"
            header = f"# Generated test {index}\ndef test_{index:05d}():\n    assert 'AGENTFS_TOKEN'\n"
        elif category == "docs":
            filename = f"note_{index:05d}.md"
            header = f"# Generated note {index}\n\nAGENTFS_TOKEN documentation fixture.\n"
        else:
            filename = f"blob_{index:05d}.txt"
            header = f"data fixture {index} AGENTFS_TOKEN\n"
        seed = hashlib.sha256(f"agentfs-git-fixture-{index}".encode("utf-8")).hexdigest()
        filler = "".join(f"{line:04d} {seed} AGENTFS_TOKEN_{line % 7}\n" for line in range(128))
        content = (header + filler)[:file_size]
        if not content.endswith("\n"):
            content += "\n"
        (directory / filename).write_text(content, encoding="utf-8")

    (root / ".gitignore").write_text("__pycache__/\n*.pyc\n", encoding="utf-8")
    require_git_ok(run_git(["add", "."], root, env=env), "git add generated repo")
    require_git_ok(run_git(["commit", "-m", "initial deterministic fixture"], root, env=env), "git commit initial")

    env["GIT_AUTHOR_DATE"] = "2024-01-01T00:01:00Z"
    env["GIT_COMMITTER_DATE"] = "2024-01-01T00:01:00Z"
    touched = sorted((root / "src").rglob("*.py"))[: max(1, min(4, file_count))]
    for index, path in enumerate(touched):
        with path.open("a", encoding="utf-8") as handle:
            handle.write(f"\n# second commit marker {index} AGENTFS_TOKEN\n")
    require_git_ok(run_git(["add", "."], root, env=env), "git add second commit")
    require_git_ok(run_git(["commit", "-m", "update source markers"], root, env=env), "git commit second")
    require_git_ok(run_git(["tag", "agentfs-benchmark-fixture"], root, env=env), "git tag fixture")


def prepare_bare_mirror(args: argparse.Namespace, temp_root: Path) -> tuple[Path, dict[str, Any]]:
    prepared = temp_root / "prepared"
    prepared.mkdir(parents=True, exist_ok=True)
    mirror = prepared / "mirror.git"
    if args.remote:
        clone = run_git(["clone", "--mirror", args.remote, str(mirror)], prepared, timeout=args.timeout)
        require_git_ok(clone, "git clone --mirror remote")
        kind = "remote"
        source_path = args.remote
    elif args.source:
        source = Path(args.source).expanduser().resolve()
        if not source.exists():
            raise RuntimeError(f"--source does not exist: {source}")
        clone = run_git(["clone", "--mirror", str(source), str(mirror)], prepared, timeout=args.timeout)
        require_git_ok(clone, "git clone --mirror source")
        kind = "source"
        source_path = str(source)
    else:
        generated = prepared / "generated-source"
        create_generated_repo(generated, args.fixture_files, args.fixture_dirs, args.fixture_file_size_bytes)
        clone = run_git(["clone", "--mirror", str(generated), str(mirror)], prepared, timeout=args.timeout)
        require_git_ok(clone, "git clone --mirror generated fixture")
        kind = "generated"
        source_path = str(generated)

    head = run_git(["--git-dir", str(mirror), "rev-parse", "HEAD"], prepared, timeout=args.timeout)
    require_git_ok(head, "git rev-parse mirror HEAD")
    return mirror, {"kind": kind, "path": source_path, "mirror_head": head.stdout.strip()}


def copy_mirror(source: Path, destination_root: Path) -> None:
    destination_root.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source, destination_root / "mirror.git", symlinks=True)


def prepare_environment(temp_root: Path, profile: bool) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    env.setdefault("GIT_CONFIG_NOSYSTEM", "1")
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    if profile:
        env["AGENTFS_PROFILE"] = "1"
    else:
        env.pop("AGENTFS_PROFILE", None)

    home = temp_root / "home"
    for path in (home, home / ".config", home / ".cache", home / ".local" / "share"):
        path.mkdir(parents=True, exist_ok=True)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(home / ".config")
    env["XDG_CACHE_HOME"] = str(home / ".cache")
    env["XDG_DATA_HOME"] = str(home / ".local" / "share")

    temp_dir = temp_root / "tmp"
    temp_dir.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(temp_dir)
    env["TMP"] = str(temp_dir)
    env["TEMP"] = str(temp_dir)
    return env


def tree_hash(root: Path) -> dict[str, Any]:
    digest = hashlib.sha256()
    file_count = 0
    dir_count = 0
    symlink_count = 0
    total_bytes = 0
    for dirpath, dirnames, filenames in os.walk(root):
        for name in sorted(list(dirnames)):
            path = Path(dirpath) / name
            if not path.is_symlink():
                continue
            rel = path.relative_to(root).as_posix()
            stat = path.lstat()
            target = os.readlink(path)
            digest.update(b"symlink-dir\0")
            digest.update(rel.encode("utf-8"))
            digest.update(b"\0")
            digest.update(
                f"{stat.st_mode}:{stat.st_uid}:{stat.st_gid}:{stat.st_size}:{stat.st_mtime_ns}:{stat.st_ctime_ns}".encode(
                    "ascii"
                )
            )
            digest.update(b"\0")
            digest.update(target.encode("utf-8", errors="surrogateescape"))
            digest.update(b"\0")
            symlink_count += 1
            dirnames.remove(name)
        dirnames.sort()
        filenames.sort()
        rel_dir = Path(dirpath).relative_to(root).as_posix()
        digest.update(b"dir\0")
        digest.update(rel_dir.encode("utf-8"))
        digest.update(b"\0")
        stat = Path(dirpath).lstat()
        digest.update(
            f"{stat.st_mode}:{stat.st_uid}:{stat.st_gid}:{stat.st_size}:{stat.st_mtime_ns}:{stat.st_ctime_ns}".encode(
                "ascii"
            )
        )
        digest.update(b"\0")
        dir_count += 1
        for name in filenames:
            path = Path(dirpath) / name
            rel = path.relative_to(root).as_posix()
            stat = path.lstat()
            if path.is_symlink():
                target = os.readlink(path)
                digest.update(b"symlink\0")
                digest.update(rel.encode("utf-8"))
                digest.update(b"\0")
                digest.update(
                    f"{stat.st_mode}:{stat.st_uid}:{stat.st_gid}:{stat.st_size}:{stat.st_mtime_ns}:{stat.st_ctime_ns}".encode(
                        "ascii"
                    )
                )
                digest.update(b"\0")
                digest.update(target.encode("utf-8", errors="surrogateescape"))
                digest.update(b"\0")
                symlink_count += 1
                continue
            digest.update(b"file\0")
            digest.update(rel.encode("utf-8"))
            digest.update(b"\0")
            size = stat.st_size
            digest.update(
                f"{stat.st_mode}:{stat.st_uid}:{stat.st_gid}:{stat.st_mtime_ns}:{stat.st_ctime_ns}".encode(
                    "ascii"
                )
            )
            digest.update(b"\0")
            digest.update(str(size).encode("ascii"))
            digest.update(b"\0")
            total_bytes += size
            file_count += 1
            with path.open("rb") as handle:
                while True:
                    block = handle.read(HASH_BLOCK_BYTES)
                    if not block:
                        break
                    digest.update(block)
    return {
        "sha256": digest.hexdigest(),
        "files": file_count,
        "directories": dir_count,
        "symlinks": symlink_count,
        "bytes": total_bytes,
    }


def db_artifacts(db_path: Path) -> dict[str, Any]:
    wal = db_path.with_name(db_path.name + "-wal")
    if wal.exists() and wal.stat().st_size == 0:
        wal.unlink()
    shm = db_path.with_name(db_path.name + "-shm")
    if shm.exists():
        shm.unlink()

    artifacts = []
    total = 0
    for path in (db_path, db_path.with_name(db_path.name + "-wal"), db_path.with_name(db_path.name + "-shm")):
        if path.exists():
            size = path.stat().st_size
            artifacts.append({"path": str(path), "bytes": size})
            total += size
    return {"path": str(db_path), "total_bytes": total, "artifacts": artifacts}


def artifacts_have_nonempty_sidecars(artifacts: dict[str, Any]) -> bool:
    return any(
        str(item.get("path", "")).endswith(("-wal", "-shm")) and int(item.get("bytes", 0) or 0) > 0
        for item in artifacts.get("artifacts", [])
    )


def artifacts_have_sidecars(artifacts: dict[str, Any]) -> bool:
    return any(str(item.get("path", "")).endswith(("-wal", "-shm")) for item in artifacts.get("artifacts", []))


def table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
        (name,),
    ).fetchone()
    return row is not None


def optional_count(conn: sqlite3.Connection, table_name: str) -> Optional[int]:
    if not table_exists(conn, table_name):
        return None
    row = conn.execute(f"SELECT COUNT(*) FROM {table_name}").fetchone()
    return int(row[0])


def inspect_db(db_path: Path) -> dict[str, Any]:
    if not db_path.exists():
        return {"inspectable": False, "reason": "database file does not exist"}
    try:
        conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        conn.execute("PRAGMA query_only = ON")
        try:
            result: dict[str, Any] = {"inspectable": True}
            if table_exists(conn, "fs_data"):
                row = conn.execute(
                    "SELECT COUNT(*), COALESCE(SUM(LENGTH(data)), 0) FROM fs_data"
                ).fetchone()
                result["fs_data_rows"] = int(row[0])
                result["fs_data_bytes"] = int(row[1])
            if table_exists(conn, "fs_inode"):
                row = conn.execute(
                    "SELECT COUNT(*), "
                    "COALESCE(SUM(CASE WHEN storage_kind = 1 THEN 1 ELSE 0 END), 0), "
                    "COALESCE(SUM(CASE WHEN storage_kind = 1 THEN LENGTH(data_inline) ELSE 0 END), 0) "
                    "FROM fs_inode"
                ).fetchone()
                result["fs_inode_rows"] = int(row[0])
                result["inline_inode_rows"] = int(row[1])
                result["fs_inline_bytes"] = int(row[2])
            for table in ("fs_origin", "fs_partial_origin", "fs_chunk_override", "fs_whiteout"):
                count = optional_count(conn, table)
                if count is not None:
                    result[f"{table}_rows"] = count
            if table_exists(conn, "fs_config"):
                result["fs_config"] = {
                    str(key): str(value)
                    for key, value in conn.execute("SELECT key, value FROM fs_config").fetchall()
                }
            partial_rows = int(result.get("fs_partial_origin_rows", 0) or 0)
            result["portability_status"] = {
                "portable": partial_rows == 0,
                "origin_backed": partial_rows > 0,
                "partial_origin_rows": partial_rows,
                "stored_bytes": int(result.get("fs_data_bytes", 0) or 0)
                + int(result.get("fs_inline_bytes", 0) or 0),
            }
            return result
        finally:
            conn.close()
    except Exception as exc:
        return {"inspectable": False, "reason": str(exc)}


def workload_argv(args: argparse.Namespace) -> list[str]:
    argv = [
        sys.executable,
        "-c",
        GIT_WORKLOAD,
        "--read-files",
        str(args.read_files),
        "--read-bytes",
        str(args.read_bytes),
        "--edit-files",
        str(args.edit_files),
        "--search-token",
        args.search_token,
    ]
    if args.skip_fsck:
        argv.append("--skip-fsck")
    return argv


def phase_ratios(native_workload: Optional[dict[str, Any]], agentfs_workload: Optional[dict[str, Any]]) -> dict[str, Any]:
    native_phases = native_workload.get("phase_seconds", {}) if isinstance(native_workload, dict) else {}
    agentfs_phases = agentfs_workload.get("phase_seconds", {}) if isinstance(agentfs_workload, dict) else {}
    names = sorted(set(native_phases) | set(agentfs_phases))
    ratios = {}
    for name in names:
        native_value = native_phases.get(name)
        agentfs_value = agentfs_phases.get(name)
        ratios[name] = {
            "native_seconds": native_value,
            "agentfs_seconds": agentfs_value,
            "ratio": (agentfs_value / native_value) if isinstance(native_value, (int, float)) and native_value > 0 and isinstance(agentfs_value, (int, float)) else None,
        }
    return ratios


def comparable_workload(workload: Optional[dict[str, Any]]) -> Optional[dict[str, Any]]:
    if not isinstance(workload, dict) or "error" in workload:
        return None
    return {
        "head_commit": workload.get("head_commit"),
        "initial_status": workload.get("initial_status"),
        "read_search": {
            "digest": workload.get("read_search", {}).get("digest"),
            "files_total": workload.get("read_search", {}).get("files_total"),
            "files_scanned": workload.get("read_search", {}).get("files_scanned"),
            "bytes_read": workload.get("read_search", {}).get("bytes_read"),
            "matches": workload.get("read_search", {}).get("matches"),
            "selected_files": workload.get("read_search", {}).get("selected_files"),
        },
        "edits": {
            "changed_files": workload.get("edits", {}).get("changed_files"),
            "edits": workload.get("edits", {}).get("edits"),
        },
        "diff": {
            "changed_files": workload.get("diff", {}).get("changed_files"),
            "changed_file_count": workload.get("diff", {}).get("changed_file_count"),
            "patch_sha256": workload.get("diff", {}).get("patch_sha256"),
            "patch_bytes": workload.get("diff", {}).get("patch_bytes"),
        },
        "fsck": {
            "ran": workload.get("fsck", {}).get("ran"),
            "ok": workload.get("fsck", {}).get("ok"),
        },
    }


def equivalence(native_workload: Optional[dict[str, Any]], agentfs_workload: Optional[dict[str, Any]]) -> dict[str, Any]:
    native_compare = comparable_workload(native_workload)
    agentfs_compare = comparable_workload(agentfs_workload)
    if native_compare is None or agentfs_compare is None:
        return {
            "checked": False,
            "equivalent": False,
            "reason": "missing comparable workload JSON",
            "native": native_compare,
            "agentfs": agentfs_compare,
        }
    return {
        "checked": True,
        "equivalent": native_compare == agentfs_compare,
        "native": native_compare,
        "agentfs": agentfs_compare,
    }


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        temp_root = Path(tempfile.mkdtemp(prefix="agentfs-git-workload-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(prefix="agentfs-git-workload-")
        temp_root = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    try:
        require_git()
        agentfs_bin = resolve_agentfs_bin(args.agentfs_bin, repo_root)
        env = prepare_environment(temp_root, args.profile)
        session = args.session or f"git-workload-{uuid.uuid4().hex}"
        db_path = Path(env["HOME"]) / ".agentfs" / "run" / session / "delta.db"
        mirror, source_info = prepare_bare_mirror(args, temp_root)

        native_root = temp_root / "native"
        agentfs_base_root = temp_root / "agentfs-base"
        copy_mirror(mirror, native_root)
        copy_mirror(mirror, agentfs_base_root)

        base_before = tree_hash(agentfs_base_root)
        base_workload = workload_argv(args)
        native_run = run_subprocess(base_workload, native_root, env, args.timeout)
        agentfs_command = [
            agentfs_bin,
            "run",
            "--session",
            session,
            "--no-default-allows",
            "--",
        ] + base_workload
        agentfs_run = run_subprocess(agentfs_command, agentfs_base_root, env, args.timeout)
        base_after = tree_hash(agentfs_base_root)
        db_after = db_artifacts(db_path)
        inspect_after = inspect_db(db_path)
        integrity_run = run_subprocess(
            [agentfs_bin, "integrity", str(db_path), "--json", "--require-portable"],
            temp_root,
            env,
            args.timeout,
        )
        integrity_payload = parse_json_stdout(integrity_run)
        backup_path = temp_root / "git-workload-backup.db"
        backup_run = run_subprocess(
            [agentfs_bin, "backup", str(db_path), str(backup_path), "--verify"],
            temp_root,
            env,
            args.timeout,
        )
        backup_artifacts = db_artifacts(backup_path)
        backup_inspect = inspect_db(backup_path)

        native_workload = parse_json_stdout(native_run)
        agentfs_workload = parse_json_stdout(agentfs_run)
        equivalent = equivalence(native_workload, agentfs_workload)
        profile_summaries = agentfs_run.get("profile_summaries", [])
        profile_counters = profile_counter_summary(profile_summaries)
        phase_labels = (
            agentfs_workload.get("profile_checkpoints", [])
            if isinstance(agentfs_workload, dict)
            else []
        )
        per_phase_counters = per_phase_profile_counters(profile_summaries, phase_labels)
        ratios = phase_ratios(native_workload, agentfs_workload)
        native_total = native_workload.get("total_seconds") if isinstance(native_workload, dict) else None
        agentfs_total = agentfs_workload.get("total_seconds") if isinstance(agentfs_workload, dict) else None
        overall_ratio = (
            agentfs_total / native_total
            if isinstance(native_total, (int, float))
            and native_total > 0
            and isinstance(agentfs_total, (int, float))
            else None
        )
        base_unchanged = base_before["sha256"] == base_after["sha256"]
        portable = inspect_after.get("portability_status", {}).get("portable")
        inspectable = inspect_after.get("inspectable") is True
        no_sidecars = not artifacts_have_sidecars(db_after)
        portability_ok = inspectable and portable is True
        integrity_ok = (
            integrity_run["returncode"] == 0
            and isinstance(integrity_payload, dict)
            and integrity_payload.get("ok") is True
        )
        backup_portability = backup_inspect.get("portability_status", {})
        backup_ok = (
            backup_run["returncode"] == 0
            and backup_inspect.get("inspectable") is True
            and backup_portability.get("portable") is True
            and int(backup_portability.get("partial_origin_rows", 1) or 0) == 0
            and not artifacts_have_sidecars(backup_artifacts)
        )
        threshold_failures = [
            {"phase": phase, **values}
            for phase, values in ratios.items()
            if (
                (phase in {"clone", "checkout"} and isinstance(values.get("ratio"), (int, float)) and values["ratio"] > 3.0)
                or (
                    phase in {"status", "read_search", "edit", "diff"}
                    and isinstance(values.get("ratio"), (int, float))
                    and values["ratio"] > 2.0
                )
            )
        ]
        performance_passed = not threshold_failures

        correctness = {
            "native_returncode_zero": native_run["returncode"] == 0,
            "agentfs_returncode_zero": agentfs_run["returncode"] == 0,
            "equivalence": equivalent,
            "agentfs_base_unchanged": base_unchanged,
            "agentfs_db_inspectable": inspectable,
            "agentfs_portable": portable is True,
            "agentfs_no_nonempty_sidecars": no_sidecars,
            "agentfs_integrity_require_portable": integrity_ok,
            "agentfs_backup_verify": backup_ok,
            "performance_passed": performance_passed,
            "passed": (
                native_run["returncode"] == 0
                and agentfs_run["returncode"] == 0
                and equivalent.get("equivalent") is True
                and base_unchanged
                and portability_ok
                and no_sidecars
                and integrity_ok
                and backup_ok
                and (not args.require_performance or performance_passed)
            ),
        }
        if not correctness["passed"]:
            exit_code = 1

        result = {
            "schema_version": 1,
            "benchmark": "phase7-git-workload",
            "git_commit": git_commit(repo_root),
            "command": {
                "argv": [str(Path(__file__).resolve())] + argv,
                "workload_argv": base_workload,
                "agentfs_prefix": [
                    agentfs_bin,
                    "run",
                    "--session",
                    session,
                    "--no-default-allows",
                    "--",
                ],
            },
            "environment": {
                "AGENTFS_PROFILE": env.get("AGENTFS_PROFILE"),
                "AGENTFS_BIN": args.agentfs_bin,
            },
            "parameters": {
                "fixture_files": args.fixture_files,
                "fixture_dirs": args.fixture_dirs,
                "fixture_file_size_bytes": args.fixture_file_size_bytes,
                "read_files": args.read_files,
                "read_bytes": args.read_bytes,
                "edit_files": args.edit_files,
                "search_token": args.search_token,
                "skip_fsck": args.skip_fsck,
                "timeout_seconds": args.timeout,
            },
            "source": source_info,
            "agentfs": {
                "bin": agentfs_bin,
                "session": session,
                "db_path": str(db_path),
                "profile_enabled": args.profile,
                "profile_summary_count": profile_counters["summary_count"],
                "profile_counters": profile_counters,
                "per_phase_counters": per_phase_counters,
            },
            "summary": {
                "native_seconds": native_total,
                "agentfs_seconds": agentfs_total,
                "ratio": overall_ratio,
                "phase_ratios": ratios,
                "threshold_failures": threshold_failures,
                "performance_passed": performance_passed,
                "all_equivalent": equivalent.get("equivalent") is True,
                "agentfs_base_unchanged": base_unchanged,
                "passed": correctness["passed"],
                "correctness_passed": correctness["passed"],
            },
            "native": {
                "run": native_run,
                "workload": native_workload,
            },
            "agentfs_overlay": {
                "run": agentfs_run,
                "workload": agentfs_workload,
                "profile_summaries": profile_summaries,
            },
            "base_tree": {
                "before": base_before,
                "after": base_after,
                "unchanged": base_unchanged,
            },
            "database": {
                "after": db_after,
                "inspect_after": inspect_after,
                "nonempty_sidecars": not no_sidecars,
                "integrity": {
                    "run": integrity_run,
                    "result": integrity_payload,
                },
                "backup": {
                    "path": str(backup_path),
                    "run": backup_run,
                    "inspect": backup_inspect,
                    "artifacts": backup_artifacts,
                },
            },
            "correctness": correctness,
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
        }
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "phase7-git-workload",
            "error": str(exc),
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
        }

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).expanduser().write_text(payload, encoding="utf-8")
        print(f"Wrote Git workload benchmark JSON to {args.output}", file=sys.stderr)
    else:
        sys.stdout.write(payload)

    if temp_manager is not None:
        temp_manager.cleanup()

    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
