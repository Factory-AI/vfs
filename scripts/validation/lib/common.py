"""Process, environment, and binary-resolution helpers shared by the
validation harnesses.

Import via a sys.path bootstrap so the dash-named top-level scripts can use
the package from any CWD:

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from lib.common import resolve_vfs_bin, run_subprocess
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from collections.abc import MutableMapping
from pathlib import Path
from typing import Any, Optional

OUTPUT_TAIL_CHARS = 8000
HASH_BLOCK_BYTES = 1024 * 1024


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


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return parsed


def percentile(values: list[float], quantile: float) -> float:
    """Linearly interpolated percentile for a non-empty numeric sample."""
    if not values:
        raise ValueError("percentile requires at least one value")
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * quantile
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    fraction = position - lower
    return ordered[lower] * (1.0 - fraction) + ordered[upper] * fraction


def summarize_floats(values: list[float]) -> dict[str, float | int]:
    """Absolute distribution summary used by local measurement harnesses."""
    cleaned = [float(value) for value in values if isinstance(value, (int, float))]
    if not cleaned:
        return {"n": 0}
    return {
        "n": len(cleaned),
        "median": statistics.median(cleaned),
        "p25": percentile(cleaned, 0.25),
        "p75": percentile(cleaned, 0.75),
        "min": min(cleaned),
        "max": max(cleaned),
        "stdev": statistics.stdev(cleaned) if len(cleaned) > 1 else 0.0,
    }


def tree_fingerprint(root: Path) -> dict[str, Any]:
    """Hash recursive content plus stable metadata without reading atime.

    Sandboxed-write validators compare this value before and after a run.
    Symlinked directories are recorded as links instead of traversed so a
    fixture cannot make the invariant check escape its declared base tree.
    """
    digest = hashlib.sha256()
    files = 0
    directories = 0
    symlinks = 0
    total_bytes = 0
    for dirpath, dirnames, filenames in os.walk(root):
        for name in sorted(list(dirnames)):
            path = Path(dirpath) / name
            if not path.is_symlink():
                continue
            rel = path.relative_to(root).as_posix()
            stat = path.lstat()
            digest.update(b"symlink-dir\0")
            digest.update(rel.encode("utf-8", errors="surrogateescape"))
            digest.update(b"\0")
            digest.update(
                f"{stat.st_mode}:{stat.st_uid}:{stat.st_gid}:{stat.st_size}:{stat.st_mtime_ns}".encode(
                    "ascii"
                )
            )
            digest.update(b"\0")
            digest.update(os.readlink(path).encode("utf-8", errors="surrogateescape"))
            digest.update(b"\0")
            symlinks += 1
            dirnames.remove(name)
        dirnames.sort()
        filenames.sort()
        directory = Path(dirpath)
        rel_dir = directory.relative_to(root).as_posix()
        stat = directory.lstat()
        digest.update(b"dir\0")
        digest.update(rel_dir.encode("utf-8", errors="surrogateescape"))
        digest.update(b"\0")
        digest.update(
            f"{stat.st_mode}:{stat.st_uid}:{stat.st_gid}:{stat.st_size}:{stat.st_mtime_ns}".encode(
                "ascii"
            )
        )
        digest.update(b"\0")
        directories += 1
        for name in filenames:
            path = directory / name
            rel = path.relative_to(root).as_posix()
            stat = path.lstat()
            if path.is_symlink():
                digest.update(b"symlink\0")
                digest.update(rel.encode("utf-8", errors="surrogateescape"))
                digest.update(b"\0")
                digest.update(
                    f"{stat.st_mode}:{stat.st_uid}:{stat.st_gid}:{stat.st_size}:{stat.st_mtime_ns}".encode(
                        "ascii"
                    )
                )
                digest.update(b"\0")
                digest.update(
                    os.readlink(path).encode("utf-8", errors="surrogateescape")
                )
                digest.update(b"\0")
                symlinks += 1
                continue
            digest.update(b"file\0")
            digest.update(rel.encode("utf-8", errors="surrogateescape"))
            digest.update(b"\0")
            digest.update(
                f"{stat.st_mode}:{stat.st_uid}:{stat.st_gid}:{stat.st_size}:{stat.st_mtime_ns}".encode(
                    "ascii"
                )
            )
            digest.update(b"\0")
            files += 1
            total_bytes += stat.st_size
            with path.open("rb") as handle:
                while chunk := handle.read(HASH_BLOCK_BYTES):
                    digest.update(chunk)
    return {
        "sha256": digest.hexdigest(),
        "files": files,
        "directories": directories,
        "symlinks": symlinks,
        "bytes": total_bytes,
    }


def env_flag(name: str) -> bool:
    value = os.environ.get(name, "")
    return value.lower() in {"1", "true", "yes", "on"}


def pin_distro_git(
    env: MutableMapping[str, str], scratch_dir: Path, home: Optional[Path] = None
) -> Path:
    """Pin `git` to the distro binary for every subprocess of a harness.

    The user PATH may route `git` through a hook-manager shim that daemonizes
    out of harness temp repos (library/environment.md). Prepend a shim dir
    whose `git` symlinks the distro binary, and point the global git config at
    a hookless file so no hook manager can re-enter via config.

    The PATH shim only holds for host-side processes: inside an `vfs run`
    sandbox, temp dirs and home files are hidden, so any shim dir silently
    falls out of PATH and the hook-manager git takes over. That is what
    ``env["GIT"]`` is for — it names the distro binary by absolute (system,
    sandbox-visible) path, and every harness git invocation must honor it,
    including the workload scripts spawned inside `vfs run`. Pass ``home``
    to also write a hookless ``~/.gitconfig`` into a temp HOME (belt and
    braces for host-side legs).
    """
    shim_dir = scratch_dir / "git-shim"
    shim_dir.mkdir(parents=True, exist_ok=True)
    real_git = next(
        (
            candidate
            for candidate in ("/usr/bin/git", "/bin/git")
            if os.access(candidate, os.X_OK)
        ),
        None,
    ) or shutil.which("git")
    if real_git is None:
        raise RuntimeError("git executable is required")
    shim = shim_dir / "git"
    shim.unlink(missing_ok=True)
    shim.symlink_to(real_git)
    hookless = f"[core]\n\thooksPath = {shim_dir / 'hooks-none'}\n"
    gitconfig = shim_dir / "gitconfig"
    gitconfig.write_text(hookless, encoding="utf-8")
    if home is not None:
        (home / ".gitconfig").write_text(hookless, encoding="utf-8")
    env["PATH"] = f"{shim_dir}{os.pathsep}{env.get('PATH', os.defpath)}"
    env["GIT_CONFIG_GLOBAL"] = str(gitconfig)
    env["GIT"] = str(real_git)
    return shim


def isolate_benchmark_env(
    base_env: MutableMapping[str, str], context_root: Path
) -> dict[str, str]:
    """Give one benchmark leg independent process and filesystem caches.

    HOME carries the Vfs session store, Git configuration, and user-level
    caches. Sharing it across samples turns later measurements into tests of
    state left by earlier legs. TMPDIR must also stay outside any base tree
    whose immutability the harness verifies.
    """
    env = dict(base_env)
    home = context_root / "home"
    for path in (
        home,
        home / ".config",
        home / ".cache",
        home / ".local" / "share",
    ):
        path.mkdir(parents=True, exist_ok=True)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(home / ".config")
    env["XDG_CACHE_HOME"] = str(home / ".cache")
    env["XDG_DATA_HOME"] = str(home / ".local" / "share")
    pin_distro_git(env, context_root, home=home)
    tmp = context_root / "tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(tmp)
    env["TMP"] = str(tmp)
    env["TEMP"] = str(tmp)
    return env


def mounts_under(root: Path) -> list[str]:
    """Return live Linux mountpoints at or below a benchmark context."""
    prefix = str(root.resolve()).rstrip(os.sep) + os.sep
    mounts: list[str] = []
    try:
        lines = (
            Path("/proc/self/mountinfo")
            .read_text(encoding="utf-8", errors="replace")
            .splitlines()
        )
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


def processes_for_benchmark_context(
    session: str, context_root: Path
) -> list[dict[str, Any]]:
    """Return processes retaining a session id or isolated context path."""
    root_bytes = str(context_root).encode()
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


def wait_for_benchmark_cleanup(
    session: str, context_root: Path, timeout_seconds: float = 2.0
) -> dict[str, Any]:
    """Prove a benchmark context is idle, then recover leaked local state."""
    started = time.monotonic()
    processes: list[dict[str, Any]] = []
    mounts: list[str] = []
    while True:
        processes = processes_for_benchmark_context(session, context_root)
        mounts = mounts_under(context_root)
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
        for process in processes_for_benchmark_context(session, context_root):
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
            processes = processes_for_benchmark_context(session, context_root)
            mounts = mounts_under(context_root)
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


def sandbox_python() -> str:
    """Interpreter path that stays visible inside an `vfs run` sandbox.

    Same hazard `pin_distro_git` documents, for the interpreter: `vfs run`
    hides home and temp dirs, so a `sys.executable` under `~/.local` (pyenv,
    uv, a user-installed CPython) simply does not exist inside the sandbox and
    the workload dies with exit 127 before it can run. Home-relative
    interpreters only survive because `~/.local` is a default allow, so any leg
    passing `--no-default-allows` breaks on exactly the machines whose python
    is not the distro one.

    Prefer a system interpreter on a read-only system path, which the sandbox
    always exposes; fall back to ``sys.executable`` when there is none (the
    caller is then no worse off than before).
    """
    for candidate in ("/usr/bin/python3", "/bin/python3"):
        if os.access(candidate, os.X_OK):
            return candidate
    return sys.executable


def git_ai_processes() -> dict[int, dict[str, Any]]:
    """Live git-ai hook-manager processes, keyed by pid.

    Census guard for the pinned-git mechanism: an unpinned `git` daemonizes
    `git-ai bg run` workers out of harness temp repos, and those outlive the
    run. Take a snapshot before and after a harness run and diff them with
    :func:`git_ai_leaks`.
    """
    procs: dict[int, dict[str, Any]] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            raw = (entry / "cmdline").read_bytes()
        except OSError:
            continue
        argv = [chunk.decode("utf-8", "replace") for chunk in raw.split(b"\0") if chunk]
        if not (argv and any(Path(token).name == "git-ai" for token in argv[:2])):
            continue
        info: dict[str, Any] = {
            "cmdline": " ".join(argv),
            "home": None,
            "vfs_session": False,
        }
        try:
            environ = (entry / "environ").read_bytes()
        except OSError:
            environ = b""
        for chunk in environ.split(b"\0"):
            if chunk.startswith(b"HOME="):
                info["home"] = chunk[len(b"HOME=") :].decode("utf-8", "replace")
            elif chunk.startswith(b"VFS_SESSION="):
                info["vfs_session"] = True
        procs[int(entry.name)] = info
    return procs


def git_ai_leaks(
    before: dict[int, dict[str, Any]], after: dict[int, dict[str, Any]]
) -> list[dict[str, Any]]:
    """New git-ai processes attributable to the harness run.

    The user's own hook manager churns independently (its daemon respawns with
    HOME under the real home dir), so a bare pid diff false-positives. Count
    only new processes with a temp-dir HOME or an VFS session in their
    environment — the same discrimination rule library/environment.md requires
    before killing one.
    """
    tmp_prefix = tempfile.gettempdir().rstrip(os.sep) + os.sep
    leaks: list[dict[str, Any]] = []
    for pid, info in sorted(after.items()):
        if pid in before:
            continue
        home = info.get("home") or ""
        if info.get("vfs_session") or home.startswith(tmp_prefix):
            leaks.append({"pid": pid, **info})
    return leaks


def tail_text(value: Any, limit: int = OUTPUT_TAIL_CHARS) -> str:
    text = (
        value.decode("utf-8", errors="replace")
        if isinstance(value, bytes)
        else str(value or "")
    )
    return text if len(text) <= limit else text[-limit:]


def extract_profile_summaries(stderr: Any) -> list[dict[str, Any]]:
    """Every `vfs_profile_summary` JSON object a run emitted on stderr.

    Scans the full stderr, not `tail_text` of it: a summary emitted before a
    chatty workload's output would otherwise fall off the front of the tail
    and silently vanish from the report.
    """
    if stderr is None:
        return []
    text = (
        stderr.decode("utf-8", errors="replace")
        if isinstance(stderr, bytes)
        else str(stderr)
    )
    summaries: list[dict[str, Any]] = []
    for line in text.splitlines():
        line = line.strip()
        if not line or "vfs_profile_summary" not in line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and value.get("event") == "vfs_profile_summary":
            summaries.append(value)
    return summaries


def summarize_profile_counters(summaries: list[dict[str, Any]]) -> dict[str, Any]:
    """Collapse one process's profile summaries into stable counter maxima."""
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


def run_subprocess(
    argv: list[str],
    cwd: Path,
    env: dict[str, str],
    timeout: float,
    tail_chars: int = OUTPUT_TAIL_CHARS,
    *,
    keep_stdout: bool = False,
    include_timing_origin: bool = False,
) -> dict[str, Any]:
    started_ns = time.perf_counter_ns()
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
            stdout, stderr = (
                "",
                "process timed out; output pipes closed after termination",
            )
        timed_out = True
    finished_ns = time.perf_counter_ns()
    result = {
        "argv": argv,
        "cwd": str(cwd),
        "duration_seconds": (finished_ns - started_ns) / 1_000_000_000,
        "returncode": proc.returncode,
        "timed_out": timed_out,
        "stdout_tail": tail_text(stdout, tail_chars),
        "stderr_tail": tail_text(stderr, tail_chars),
        "stdout_bytes": len((stdout or "").encode("utf-8", errors="replace")),
        "stderr_bytes": len((stderr or "").encode("utf-8", errors="replace")),
        "profile_summaries": extract_profile_summaries(stderr),
    }
    if include_timing_origin:
        result["started_perf_counter_ns"] = started_ns
        result["finished_perf_counter_ns"] = finished_ns
    if keep_stdout:
        # A workload's single-line JSON result can exceed the tail once temp
        # paths grow (phase8 nests TMPDIR several levels deep), and a
        # truncated tail is unparseable. Callers that parse it keep the full
        # text and drop this key before embedding the record in a report.
        result["stdout"] = stdout or ""
    return result


def parse_json_stdout(run: dict[str, Any]) -> Optional[dict[str, Any]]:
    text = str(run.get("stdout") or run.get("stdout_tail", "")).strip()
    if text:
        try:
            value = json.loads(text)
            if isinstance(value, dict):
                return value
        except json.JSONDecodeError:
            start = text.find("{")
            end = text.rfind("}")
            if start >= 0 and end > start:
                try:
                    value = json.loads(text[start : end + 1])
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


def workspace_target_dir(repo_root: Path) -> Path:
    """Resolve the cargo target dir; never hardcode per-crate target paths
    (stale pre-workspace target dirs shadowed fixed binaries before)."""
    proc = subprocess.run(
        ["cargo", "metadata", "--format-version=1", "--no-deps"],
        cwd=str(repo_root),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    if proc.returncode == 0:
        try:
            metadata = json.loads(proc.stdout)
            target = metadata.get("target_directory")
            if isinstance(target, str) and target:
                return Path(target)
        except json.JSONDecodeError:
            pass
    return repo_root / "target"


def resolve_vfs_bin(vfs_bin: Optional[str], repo_root: Path) -> str:
    if vfs_bin:
        candidate = Path(vfs_bin).expanduser()
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate.resolve())
        if os.sep not in vfs_bin:
            found = shutil.which(vfs_bin)
            if found:
                return found
        raise RuntimeError(
            f"configured vfs executable not found or not executable: {vfs_bin}"
        )

    target_dir = workspace_target_dir(repo_root)
    # Release first: it is what the gates measure and it is rebuilt more often
    # during active development, so it is less likely to be stale.
    for candidate in (
        target_dir / "release" / "vfs",
        target_dir / "debug" / "vfs",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)

    build = subprocess.run(
        [
            "cargo",
            "build",
            "-p",
            "vfs-cli",
            "--manifest-path",
            str(repo_root / "Cargo.toml"),
        ],
        cwd=str(repo_root),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if build.returncode != 0:
        raise RuntimeError(
            "failed to build repo-local vfs binary; set VFS_BIN explicitly\n"
            f"stdout:\n{tail_text(build.stdout)}\n"
            f"stderr:\n{tail_text(build.stderr)}"
        )
    built = target_dir / "debug" / "vfs"
    if built.is_file() and os.access(built, os.X_OK):
        return str(built)
    raise RuntimeError(f"repo-local build completed but binary was not found: {built}")


def git_commit(repo_root: Path) -> Optional[str]:
    proc = subprocess.run(
        [os.environ.get("GIT", "git"), "rev-parse", "HEAD"],
        cwd=str(repo_root),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    if proc.returncode == 0:
        return proc.stdout.strip()
    return None
