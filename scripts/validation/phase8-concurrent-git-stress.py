#!/usr/bin/env python3
"""Phase 8 concurrent Git status/diff stress gate.

The gate builds a deterministic local Git fixture, runs the same concurrent
read-mostly Git workload natively and through Vfs, and requires the hashed
status/diff/log outputs to match exactly.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sqlite3
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
    tail_text,
)


HASH_BLOCK_BYTES = 1024 * 1024


CONCURRENT_GIT_WORKLOAD = r'''
import argparse
import concurrent.futures
import hashlib
import json
import os
import subprocess
import sys
import time
from pathlib import Path


OUTPUT_TAIL_CHARS = 4000


def tail_text(value):
    text = value if isinstance(value, str) else str(value or "")
    return text if len(text) <= OUTPUT_TAIL_CHARS else text[-OUTPUT_TAIL_CHARS:]


def git_env():
    env = os.environ.copy()
    env.setdefault("GIT_CONFIG_NOSYSTEM", "1")
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    env.setdefault("NO_COLOR", "1")
    env.setdefault("LC_ALL", "C")
    env["GIT_PAGER"] = "cat"
    return env


def run_git(label, argv, cwd):
    # Honor the harness's pinned-git override: PATH shims are invisible inside
    # the vfs sandbox, so only an absolute system-path GIT avoids the
    # daemonizing hook-manager shim (scripts/validation/lib/common.py).
    git = os.environ.get("GIT", "git")
    started = time.perf_counter()
    proc = subprocess.run(
        [git] + argv,
        cwd=str(cwd),
        env=git_env(),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    stdout = proc.stdout or ""
    stderr = proc.stderr or ""
    digest = hashlib.sha256()
    digest.update(label.encode("utf-8"))
    digest.update(b"\0")
    digest.update(b"stdout\0")
    digest.update(stdout.encode("utf-8", errors="replace"))
    return {
        "label": label,
        "argv": [git] + argv,
        "returncode": proc.returncode,
        "duration_seconds": time.perf_counter() - started,
        "stdout_sha256": hashlib.sha256(stdout.encode("utf-8", errors="replace")).hexdigest(),
        "stderr_sha256": hashlib.sha256(stderr.encode("utf-8", errors="replace")).hexdigest(),
        "combined_sha256": digest.hexdigest(),
        "stdout_tail": tail_text(stdout),
        "stderr_tail": tail_text(stderr),
        "stdout_bytes": len(stdout.encode("utf-8", errors="replace")),
        "stderr_bytes": len(stderr.encode("utf-8", errors="replace")),
    }


def require_ok(record, phase):
    if record["returncode"] != 0:
        raise RuntimeError(f"{phase} failed: {record['stderr_tail']}")


def mutate_fixture(root, edit_files, append_bytes):
    ls_files = run_git("ls_files_for_mutation", ["ls-files", "-z"], root)
    require_ok(ls_files, "git ls-files")
    proc = subprocess.run(
        [os.environ.get("GIT", "git"), "ls-files", "-z"],
        cwd=str(root),
        env=git_env(),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr)
    paths = [item for item in proc.stdout.split("\0") if item]
    selected = []
    for preferred in ("src/", "tests/", "docs/"):
        for rel in paths:
            if rel.startswith(preferred) and rel not in selected:
                selected.append(rel)
                if len(selected) >= edit_files:
                    break
        if len(selected) >= edit_files:
            break
    for rel in paths:
        if len(selected) >= edit_files:
            break
        if rel not in selected:
            selected.append(rel)

    appended = []
    payload_seed = ("phase8-concurrent-git-stress\n" * ((append_bytes // 29) + 2)).encode("utf-8")
    payload = payload_seed[:append_bytes]
    for index, rel in enumerate(selected):
        path = root / rel
        with path.open("ab", buffering=0) as handle:
            handle.write(b"\n")
            handle.write(f"phase8 edit {index}: ".encode("utf-8"))
            handle.write(payload)
        appended.append({"path": rel, "bytes": len(payload) + len(f"\nphase8 edit {index}: ".encode("utf-8"))})

    untracked = root / "phase8_untracked.txt"
    untracked.write_text("untracked phase8 concurrent git stress\n", encoding="utf-8")
    return {"tracked_files": appended, "untracked": untracked.name}


def main(argv):
    parser = argparse.ArgumentParser()
    parser.add_argument("--edit-files", type=int, required=True)
    parser.add_argument("--append-bytes", type=int, required=True)
    args = parser.parse_args(argv)

    root = Path.cwd()
    mutation = mutate_fixture(root, args.edit_files, args.append_bytes)
    commands = [
        ("status_short", ["status", "--short"]),
        ("status_branch", ["status", "--short", "--branch"]),
        ("diff_patch", ["diff", "--", "."]),
        ("log_oneline", ["log", "--oneline", "-5", "--decorate=short"]),
    ]

    started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(commands)) as executor:
        futures = [executor.submit(run_git, label, command, root) for label, command in commands]
        records = [future.result() for future in futures]
    records.sort(key=lambda item: item["label"])

    digest = hashlib.sha256()
    for record in records:
        digest.update(record["label"].encode("utf-8"))
        digest.update(b"\0")
        digest.update(record["combined_sha256"].encode("ascii"))
        digest.update(b"\0")

    print(json.dumps({
        "digest": digest.hexdigest(),
        "zero_exits": all(record["returncode"] == 0 for record in records),
        "commands": records,
        "mutation": mutation,
        "total_seconds": time.perf_counter() - started,
    }, sort_keys=True))


try:
    main(sys.argv[1:])
except Exception as exc:
    print(json.dumps({"error": str(exc)}, sort_keys=True))
    raise
'''


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run concurrent git status/status/diff/log through native storage and Vfs."
    )
    parser.add_argument("--fixture-files", type=positive_int, default=48)
    parser.add_argument("--fixture-dirs", type=positive_int, default=6)
    parser.add_argument("--fixture-file-size-bytes", type=positive_int, default=1024)
    parser.add_argument("--edit-files", type=positive_int, default=4)
    parser.add_argument("--append-bytes", type=positive_int, default=128)
    parser.add_argument(
        "--vfs-bin",
        default=os.environ.get("VFS_BIN"),
        help="vfs executable path/name (default: repo target binary, building cli if needed)",
    )
    parser.add_argument(
        "--timeout",
        type=positive_float,
        default=positive_float(os.environ.get("PHASE8_CONCURRENT_GIT_TIMEOUT", "120")),
    )
    parser.add_argument("--session", default=None)
    parser.add_argument("--profile", action="store_true", default=True)
    parser.add_argument("--keep-temp", action="store_true", default=env_flag("PHASE8_KEEP_TEMP"))
    parser.add_argument("--output", help="write JSON result to this file")
    parser.add_argument("--json-indent", type=int, default=2)
    return parser.parse_args(argv)


def git_env() -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("GIT_CONFIG_NOSYSTEM", "1")
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    env.setdefault("NO_COLOR", "1")
    env.setdefault("LC_ALL", "C")
    env["GIT_AUTHOR_NAME"] = "Vfs Phase8"
    env["GIT_AUTHOR_EMAIL"] = "vfs-phase8@example.invalid"
    env["GIT_COMMITTER_NAME"] = "Vfs Phase8"
    env["GIT_COMMITTER_EMAIL"] = "vfs-phase8@example.invalid"
    return env


def run_git(argv: list[str], cwd: Path, *, env: Optional[dict[str, str]] = None, timeout: float = 60) -> subprocess.CompletedProcess[str]:
    resolved_env = env or git_env()
    return subprocess.run(
        [resolved_env.get("GIT", "git")] + argv,
        cwd=str(cwd),
        env=resolved_env,
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


def create_generated_repo(root: Path, file_count: int, dir_count: int, file_size: int) -> None:
    root.mkdir(parents=True, exist_ok=True)
    env = git_env()
    env["GIT_AUTHOR_DATE"] = "2024-01-01T00:00:00Z"
    env["GIT_COMMITTER_DATE"] = "2024-01-01T00:00:00Z"
    require_git_ok(run_git(["init"], root, env=env), "git init generated repo")
    require_git_ok(run_git(["checkout", "-B", "main"], root, env=env), "git checkout main")
    require_git_ok(run_git(["config", "user.name", "Vfs Phase8"], root, env=env), "git config user.name")
    require_git_ok(
        run_git(["config", "user.email", "vfs-phase8@example.invalid"], root, env=env),
        "git config user.email",
    )

    categories = ("src", "tests", "docs", "data")
    for index in range(file_count):
        category = categories[index % len(categories)]
        directory = root / category / f"pkg{index % dir_count:03d}"
        directory.mkdir(parents=True, exist_ok=True)
        if category == "src":
            filename = f"module_{index:05d}.py"
            header = f"# phase8 source {index}\nPHASE8_TOKEN = 'token-{index % 13}'\n"
        elif category == "tests":
            filename = f"test_{index:05d}.py"
            header = f"# phase8 test {index}\ndef test_{index:05d}():\n    assert 'PHASE8_TOKEN'\n"
        elif category == "docs":
            filename = f"note_{index:05d}.md"
            header = f"# phase8 note {index}\n\nPHASE8_TOKEN documentation fixture.\n"
        else:
            filename = f"blob_{index:05d}.txt"
            header = f"phase8 data fixture {index} PHASE8_TOKEN\n"
        seed = hashlib.sha256(f"vfs-phase8-concurrent-git-{index}".encode("utf-8")).hexdigest()
        filler = "".join(f"{line:04d} {seed} PHASE8_TOKEN_{line % 7}\n" for line in range(128))
        content = (header + filler)[:file_size]
        if not content.endswith("\n"):
            content += "\n"
        (directory / filename).write_text(content, encoding="utf-8")

    (root / ".gitignore").write_text("__pycache__/\n*.pyc\n", encoding="utf-8")
    require_git_ok(run_git(["add", "."], root, env=env), "git add generated repo")
    require_git_ok(run_git(["commit", "-m", "initial phase8 concurrent git fixture"], root, env=env), "git commit initial")

    env["GIT_AUTHOR_DATE"] = "2024-01-01T00:01:00Z"
    env["GIT_COMMITTER_DATE"] = "2024-01-01T00:01:00Z"
    touched = sorted((root / "src").rglob("*.py"))[: max(1, min(3, file_count))]
    for index, path in enumerate(touched):
        with path.open("a", encoding="utf-8") as handle:
            handle.write(f"\n# second commit marker {index} PHASE8_TOKEN\n")
    require_git_ok(run_git(["add", "."], root, env=env), "git add second commit")
    require_git_ok(run_git(["commit", "-m", "update phase8 markers"], root, env=env), "git commit second")


def prepare_environment(temp_root: Path, profile: bool) -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    env.setdefault("NO_COLOR", "1")
    env.setdefault("GIT_CONFIG_NOSYSTEM", "1")
    env.setdefault("GIT_TERMINAL_PROMPT", "0")
    if profile:
        env["VFS_PROFILE"] = "1"
    else:
        env.pop("VFS_PROFILE", None)

    home = temp_root / "home"
    for path in (home, home / ".config", home / ".cache", home / ".local" / "share"):
        path.mkdir(parents=True, exist_ok=True)
    env["HOME"] = str(home)
    env["XDG_CONFIG_HOME"] = str(home / ".config")
    env["XDG_CACHE_HOME"] = str(home / ".cache")
    env["XDG_DATA_HOME"] = str(home / ".local" / "share")
    common.pin_distro_git(env, home, home=home)

    tmp = temp_root / "tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    env["TMPDIR"] = str(tmp)
    env["TMP"] = str(tmp)
    env["TEMP"] = str(tmp)
    return env


def tree_hash(root: Path) -> dict[str, Any]:
    digest = hashlib.sha256()
    file_count = 0
    dir_count = 0
    symlink_count = 0
    total_bytes = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        filenames.sort()
        rel_dir = Path(dirpath).relative_to(root).as_posix()
        stat = Path(dirpath).lstat()
        digest.update(b"dir\0")
        digest.update(rel_dir.encode("utf-8"))
        digest.update(b"\0")
        digest.update(f"{stat.st_mode}:{stat.st_mtime_ns}:{stat.st_ctime_ns}".encode("ascii"))
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
                digest.update(target.encode("utf-8", errors="surrogateescape"))
                digest.update(b"\0")
                symlink_count += 1
                continue
            digest.update(b"file\0")
            digest.update(rel.encode("utf-8"))
            digest.update(b"\0")
            digest.update(f"{stat.st_mode}:{stat.st_size}:{stat.st_mtime_ns}".encode("ascii"))
            digest.update(b"\0")
            file_count += 1
            total_bytes += stat.st_size
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


def table_exists(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
        (name,),
    ).fetchone()
    return row is not None


def inspect_db(db_path: Path) -> dict[str, Any]:
    if not db_path.exists():
        return {"inspectable": False, "reason": "database file does not exist", "path": str(db_path)}
    try:
        conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        conn.execute("PRAGMA query_only = ON")
        try:
            result: dict[str, Any] = {"inspectable": True, "path": str(db_path)}
            for table in ("fs_inode", "fs_dentry", "fs_data", "fs_partial_origin", "fs_chunk_override"):
                if table_exists(conn, table):
                    row = conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()
                    result[f"{table}_rows"] = int(row[0])
            partial_rows = int(result.get("fs_partial_origin_rows", 0) or 0)
            result["portability_status"] = {
                "portable": partial_rows == 0,
                "partial_origin_rows": partial_rows,
            }
            return result
        finally:
            conn.close()
    except Exception as exc:
        return {"inspectable": False, "reason": str(exc), "path": str(db_path)}


def db_artifacts(db_path: Path) -> dict[str, Any]:
    wal = db_path.with_name(db_path.name + "-wal")
    if wal.exists() and wal.stat().st_size == 0:
        wal.unlink()
    shm = db_path.with_name(db_path.name + "-shm")
    if shm.exists():
        shm.unlink()

    artifacts = []
    for path in (db_path, db_path.with_name(db_path.name + "-wal"), db_path.with_name(db_path.name + "-shm")):
        artifacts.append({"path": str(path), "exists": path.exists(), "bytes": path.stat().st_size if path.exists() else 0})
    return {
        "path": str(db_path),
        "artifacts": artifacts,
        "strict_no_sidecar_files": all(
            not item["path"].endswith(("-wal", "-shm")) or not item["exists"]
            for item in artifacts
        ),
        "no_nonempty_sidecars": all(
            not item["path"].endswith(("-wal", "-shm")) or int(item["bytes"]) == 0
            for item in artifacts
        ),
    }


def run_integrity(vfs_bin: str, db_path: Path, cwd: Path, env: dict[str, str], timeout: float) -> dict[str, Any]:
    run = run_subprocess(
        [vfs_bin, "integrity", str(db_path), "--json", "--require-portable"],
        cwd,
        env,
        timeout,
    )
    payload = parse_json_stdout(run)
    return {
        "run": run,
        "result": payload,
        "ok": run["returncode"] == 0 and isinstance(payload, dict) and payload.get("ok") is True,
    }


def workload_argv(args: argparse.Namespace) -> list[str]:
    return [
        sandbox_python(),
        "-c",
        CONCURRENT_GIT_WORKLOAD,
        "--edit-files",
        str(args.edit_files),
        "--append-bytes",
        str(args.append_bytes),
    ]


def default_output_path() -> Path:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return Path(tempfile.gettempdir()) / f"vfs-phase8-concurrent-git-{stamp}-{uuid.uuid4().hex[:8]}.json"


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(__file__).resolve().parents[2]
    output_path = Path(args.output).expanduser() if args.output else default_output_path()

    temp_manager: Optional[tempfile.TemporaryDirectory[str]] = None
    if args.keep_temp:
        temp_root = Path(tempfile.mkdtemp(prefix="vfs-phase8-concurrent-git-"))
    else:
        temp_manager = tempfile.TemporaryDirectory(
            prefix="vfs-phase8-concurrent-git-",
            ignore_cleanup_errors=True,
        )
        temp_root = Path(temp_manager.name)

    exit_code = 0
    result: dict[str, Any]
    try:
        common.pin_distro_git(os.environ, temp_root)
        if shutil.which(os.environ.get("GIT", "git")) is None:
            raise RuntimeError("git executable is required")
        vfs_bin = resolve_vfs_bin(args.vfs_bin, repo_root)
        env = prepare_environment(temp_root, args.profile)
        session = args.session or f"phase8-concurrent-git-{uuid.uuid4().hex}"
        db_path = Path(env["HOME"]) / ".vfs" / "run" / session / "delta.db"

        source_root = temp_root / "source"
        native_root = temp_root / "native"
        vfs_base_root = temp_root / "vfs-base"
        create_generated_repo(
            source_root,
            args.fixture_files,
            args.fixture_dirs,
            args.fixture_file_size_bytes,
        )
        shutil.copytree(source_root, native_root, symlinks=True)
        shutil.copytree(source_root, vfs_base_root, symlinks=True)

        base_before = tree_hash(vfs_base_root)
        workload = workload_argv(args)
        native_run = run_subprocess(workload, native_root, env, args.timeout)
        vfs_run = run_subprocess(
            [vfs_bin, "run", "--session", session, "--no-default-allows", "--"] + workload,
            vfs_base_root,
            env,
            args.timeout,
        )
        base_after = tree_hash(vfs_base_root)

        native_payload = parse_json_stdout(native_run)
        vfs_payload = parse_json_stdout(vfs_run)
        digest_equal = (
            isinstance(native_payload, dict)
            and isinstance(vfs_payload, dict)
            and native_payload.get("digest") == vfs_payload.get("digest")
        )
        zero_exits = (
            native_run["returncode"] == 0
            and vfs_run["returncode"] == 0
            and isinstance(native_payload, dict)
            and isinstance(vfs_payload, dict)
            and native_payload.get("zero_exits") is True
            and vfs_payload.get("zero_exits") is True
        )
        base_unchanged = base_before["sha256"] == base_after["sha256"]
        db_after = db_artifacts(db_path)
        db_inspect = inspect_db(db_path)
        integrity = run_integrity(vfs_bin, db_path, temp_root, env, args.timeout) if db_path.exists() else {
            "run": None,
            "result": None,
            "ok": False,
        }

        passed = (
            zero_exits
            and digest_equal
            and base_unchanged
            and db_after.get("strict_no_sidecar_files") is True
            and db_inspect.get("inspectable") is True
            and db_inspect.get("portability_status", {}).get("portable") is True
            and integrity.get("ok") is True
        )
        if not passed:
            exit_code = 1

        result = {
            "schema_version": 1,
            "benchmark": "phase8-concurrent-git-stress",
            "git_commit": git_commit(repo_root),
            "command": {
                "argv": [str(Path(__file__).resolve())] + argv,
                "workload_argv": workload,
                "vfs_prefix": [
                    vfs_bin,
                    "run",
                    "--session",
                    session,
                    "--no-default-allows",
                    "--",
                ],
            },
            "parameters": {
                "fixture_files": args.fixture_files,
                "fixture_dirs": args.fixture_dirs,
                "fixture_file_size_bytes": args.fixture_file_size_bytes,
                "edit_files": args.edit_files,
                "append_bytes": args.append_bytes,
                "timeout_seconds": args.timeout,
            },
            "vfs": {
                "bin": vfs_bin,
                "session": session,
                "db_path": str(db_path),
                "profile_enabled": args.profile,
            },
            "summary": {
                "passed": passed,
                "zero_exits": zero_exits,
                "digest_equal": digest_equal,
                "native_digest": native_payload.get("digest") if isinstance(native_payload, dict) else None,
                "vfs_digest": vfs_payload.get("digest") if isinstance(vfs_payload, dict) else None,
                "base_unchanged": base_unchanged,
                "strict_no_sidecar_files": db_after.get("strict_no_sidecar_files"),
                "integrity_ok": integrity.get("ok"),
            },
            "native": {"run": native_run, "workload": native_payload},
            "vfs_overlay": {"run": vfs_run, "workload": vfs_payload},
            "base_tree": {"before": base_before, "after": base_after, "unchanged": base_unchanged},
            "database": {"after": db_after, "inspect_after": db_inspect, "integrity": integrity},
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }
    except Exception as exc:
        exit_code = 1
        result = {
            "schema_version": 1,
            "benchmark": "phase8-concurrent-git-stress",
            "error": str(exc),
            "temp_dir": str(temp_root),
            "kept_temp": bool(args.keep_temp),
            "output_path": str(output_path),
        }

    payload = json.dumps(result, indent=args.json_indent, sort_keys=True) + "\n"
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(payload, encoding="utf-8")
    sys.stdout.write(payload)
    print(f"Wrote Phase 8 concurrent git stress JSON to {output_path}", file=sys.stderr)

    if temp_manager is not None:
        temp_manager.cleanup()
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
