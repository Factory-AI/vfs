"""Process, environment, and binary-resolution helpers shared by the
validation harnesses.

Import via a sys.path bootstrap so the dash-named top-level scripts can use
the package from any CWD:

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from lib.common import resolve_agentfs_bin, run_subprocess
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import subprocess
import tempfile
import time
from collections.abc import MutableMapping
from pathlib import Path
from typing import Any, Optional

OUTPUT_TAIL_CHARS = 8000


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


def pin_distro_git(
    env: MutableMapping[str, str], scratch_dir: Path, home: Optional[Path] = None
) -> Path:
    """Pin `git` to the distro binary for every subprocess of a harness.

    The user PATH may route `git` through a hook-manager shim that daemonizes
    out of harness temp repos (library/environment.md). Prepend a shim dir
    whose `git` symlinks the distro binary, and point the global git config at
    a hookless file so no hook manager can re-enter via config.

    The PATH shim only holds for host-side processes: inside an `agentfs run`
    sandbox, temp dirs and home files are hidden, so any shim dir silently
    falls out of PATH and the hook-manager git takes over. That is what
    ``env["GIT"]`` is for — it names the distro binary by absolute (system,
    sandbox-visible) path, and every harness git invocation must honor it,
    including the workload scripts spawned inside `agentfs run`. Pass ``home``
    to also write a hookless ``~/.gitconfig`` into a temp HOME (belt and
    braces for host-side legs).
    """
    shim_dir = scratch_dir / "git-shim"
    shim_dir.mkdir(parents=True, exist_ok=True)
    real_git = next(
        (candidate for candidate in ("/usr/bin/git", "/bin/git") if os.access(candidate, os.X_OK)),
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
            "agentfs_session": False,
        }
        try:
            environ = (entry / "environ").read_bytes()
        except OSError:
            environ = b""
        for chunk in environ.split(b"\0"):
            if chunk.startswith(b"HOME="):
                info["home"] = chunk[len(b"HOME="):].decode("utf-8", "replace")
            elif chunk.startswith(b"AGENTFS_SESSION="):
                info["agentfs_session"] = True
        procs[int(entry.name)] = info
    return procs


def git_ai_leaks(
    before: dict[int, dict[str, Any]], after: dict[int, dict[str, Any]]
) -> list[dict[str, Any]]:
    """New git-ai processes attributable to the harness run.

    The user's own hook manager churns independently (its daemon respawns with
    HOME under the real home dir), so a bare pid diff false-positives. Count
    only new processes with a temp-dir HOME or an AGENTFS session in their
    environment — the same discrimination rule library/environment.md requires
    before killing one.
    """
    tmp_prefix = tempfile.gettempdir().rstrip(os.sep) + os.sep
    leaks: list[dict[str, Any]] = []
    for pid, info in sorted(after.items()):
        if pid in before:
            continue
        home = info.get("home") or ""
        if info.get("agentfs_session") or home.startswith(tmp_prefix):
            leaks.append({"pid": pid, **info})
    return leaks


def tail_text(value: Any) -> str:
    text = value.decode("utf-8", errors="replace") if isinstance(value, bytes) else str(value or "")
    return text if len(text) <= OUTPUT_TAIL_CHARS else text[-OUTPUT_TAIL_CHARS:]


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
            stdout, stderr = "", "process timed out; output pipes closed after termination"
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
    }


def parse_json_stdout(run: dict[str, Any]) -> Optional[dict[str, Any]]:
    text = str(run.get("stdout_tail", "")).strip()
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


def resolve_agentfs_bin(agentfs_bin: Optional[str], repo_root: Path) -> str:
    if agentfs_bin:
        candidate = Path(agentfs_bin).expanduser()
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate.resolve())
        if os.sep not in agentfs_bin:
            found = shutil.which(agentfs_bin)
            if found:
                return found
        raise RuntimeError(f"configured agentfs executable not found or not executable: {agentfs_bin}")

    target_dir = workspace_target_dir(repo_root)
    # Release first: it is what the gates measure and it is rebuilt more often
    # during active development, so it is less likely to be stale.
    for candidate in (
        target_dir / "release" / "agentfs",
        target_dir / "debug" / "agentfs",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)

    build = subprocess.run(
        ["cargo", "build", "-p", "agentfs-cli", "--manifest-path", str(repo_root / "Cargo.toml")],
        cwd=str(repo_root),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if build.returncode != 0:
        raise RuntimeError(
            "failed to build repo-local agentfs binary; set AGENTFS_BIN explicitly\n"
            f"stdout:\n{tail_text(build.stdout)}\n"
            f"stderr:\n{tail_text(build.stderr)}"
        )
    built = target_dir / "debug" / "agentfs"
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
